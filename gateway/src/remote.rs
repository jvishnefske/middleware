//! The remote update client behind the `mw-update` binary: signed payload
//! uploads and RP2350 firmware updates through a gateway's `gw/sys`.
//!
//! It reuses `mwctl`'s protocol operations unchanged: `mwctl::device::Device`
//! runs `upload` (begin, chunks, signed commit, signed start) and `fw_update`
//! (info, begin, chunks, signed commit, reboot) over any
//! `mwctl::link::Endpoint`, and [`GwEndpoint`] is one: each device request
//! becomes one `gw/sys` call (`[key_len][key][body]`) through a [`SysCall`].
//! What is decided here is pure and unit tested:
//!
//! * [`cli`] — the command line.
//! * [`RequestIds`] — one `x-request-id` per device request
//!   (`upd-<session>-<n>`); every attempt of a request carries the same
//!   id, so the gateway answers a resend from its replay cache instead of
//!   acting twice ([`crate::update::replay`]).
//! * [`retryable`] — a request is resent (same id, at most `attempts`
//!   times, with a pause) when the answer is `DeadlineExceeded` or
//!   `Unavailable` (the gateway busy, the first copy still at the device,
//!   the gateway restarting) or the transport failed; any other answer is
//!   final.
//! * [`timeout_for`] — the per-attempt deadline, sent as `grpc-timeout`:
//!   the base (`--timeout`), at least 5 s for a `begin`, a loader commit, a
//!   start and a firmware confirm, at least 15 s for a firmware commit (the
//!   device hashes the image), as `mwctl` waits on the serial port.
//! * [`job`] and [`execute`] — the inputs a command needs (files read
//!   through an injected reader, a token key opened through an injected
//!   opener, signatures made with the current time as version and counter
//!   by default) and the device operations run for it. The signing key is
//!   any `mwctl::sign::Signer`: an Ed25519 key file (`--key`) or a key in a
//!   PKCS#11 token (`--pkcs11`, P-256 in a TPM), whichever kind the device
//!   pinned.
//!
//! Every request goes out signed: the gateway forwards only signed commits
//! and signed start/stop requests ([`crate::update`]). The control target is
//! the device's node id as `sys/info` reports it (first six bytes).
//!
//! The I/O shell is [`mesh::MeshSysCall`] (a `MeshClient` through the
//! static endpoint, on its own current-thread runtime) and the binary
//! `src/bin/mw-update.rs`.
//!
//! After `fw update` reboots the device, the gateway's serial stream to
//! it may close (a USB CDC device re-enumerates), and the gateway with it
//! until its supervisor restarts it: the confirm that follows is retried
//! ([`CONFIRM_ATTEMPTS`]) while the gateway or the device's self-test is
//! not ready.

pub mod cli;
pub mod mesh;

use std::path::Path;
use std::time::{Duration, SystemTime};

use middleware::fw_ota::wire::keys as fw;
use middleware::key::Key;
use middleware::loader::keys;
use middleware::loader::trust::{ControlCounter, ControlOp, Version};
use middleware::transport::HandlerError;
use mwctl::device::{Device, UploadOptions};
use mwctl::error::Error;
use mwctl::fw::{FwImage, UpdateOptions, load_image, parse_version};
use mwctl::link::{Endpoint, Event};
use mwctl::report::{format_fw_info, format_info, format_trust};
use mwctl::sign::{Seed, Signer, counter_at, sign_commit, sign_control, version_at};
use std::time::Instant;

use crate::bridge::encode_sys_request;
use crate::update::control_target;
use cli::{Cmd, KeySpec};

/// How often `fw update` tries the confirm after the reboot.
pub const CONFIRM_ATTEMPTS: u32 = 30;
/// The pause between resends (and confirm attempts).
pub const RETRY_PAUSE: Duration = Duration::from_millis(500);

/// Why one `gw/sys` call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The gateway (or the device behind it) answered with this status.
    Status(HandlerError),
    /// The call did not complete (connection refused, reset, ...).
    Transport(String),
}

/// One `gw/sys` call: the shell's side of [`GwEndpoint`].
pub trait SysCall {
    /// Sends the `gw/sys` request `body` with the `x-request-id`
    /// `request_id`, waiting at most `timeout`; the answer's body.
    fn call(
        &mut self,
        body: &[u8],
        request_id: &str,
        timeout: Duration,
    ) -> Result<Vec<u8>, CallError>;

    /// Waits before a resend.
    fn pause(&mut self);
}

/// Whether an attempt that failed with `e` is resent (module docs).
pub fn retryable(e: &CallError) -> bool {
    match e {
        CallError::Status(s) => {
            matches!(
                s,
                HandlerError::DeadlineExceeded | HandlerError::Unavailable
            )
        }
        CallError::Transport(_) => true,
    }
}

/// The per-attempt deadline of a request on `key` (module docs).
pub fn timeout_for(key: Key<'_>, base: Duration) -> Duration {
    let floor = if key == fw::COMMIT {
        Duration::from_secs(15)
    } else if [
        keys::BEGIN,
        keys::COMMIT,
        keys::START,
        fw::BEGIN,
        fw::CONFIRM,
    ]
    .contains(&key)
    {
        Duration::from_secs(5)
    } else {
        Duration::ZERO
    };
    base.max(floor)
}

/// Request ids: `upd-<session hex>-<n>`, `n` counting from 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestIds {
    session: u64,
    next: u64,
}

impl RequestIds {
    /// Ids of the session `session` (the shell takes it from the clock and
    /// process id, so two runs do not share ids).
    pub fn new(session: u64) -> Self {
        RequestIds { session, next: 1 }
    }

    /// The next id and the generator after it.
    pub fn take(self) -> (String, Self) {
        let id = format!("upd-{:016x}-{}", self.session, self.next);
        (
            id,
            RequestIds {
                next: self.next.saturating_add(1),
                ..self
            },
        )
    }
}

/// An `mwctl` endpoint over `gw/sys` (module docs).
#[derive(Debug)]
pub struct GwEndpoint<C> {
    call: C,
    ids: RequestIds,
    base: Duration,
    attempts: u32,
}

impl<C: SysCall> GwEndpoint<C> {
    /// Requests through `call`, ids from `ids`, `base` the per-attempt
    /// deadline and `attempts` (at least 1) per request.
    pub fn new(call: C, ids: RequestIds, base: Duration, attempts: u32) -> Self {
        GwEndpoint {
            call,
            ids,
            base,
            attempts: attempts.max(1),
        }
    }

    /// The call.
    pub fn call(&self) -> &C {
        &self.call
    }
}

/// The `mwctl` error for a final failure of a request on `key` after
/// `attempts`.
pub fn final_error(key: Key<'_>, e: CallError, attempts: u32) -> Error {
    let key = key.as_str().to_owned();
    match e {
        CallError::Status(HandlerError::DeadlineExceeded) => Error::Timeout { key, attempts },
        CallError::Status(status) => Error::Device { key, status },
        // A gateway that cannot be reached is unavailable (gRPC
        // UNAVAILABLE), which callers such as `fw_confirm` retry.
        CallError::Transport(_) => Error::Device {
            key,
            status: HandlerError::Unavailable,
        },
    }
}

impl<C: SysCall> Endpoint for GwEndpoint<C> {
    fn request(&mut self, key: Key<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
        let mut gw = vec![0u8; 1 + key.as_bytes().len() + body.len()];
        let n = encode_sys_request(key, body, &mut gw).ok_or(Error::Frame(
            middleware::serial::frame::FrameError::TooLarge,
        ))?;
        let ids = std::mem::replace(&mut self.ids, RequestIds::new(0));
        let (id, ids) = ids.take();
        self.ids = ids;
        let timeout = timeout_for(key, self.base);
        let mut attempt = 1;
        loop {
            match self
                .call
                .call(gw.get(..n).unwrap_or_default(), &id, timeout)
            {
                Ok(answer) => return Ok(answer),
                Err(e) if retryable(&e) && attempt < self.attempts => {
                    attempt += 1;
                    self.call.pause();
                }
                Err(e) => return Err(final_error(key, e, attempt)),
            }
        }
    }

    fn take_events(&mut self) -> Vec<Event> {
        Vec::new()
    }

    /// `gw/sys` carries no publications.
    fn next_event(&mut self, _deadline: Instant) -> Result<Option<Event>, Error> {
        Ok(None)
    }

    fn chunk_size(&self) -> usize {
        middleware::loader::CHUNK_MAX
    }
}

/// What a command needs, loaded (not `Debug`: it holds a signing key).
pub enum Job {
    /// `info`.
    Info,
    /// `upload`.
    Upload {
        /// The image.
        image: Vec<u8>,
        /// The signing key.
        signer: Box<dyn Signer + Send>,
        /// The signed version.
        version: Version,
        /// The counter of the start to send after it, if any.
        start: Option<ControlCounter>,
    },
    /// `start` / `stop`.
    Control {
        /// Which.
        op: ControlOp,
        /// The signing key.
        signer: Box<dyn Signer + Send>,
        /// The counter.
        counter: ControlCounter,
    },
    /// `fw info`.
    FwInfo,
    /// `fw update`.
    FwUpdate {
        /// The checked image.
        image: FwImage,
        /// The signing key.
        signer: Box<dyn Signer + Send>,
        /// Reboot and confirm.
        reboot: bool,
    },
    /// `fw reboot`.
    FwReboot,
    /// `fw confirm`.
    FwConfirm,
    /// `fw abort`.
    FwAbort,
}

/// Loads what `cmd` needs, reading files with `read`, opening a token key
/// by its label with `open_token`, and taking default versions and
/// counters from `now`. Pure apart from `read` and `open_token`.
pub fn job(
    cmd: &Cmd,
    read: impl Fn(&Path) -> std::io::Result<Vec<u8>>,
    open_token: impl Fn(&str) -> Result<Box<dyn Signer + Send>, Error>,
    now: SystemTime,
) -> Result<Job, Error> {
    let signer = |k: &KeySpec| -> Result<Box<dyn Signer + Send>, Error> {
        match k {
            KeySpec::File(p) => {
                let bytes = read(p).map_err(Error::File)?;
                Ok(Box::new(Seed::from_file_bytes(&bytes)?))
            }
            KeySpec::Pkcs11(label) => open_token(label),
        }
    };
    let counter = |c: Option<u64>| -> Result<ControlCounter, Error> {
        match c {
            Some(c) => Ok(ControlCounter(c)),
            None => Ok(counter_at(now)?),
        }
    };
    Ok(match cmd {
        Cmd::Info => Job::Info,
        Cmd::FwInfo => Job::FwInfo,
        Cmd::FwReboot => Job::FwReboot,
        Cmd::FwConfirm => Job::FwConfirm,
        Cmd::FwAbort => Job::FwAbort,
        Cmd::Upload {
            file,
            key,
            version,
            start,
            counter: c,
        } => Job::Upload {
            image: read(file).map_err(Error::File)?,
            signer: signer(key)?,
            version: match version {
                Some(v) => Version(*v),
                None => version_at(now)?,
            },
            start: if *start { Some(counter(*c)?) } else { None },
        },
        Cmd::Control {
            start,
            key,
            counter: c,
        } => Job::Control {
            op: if *start {
                ControlOp::Start
            } else {
                ControlOp::Stop
            },
            signer: signer(key)?,
            counter: counter(*c)?,
        },
        Cmd::FwUpdate {
            file,
            key,
            version,
            reboot,
        } => {
            let expected = match version {
                Some(v) => Some(
                    parse_version(v)
                        .ok_or_else(|| Error::Fw(format!("--version {v}: not MAJOR.MINOR")))?,
                ),
                None => None,
            };
            Job::FwUpdate {
                image: load_image(&read(file).map_err(Error::File)?, expected)?,
                signer: signer(key)?,
                reboot: *reboot,
            }
        }
    })
}

/// The control target of the device behind `device`: its node id from
/// `sys/info`.
fn target<E: Endpoint>(device: &mut Device<E>) -> Result<[u8; 6], Error> {
    let info = device.info()?;
    info.node.map(control_target).ok_or(Error::Malformed {
        key: keys::INFO.as_str().to_owned(),
    })
}

/// Runs `job` against `device`; returns what to print. `pause` waits
/// between confirm attempts after a firmware reboot.
pub fn execute<E: Endpoint>(
    job: Job,
    device: &mut Device<E>,
    mut pause: impl FnMut(),
) -> Result<String, Error> {
    match job {
        Job::Info => {
            let info = device.info()?;
            let trust = device.trust_info()?;
            Ok(format!("{}\n{}", format_info(&info), format_trust(&trust)))
        }
        Job::Upload {
            image,
            signer,
            version,
            start,
        } => {
            let auth = sign_commit(&image, &*signer, version)?;
            let start_auth = match start {
                Some(counter) => {
                    let t = target(device)?;
                    Some(sign_control(&*signer, ControlOp::Start, &t, counter)?)
                }
                None => None,
            };
            let opts = UploadOptions {
                start: start.is_some(),
                corrupt_chunk: None,
                auth: Some(auth),
                start_auth,
            };
            device.upload(&image, opts, |_| {})?;
            Ok(format!(
                "installed {} bytes as version {}{}",
                image.len(),
                version.0,
                if start.is_some() { ", started" } else { "" }
            ))
        }
        Job::Control {
            op,
            signer,
            counter,
        } => {
            let t = target(device)?;
            let auth = Some(sign_control(&*signer, op, &t, counter)?);
            match op {
                ControlOp::Start => device.start_signed(auth)?,
                ControlOp::Stop => device.stop_signed(auth)?,
                // `job` builds start and stop only.
                _ => return Err(Error::Fw(format!("{op:?} is not a payload control"))),
            }
            Ok(format!(
                "{} (counter {})",
                if op == ControlOp::Start {
                    "started"
                } else {
                    "stopped"
                },
                counter.0
            ))
        }
        Job::FwInfo => Ok(format_fw_info(&device.fw_info()?)),
        Job::FwUpdate {
            image,
            signer,
            reboot,
        } => {
            // Over gw/sys the reboot travels as the serial form (unsigned):
            // the gateway is the device's serial side.
            let opts = UpdateOptions {
                release: None,
                key: Some(&*signer),
                reboot,
                control: None,
            };
            let done = device.fw_update(&image, opts, |_| {})?;
            if reboot {
                device.fw_confirm(CONFIRM_ATTEMPTS, &mut pause)?;
                Ok(format!(
                    "firmware {} installed in slot {}, rebooted and confirmed",
                    mwctl::fw::format_version(image.version),
                    done.slot.name()
                ))
            } else {
                Ok(format!(
                    "firmware {} committed to slot {}; run `fw reboot`, then `fw confirm`",
                    mwctl::fw::format_version(image.version),
                    done.slot.name()
                ))
            }
        }
        Job::FwReboot => device.fw_reboot().map(|()| "rebooting".to_owned()),
        Job::FwConfirm => device
            .fw_confirm(CONFIRM_ATTEMPTS, &mut pause)
            .map(|()| "confirmed".to_owned()),
        Job::FwAbort => device.fw_abort().map(|()| "aborted".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::parse_sys_request;

    /// Answers from a script; records (key, request id, timeout).
    struct Scripted {
        answers: Vec<Result<Vec<u8>, CallError>>,
        seen: Vec<(String, String, Duration)>,
        pauses: u32,
    }

    impl SysCall for Scripted {
        fn call(&mut self, body: &[u8], id: &str, timeout: Duration) -> Result<Vec<u8>, CallError> {
            let (key, _) = parse_sys_request(body).expect("gw/sys body");
            self.seen
                .push((key.as_str().to_owned(), id.to_owned(), timeout));
            if self.answers.is_empty() {
                return Err(CallError::Status(HandlerError::NotFound));
            }
            self.answers.remove(0)
        }

        fn pause(&mut self) {
            self.pauses += 1;
        }
    }

    fn ep(answers: Vec<Result<Vec<u8>, CallError>>, attempts: u32) -> GwEndpoint<Scripted> {
        GwEndpoint::new(
            Scripted {
                answers,
                seen: Vec::new(),
                pauses: 0,
            },
            RequestIds::new(0xabc),
            Duration::from_millis(2000),
            attempts,
        )
    }

    #[test]
    fn rg21_resends_carry_the_same_request_id() {
        let mut e = ep(
            vec![
                Err(CallError::Status(HandlerError::DeadlineExceeded)),
                Err(CallError::Transport("reset".into())),
                Ok(b"ok".to_vec()),
                Ok(Vec::new()),
            ],
            3,
        );
        assert_eq!(e.request(keys::BEGIN, b"x").unwrap(), b"ok");
        assert_eq!(e.request(keys::CHUNK, b"y").unwrap(), b"");
        let seen = &e.call().seen;
        assert_eq!(seen.len(), 4);
        assert!(seen[..3].iter().all(|(k, id, t)| k == "sys/loader/begin"
            && id == "upd-0000000000000abc-1"
            && *t == Duration::from_secs(5)));
        assert_eq!(seen[3].1, "upd-0000000000000abc-2");
        assert_eq!(seen[3].2, Duration::from_millis(2000));
        assert_eq!(e.call().pauses, 2);
    }

    #[test]
    fn rg21_final_answers_and_exhausted_attempts() {
        // A refusal is final at once.
        let mut e = ep(vec![Err(CallError::Status(HandlerError::Denied))], 3);
        let err = e.request(keys::COMMIT, b"").unwrap_err();
        assert_eq!(err.status(), Some(HandlerError::Denied));
        assert_eq!(e.call().seen.len(), 1);
        // Timeouts until the attempts run out: a timeout error.
        let late = || Err(CallError::Status(HandlerError::DeadlineExceeded));
        let mut e = ep(vec![late(), late()], 2);
        assert!(matches!(
            e.request(keys::CHUNK, b""),
            Err(Error::Timeout { attempts: 2, .. })
        ));
        // An unreachable gateway is Unavailable.
        let mut e = ep(vec![Err(CallError::Transport("refused".into()))], 1);
        assert_eq!(
            e.request(keys::INFO, b"").unwrap_err().status(),
            Some(HandlerError::Unavailable)
        );
        assert!(retryable(&CallError::Status(HandlerError::Unavailable)));
        assert!(!retryable(&CallError::Status(HandlerError::InvalidRequest)));
        assert_eq!(
            timeout_for(fw::COMMIT, Duration::from_secs(1)),
            Duration::from_secs(15)
        );
        assert_eq!(
            timeout_for(fw::CONFIRM, Duration::from_secs(1)),
            Duration::from_secs(5)
        );
        assert_eq!(
            timeout_for(fw::CHUNK, Duration::from_secs(1)),
            Duration::from_secs(1)
        );
        assert_eq!(
            timeout_for(keys::START, Duration::from_secs(9)),
            Duration::from_secs(9)
        );
        assert_eq!(e.chunk_size(), middleware::loader::CHUNK_MAX);
        assert!(e.take_events().is_empty());
    }

    fn no_token(label: &str) -> Result<Box<dyn Signer + Send>, Error> {
        Err(Error::Fw(format!("no token key {label}")))
    }

    #[test]
    fn rg24_jobs_load_their_inputs() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_millis(1_761_500_000_123);
        let read = |p: &Path| match p.to_str() {
            Some("k") => Ok(vec![7u8; 32]),
            Some("p.wasm") => Ok(b"\0asm".to_vec()),
            _ => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        };
        let up = Cmd::Upload {
            file: "p.wasm".into(),
            key: "k".into(),
            version: None,
            start: true,
            counter: None,
        };
        let Job::Upload {
            image,
            version,
            start,
            ..
        } = job(&up, read, no_token, now).unwrap()
        else {
            panic!()
        };
        assert_eq!(image, b"\0asm");
        assert_eq!(version, Version(1_761_500_000));
        assert_eq!(start, Some(ControlCounter(1_761_500_000_123)));
        let stop = Cmd::Control {
            start: false,
            key: "k".into(),
            counter: Some(5),
        };
        assert!(matches!(
            job(&stop, read, no_token, now).unwrap(),
            Job::Control {
                op: ControlOp::Stop,
                counter: ControlCounter(5),
                ..
            }
        ));
        let missing = Cmd::Control {
            start: true,
            key: "nope".into(),
            counter: None,
        };
        assert!(matches!(
            job(&missing, read, no_token, now),
            Err(Error::File(_))
        ));
        let bad_fw = Cmd::FwUpdate {
            file: "p.wasm".into(),
            key: "k".into(),
            version: None,
            reboot: true,
        };
        assert!(matches!(
            job(&bad_fw, read, no_token, now),
            Err(Error::Image(_))
        ));
        assert!(matches!(
            job(&Cmd::FwAbort, read, no_token, now).unwrap(),
            Job::FwAbort
        ));
        // A token key goes through the opener, by its label; the key file
        // is Ed25519, the token key here P-256.
        let p256 = |label: &str| -> Result<Box<dyn Signer + Send>, Error> {
            assert_eq!(label, "mw-release");
            Ok(Box::new(
                mwctl::sign::P256Key::from_scalar(&[0x20; 32]).expect("scalar"),
            ))
        };
        let token_stop = Cmd::Control {
            start: false,
            key: KeySpec::Pkcs11("mw-release".into()),
            counter: Some(5),
        };
        let Job::Control { signer, .. } = job(&token_stop, read, p256, now).unwrap() else {
            panic!()
        };
        assert_eq!(
            signer.public_key().kind(),
            middleware::loader::trust::KeyKind::P256
        );
        let Job::Control { signer, .. } = job(&stop, read, p256, now).unwrap() else {
            panic!()
        };
        assert_eq!(
            signer.public_key().kind(),
            middleware::loader::trust::KeyKind::Ed25519
        );
        assert!(matches!(
            job(&token_stop, read, no_token, now),
            Err(Error::Fw(_))
        ));
    }
}
