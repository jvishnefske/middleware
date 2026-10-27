//! Request dispatch and payload lifecycle: the firmware's decision logic.
//!
//! [`handle_request`] takes the current [`State`] and one request and
//! returns an [`Outcome`]: the next state, the reply (body length written
//! into the caller's buffer, or a [`HandlerError`]), and an [`Action`] the
//! shell must perform. It does no I/O. The shell performs the action and, if
//! the action fails, applies [`State::storage_failed`] or
//! [`State::trapped`] and replies `Internal` instead.
//!
//! | request              | condition            | action     | status after |
//! |----------------------|----------------------|------------|--------------|
//! | `sys/info`           | —                    | none       | unchanged    |
//! | `sys/loader/begin`   | fits                 | `Erase`    | `Loading`    |
//! | `sys/loader/chunk`   | in order, fits       | `Write`    | `Loading`    |
//! | `sys/loader/chunk`   | resend of last chunk | none (`Ok`) | `Loading`   |
//! | `sys/loader/commit`  | complete, CRC ok     | `Activate` | `Stopped`    |
//! | any loader failure   | transfer abandoned   | none       | `Stopped` if an image is installed, else `Empty` |
//! | `sys/payload/start`  | `Stopped`/`Trapped`  | `Start`    | `Running` once the shell calls [`State::started`] |
//! | `sys/payload/start`  | `Empty`/`Loading`    | none (`InvalidRequest`) | unchanged |
//! | `sys/payload/start`  | built without runtime | none (`NotFound`) | unchanged |
//! | `sys/payload/stop`   | `Running`/`Trapped`  | `Stop`     | `Stopped`    |
//! | other `sys/...`      | —                    | none (`NotFound`) | unchanged |
//! | application key      | —                    | none (`NotFound`) | unchanged |
//!
//! The store writes a transfer into the slot that is not active (see
//! [`crate::store`]), so an abandoned transfer or a failed storage action
//! leaves the installed image in place. [`State`] remembers whether one is
//! installed (from boot, and after each successful activate) and returns to
//! `Stopped` rather than `Empty` when a transfer fails.
//!
//! Incoming publications go to the payload only when it is running, the key
//! is not a system key, and the payload subscribed to exactly that key
//! ([`route_publication`]; keys have no wildcards).

use middleware::Key;
use middleware::loader::{Command, DeviceInfo, Effect, Loader, PayloadStatus, is_system_key};
use middleware::transport::HandlerError;
use middleware::wasm_abi::ABI_VERSION;

/// Fixed facts reported by `sys/info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Context<'a> {
    /// Chip name, e.g. `esp32c3`.
    pub chip: &'a str,
    /// Firmware version.
    pub firmware: &'a str,
    /// Largest payload image in bytes.
    pub capacity: u32,
    /// Whether the firmware includes the WASM runtime. Without it the loader
    /// and storage work but `sys/payload/start` is refused with `NotFound`.
    pub runtime: bool,
}

/// Dispatcher state: loader progress and payload status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    loader: Loader,
    status: PayloadStatus,
    /// A valid image is installed (it may not be running).
    installed: bool,
}

impl State {
    /// State after boot: `Stopped` if a valid image is installed, else
    /// `Empty`.
    pub const fn boot(installed: bool) -> Self {
        let status = Self::resting(installed);
        State { loader: Loader::Idle, status, installed }
    }

    /// Status when no transfer is in progress and nothing runs.
    const fn resting(installed: bool) -> PayloadStatus {
        if installed { PayloadStatus::Stopped } else { PayloadStatus::Empty }
    }

    /// Current payload status.
    pub const fn status(&self) -> PayloadStatus {
        self.status
    }

    /// Current loader progress.
    pub const fn loader(&self) -> Loader {
        self.loader
    }

    /// Whether the payload may be started (an image is installed).
    pub const fn startable(&self) -> bool {
        matches!(self.status, PayloadStatus::Stopped | PayloadStatus::Trapped | PayloadStatus::Running)
    }

    /// The shell loaded and started the payload.
    pub const fn started(self) -> Self {
        State { status: PayloadStatus::Running, ..self }
    }

    /// The payload trapped, ran out of fuel, or failed to load.
    pub const fn trapped(self) -> Self {
        State { status: PayloadStatus::Trapped, ..self }
    }

    /// A storage action failed. Storage actions only touch the inactive
    /// slot, so the transfer ends and the status returns to `Stopped` if an
    /// image is installed, else `Empty`.
    pub const fn storage_failed(self) -> Self {
        State { loader: Loader::Idle, status: Self::resting(self.installed), ..self }
    }
}

/// Side effect the shell performs before sending the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action<'a> {
    /// Nothing.
    None,
    /// Stop the payload, then erase the inactive slot for an image of `len`
    /// bytes.
    Erase {
        /// Image length.
        len: u32,
    },
    /// Write image bytes at `offset`.
    Write {
        /// Byte offset in the image.
        offset: u32,
        /// Bytes to write.
        data: &'a [u8],
    },
    /// Flush pending bytes, check the CRC and write the slot header: the
    /// image is installed and its slot becomes active.
    Activate {
        /// Image length.
        len: u32,
        /// Image CRC-32.
        crc: u32,
    },
    /// Load the installed image and run it.
    Start,
    /// Stop the payload.
    Stop,
}

impl Action<'_> {
    /// Whether the payload must not run after this action.
    pub const fn stops_payload(&self) -> bool {
        matches!(self, Action::Erase { .. } | Action::Stop)
    }
}

/// Result of [`handle_request`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome<'a> {
    /// Next state, assuming the action succeeds.
    pub state: State,
    /// Reply body length in the caller's buffer, or the error to send.
    pub reply: Result<usize, HandlerError>,
    /// Effect for the shell.
    pub action: Action<'a>,
}

impl<'a> Outcome<'a> {
    const fn reply(state: State, reply: Result<usize, HandlerError>) -> Self {
        Outcome { state, reply, action: Action::None }
    }

    const fn act(state: State, action: Action<'a>) -> Self {
        Outcome { state, reply: Ok(0), action }
    }
}

/// Dispatches one request. The response body (for `sys/info`) is written
/// into `out`.
pub fn handle_request<'a>(
    state: State,
    ctx: &Context<'_>,
    key: Key<'_>,
    body: &'a [u8],
    out: &mut [u8],
) -> Outcome<'a> {
    let cmd = match Command::parse(key, body) {
        Err(e) => return Outcome::reply(state, Err(e)),
        Ok(None) => return Outcome::reply(state, Err(HandlerError::NotFound)),
        Ok(Some(cmd)) => cmd,
    };
    match cmd {
        Command::Info => {
            let info = DeviceInfo {
                abi_version: ABI_VERSION,
                capacity: ctx.capacity,
                status: state.status,
                chip: ctx.chip,
                firmware: ctx.firmware,
            };
            Outcome::reply(state, info.encode(out).ok_or(HandlerError::BufferTooSmall))
        }
        Command::Start if !ctx.runtime => Outcome::reply(state, Err(HandlerError::NotFound)),
        Command::Start => match state.status {
            PayloadStatus::Empty | PayloadStatus::Loading => {
                Outcome::reply(state, Err(HandlerError::InvalidRequest))
            }
            PayloadStatus::Running => Outcome::reply(state, Ok(0)),
            PayloadStatus::Stopped | PayloadStatus::Trapped => Outcome::act(state, Action::Start),
        },
        Command::Stop => match state.status {
            PayloadStatus::Running | PayloadStatus::Trapped => {
                Outcome::act(State { status: PayloadStatus::Stopped, ..state }, Action::Stop)
            }
            _ => Outcome::reply(state, Ok(0)),
        },
        Command::Begin { .. } | Command::Chunk { .. } | Command::Commit => {
            transfer(state, &cmd, ctx.capacity)
        }
    }
}

fn transfer<'a>(state: State, cmd: &Command<'a>, capacity: u32) -> Outcome<'a> {
    let (loader, result) = state.loader.step(cmd, capacity);
    match result {
        Err(e) => {
            // A failure mid-transfer abandons the partly written inactive
            // slot; the installed image, if any, is intact. A rejected
            // `begin` from idle touched nothing.
            let status = match state.status {
                PayloadStatus::Loading => State::resting(state.installed),
                other => other,
            };
            Outcome::reply(State { loader, status, ..state }, Err(e))
        }
        Ok(Effect::Erase { len }) => {
            Outcome::act(State { loader, status: PayloadStatus::Loading, ..state }, Action::Erase { len })
        }
        Ok(Effect::Write { offset, data }) => {
            Outcome::act(State { loader, ..state }, Action::Write { offset, data })
        }
        Ok(Effect::Activate { len, crc }) => Outcome::act(
            State { loader, status: PayloadStatus::Stopped, installed: true },
            Action::Activate { len, crc },
        ),
        Ok(Effect::None) => Outcome::reply(State { loader, ..state }, Ok(0)),
    }
}

/// Whether a publication from the host on `key` goes to the payload.
pub fn route_publication<'s>(
    state: &State,
    key: Key<'_>,
    mut subscriptions: impl Iterator<Item = &'s str>,
) -> bool {
    state.status == PayloadStatus::Running
        && !is_system_key(key)
        && subscriptions.any(|s| s == key.as_str())
}

/// Milliseconds from `now` until `deadline` on a wrapping millisecond
/// clock; zero when the deadline has passed (by less than half the range).
pub const fn tick_delay(now: u32, deadline: u32) -> u32 {
    let d = deadline.wrapping_sub(now) as i32;
    if d > 0 { d as u32 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::crc::Crc32;
    use middleware::loader::keys;

    const CTX: Context<'static> =
        Context { chip: "esp32c3", firmware: "0.1.0", capacity: 1024, runtime: true };

    fn req<'a>(state: State, key: Key<'_>, body: &'a [u8]) -> Outcome<'a> {
        let mut out = [0u8; 128];
        handle_request(state, &CTX, key, body, &mut out)
    }

    fn begin(len: u32, crc: u32) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&len.to_le_bytes());
        b[4..].copy_from_slice(&crc.to_le_bytes());
        b
    }

    fn chunk(offset: u32, data: &[u8]) -> ([u8; 64], usize) {
        let mut b = [0u8; 64];
        b[..4].copy_from_slice(&offset.to_le_bytes());
        b[4..4 + data.len()].copy_from_slice(data);
        (b, 4 + data.len())
    }

    #[test]
    fn info_reports_device() {
        let mut out = [0u8; 128];
        let o = handle_request(State::boot(true), &CTX, keys::INFO, &[], &mut out);
        let n = o.reply.unwrap();
        assert_eq!(o.action, Action::None);
        let info = DeviceInfo::decode(&out[..n]).unwrap();
        assert_eq!(info.abi_version, ABI_VERSION);
        assert_eq!(info.capacity, 1024);
        assert_eq!(info.status, PayloadStatus::Stopped);
        assert_eq!(info.chip, "esp32c3");
        assert_eq!(info.firmware, "0.1.0");
        let mut tiny = [0u8; 4];
        let o = handle_request(State::boot(true), &CTX, keys::INFO, &[], &mut tiny);
        assert_eq!(o.reply, Err(HandlerError::BufferTooSmall));
    }

    #[test]
    fn full_transfer_installs_then_start_and_stop() {
        let image = [0xA5u8; 40];
        let crc = Crc32::checksum(&image);
        let s = State::boot(false);
        let b = begin(40, crc);
        let o = req(s, keys::BEGIN, &b);
        assert_eq!((o.action, o.reply), (Action::Erase { len: 40 }, Ok(0)));
        assert_eq!(o.state.status(), PayloadStatus::Loading);
        assert!(o.action.stops_payload());
        let (c, n) = chunk(0, &image[..30]);
        let o = req(o.state, keys::CHUNK, &c[..n]);
        assert_eq!(o.action, Action::Write { offset: 0, data: &image[..30] });
        let (c, n) = chunk(30, &image[30..]);
        let o = req(o.state, keys::CHUNK, &c[..n]);
        assert_eq!(o.action, Action::Write { offset: 30, data: &image[30..] });
        let o = req(o.state, keys::COMMIT, &[]);
        assert_eq!(o.action, Action::Activate { len: 40, crc });
        assert_eq!(o.state.status(), PayloadStatus::Stopped);

        let o = req(o.state, keys::START, &[]);
        assert_eq!(o.action, Action::Start);
        let running = o.state.started();
        assert_eq!(running.status(), PayloadStatus::Running);
        let o = req(running, keys::START, &[]);
        assert_eq!((o.action, o.reply), (Action::None, Ok(0)));
        let o = req(running, keys::STOP, &[]);
        assert_eq!(o.action, Action::Stop);
        assert_eq!(o.state.status(), PayloadStatus::Stopped);
        let o = req(o.state, keys::STOP, &[]);
        assert_eq!(o.action, Action::None);
    }

    #[test]
    fn resent_last_chunk_is_acknowledged_without_rewriting() {
        let image = [1u8, 2, 3, 4, 5, 6];
        let b = begin(6, Crc32::checksum(&image));
        let o = req(State::boot(false), keys::BEGIN, &b);
        let (c, n) = chunk(0, &image[..4]);
        let after_first = req(o.state, keys::CHUNK, &c[..n]).state;
        let again = req(after_first, keys::CHUNK, &c[..n]);
        assert_eq!((again.action, again.reply), (Action::None, Ok(0)));
        assert_eq!(again.state, after_first);
        let (c, n) = chunk(4, &image[4..]);
        let o = req(again.state, keys::CHUNK, &c[..n]);
        let o = req(o.state, keys::COMMIT, &[]);
        assert!(matches!(o.action, Action::Activate { len: 6, .. }));
    }

    #[test]
    fn begin_while_running_stops_and_erases() {
        let b = begin(10, 0);
        let o = req(State::boot(true).started(), keys::BEGIN, &b);
        assert_eq!(o.action, Action::Erase { len: 10 });
        assert_eq!(o.state.status(), PayloadStatus::Loading);
    }

    #[test]
    fn abandoned_transfer_keeps_installed_payload() {
        for (installed, after) in [(true, PayloadStatus::Stopped), (false, PayloadStatus::Empty)] {
            let b = begin(8, 0);
            let o = req(State::boot(installed).started(), keys::BEGIN, &b);
            let (c, n) = chunk(4, &[1]);
            let o = req(o.state, keys::CHUNK, &c[..n]);
            assert_eq!(o.reply, Err(HandlerError::InvalidRequest));
            assert_eq!(o.state.status(), after);
            assert_eq!(o.state.loader(), Loader::Idle);
            assert_eq!(o.state.startable(), installed);

            // Wrong CRC on commit.
            let b = begin(4, 0);
            let o = req(State::boot(installed), keys::BEGIN, &b);
            let (c, n) = chunk(0, &[1, 2, 3, 4]);
            let o = req(o.state, keys::CHUNK, &c[..n]);
            let o = req(o.state, keys::COMMIT, &[]);
            assert_eq!(o.reply, Err(HandlerError::InvalidRequest));
            assert_eq!(o.state.status(), after);
        }
    }

    #[test]
    fn failed_transfer_after_first_install_keeps_it() {
        let image = [3u8; 4];
        let b = begin(4, Crc32::checksum(&image));
        let o = req(State::boot(false), keys::BEGIN, &b);
        let (c, n) = chunk(0, &image);
        let o = req(o.state, keys::CHUNK, &c[..n]);
        let installed = req(o.state, keys::COMMIT, &[]).state;
        let b = begin(4, 0);
        let o = req(installed, keys::BEGIN, &b);
        let o = req(o.state, keys::COMMIT, &[]);
        assert_eq!(o.reply, Err(HandlerError::InvalidRequest));
        assert_eq!(o.state.status(), PayloadStatus::Stopped);
        assert_eq!(req(o.state, keys::START, &[]).action, Action::Start);
    }

    #[test]
    fn oversized_begin_from_idle_keeps_installed_payload() {
        let b = begin(CTX.capacity + 1, 0);
        let o = req(State::boot(true), keys::BEGIN, &b);
        assert_eq!(o.reply, Err(HandlerError::BufferTooSmall));
        assert_eq!(o.action, Action::None);
        assert_eq!(o.state.status(), PayloadStatus::Stopped);
    }

    #[test]
    fn start_requires_an_installed_image() {
        let o = req(State::boot(false), keys::START, &[]);
        assert_eq!((o.action, o.reply), (Action::None, Err(HandlerError::InvalidRequest)));
        let loading = req(State::boot(true), keys::BEGIN, &begin(4, 0)).state;
        assert_eq!(req(loading, keys::START, &[]).reply, Err(HandlerError::InvalidRequest));
        let trapped = State::boot(true).started().trapped();
        assert_eq!(req(trapped, keys::START, &[]).action, Action::Start);
        assert_eq!(req(trapped, keys::STOP, &[]).state.status(), PayloadStatus::Stopped);
    }

    #[test]
    fn start_without_runtime_is_refused_but_loader_works() {
        let ctx = Context { runtime: false, ..CTX };
        let mut out = [0u8; 128];
        let o = handle_request(State::boot(true), &ctx, keys::START, &[], &mut out);
        assert_eq!((o.action, o.reply), (Action::None, Err(HandlerError::NotFound)));
        let b = begin(4, 0);
        let o = handle_request(State::boot(true), &ctx, keys::BEGIN, &b, &mut out);
        assert_eq!(o.action, Action::Erase { len: 4 });
    }

    #[test]
    fn unknown_and_application_keys_are_not_found() {
        let s = State::boot(true);
        assert_eq!(req(s, Key::from_static("sys/nope"), &[]).reply, Err(HandlerError::NotFound));
        assert_eq!(req(s, Key::from_static("app/ping"), b"x").reply, Err(HandlerError::NotFound));
        assert_eq!(req(s, keys::INFO, b"x").reply, Err(HandlerError::InvalidRequest));
        assert_eq!(req(s, keys::CHUNK, &[]).reply, Err(HandlerError::InvalidRequest));
    }

    #[test]
    fn storage_failure_and_traps_update_status() {
        let s = State::boot(true).started();
        assert_eq!(s.trapped().status(), PayloadStatus::Trapped);
        let f = s.storage_failed();
        assert_eq!((f.status(), f.loader()), (PayloadStatus::Stopped, Loader::Idle));
        assert!(f.startable());
        let loading = req(State::boot(false), keys::BEGIN, &begin(4, 0)).state;
        let f = loading.storage_failed();
        assert_eq!((f.status(), f.loader()), (PayloadStatus::Empty, Loader::Idle));
        assert!(!f.startable());
        assert!(State::boot(true).startable());
    }

    #[test]
    fn publications_route_only_to_running_subscribed_payloads() {
        let subs = ["sensor/temp", "cmd/led"];
        let k = Key::from_static("cmd/led");
        let running = State::boot(true).started();
        assert!(route_publication(&running, k, subs.iter().copied()));
        assert!(!route_publication(&running, Key::from_static("cmd/other"), subs.iter().copied()));
        assert!(!route_publication(&running, Key::from_static("cmd"), subs.iter().copied()));
        assert!(!route_publication(&State::boot(true), k, subs.iter().copied()));
        let sys = ["sys/log"];
        assert!(!route_publication(&running, keys::LOG, sys.iter().copied()));
    }

    #[test]
    fn tick_delay_handles_wrap() {
        assert_eq!(tick_delay(100, 150), 50);
        assert_eq!(tick_delay(150, 100), 0);
        assert_eq!(tick_delay(100, 100), 0);
        assert_eq!(tick_delay(u32::MAX - 5, 4), 10);
        assert_eq!(tick_delay(4, u32::MAX - 5), 0);
    }
}
