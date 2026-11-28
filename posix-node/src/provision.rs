//! Claiming a node over SSH: a key proposed over the network, accepted on
//! the machine.
//!
//! A node is claimed (its key pinned, `sys/trust/claim`) only over its local
//! serial port: physical access. A host without one (a PC on the LAN) is
//! claimed in two steps instead:
//!
//! 1. **Propose**, over the network: `sys/trust/propose` with the public key
//!    as hex text (what `mwctl key pub` prints), e.g.
//!    `mwctl --zenoh <endpoint> --node <id> request sys/trust/propose <hex>`.
//!    An unclaimed node keeps it as the *pending* key ([`PENDING_FILE`] in
//!    its state directory, replacing an earlier one) and answers its
//!    [`fingerprint`]. A pending key pins nothing and authorises nothing.
//!    A claimed node refuses (`Denied`).
//! 2. **Accept**, on the machine, as root (over SSH):
//!    `mw-posix-node trust accept <fingerprint>` ([`cli`]). It writes the
//!    fingerprint to [`ACCEPT_FILE`]; the running node, which looks for it
//!    once a second while a key is pending, pins the pending key through
//!    its local claim path when the fingerprints match, and removes both
//!    files. A mismatch claims nothing (the accept file is removed and the
//!    pending key kept).
//!
//! Root on the machine stands for physical access here, as the serial port
//! does: only someone who can write the node's state directory turns a
//! proposed key into the pinned one, and only the key whose fingerprint
//! they confirmed. `mw-posix-node trust pending` shows what was proposed.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use middleware::loader::trust::PublicKey;
use middleware::transport::HandlerError;
use sha2::{Digest, Sha256};

/// The key a public key is proposed on.
pub const PROPOSE: &str = "sys/trust/propose";
/// The pending key's file in the state directory: the key's wire bytes.
pub const PENDING_FILE: &str = "trust-pending";
/// The accept file: the fingerprint the operator confirmed.
pub const ACCEPT_FILE: &str = "trust-accept";
/// Hex digits of a fingerprint.
pub const FINGERPRINT_LEN: usize = 16;
/// How often a node with a pending key looks for an accept.
pub const ACCEPT_POLL: Duration = Duration::from_secs(1);

/// The key's fingerprint: the first 8 bytes of SHA-256 over its wire bytes,
/// as 16 lowercase hex digits.
pub fn fingerprint(key: &PublicKey) -> String {
    hex(&Sha256::digest(key.as_bytes())[..FINGERPRINT_LEN / 2])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Bytes from hex text (surrounding whitespace ignored).
fn unhex(text: &[u8]) -> Option<Vec<u8>> {
    let t = std::str::from_utf8(text).ok()?.trim();
    if t.len() % 2 != 0 {
        return None;
    }
    (0..t.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(t.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Writes `data` to `path` through a temporary file (fsynced), with `mode`.
fn write_atomic(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// The node's side: the pending key and the accept, in `dir`.
#[derive(Debug)]
pub struct Provision {
    dir: PathBuf,
    next_look: Option<Instant>,
}

impl Provision {
    /// The provisioning files in the state directory `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Provision {
            dir: dir.into(),
            next_look: None,
        }
    }

    fn pending_path(&self) -> PathBuf {
        self.dir.join(PENDING_FILE)
    }

    fn accept_path(&self) -> PathBuf {
        self.dir.join(ACCEPT_FILE)
    }

    /// The pending key, if one was proposed (and is a valid key).
    pub fn pending(&self) -> Option<PublicKey> {
        read_pending(&self.dir)
    }

    /// `sys/trust/propose`: keeps `body` (the key as hex) as the pending key
    /// unless the node is `claimed`; answers the fingerprint (ASCII) in
    /// `out`.
    pub fn propose(
        &mut self,
        body: &[u8],
        claimed: bool,
        out: &mut [u8],
    ) -> Result<usize, HandlerError> {
        if claimed {
            return Err(HandlerError::Denied);
        }
        let bytes = unhex(body).ok_or(HandlerError::InvalidRequest)?;
        let key = PublicKey::parse_claim(&bytes)?;
        write_atomic(&self.pending_path(), key.as_bytes(), 0o600)
            .map_err(|_| HandlerError::Internal)?;
        self.next_look = Some(Instant::now());
        let fp = fingerprint(&key);
        let dst = out
            .get_mut(..fp.len())
            .ok_or(HandlerError::BufferTooSmall)?;
        dst.copy_from_slice(fp.as_bytes());
        Ok(fp.len())
    }

    /// When the node should look for an accept next: while a key is pending
    /// (or one may have been proposed before this start), every
    /// [`ACCEPT_POLL`]; `None` otherwise.
    pub fn next_look(&mut self) -> Option<Instant> {
        if self.next_look.is_none() && self.pending_path().exists() {
            self.next_look = Some(Instant::now());
        }
        self.next_look
    }

    /// Looks for an accept when one is due: the pending key to pin when the
    /// operator confirmed its fingerprint (both files are then removed), or
    /// `None`. A mismatched or unreadable accept is removed and logged
    /// through `log`.
    pub fn take_accepted(&mut self, log: &mut dyn FnMut(&str)) -> Option<PublicKey> {
        let due = self.next_look?;
        let now = Instant::now();
        if now < due {
            return None;
        }
        let Some(key) = self.pending() else {
            self.next_look = None; // nothing pending (any more)
            let _ = std::fs::remove_file(self.accept_path());
            return None;
        };
        self.next_look = Some(now + ACCEPT_POLL);
        let accept = match std::fs::read(self.accept_path()) {
            Ok(b) => b,
            Err(_) => return None, // not accepted yet
        };
        let _ = std::fs::remove_file(self.accept_path());
        let want = fingerprint(&key);
        if String::from_utf8_lossy(&accept).trim() != want {
            log(&format!(
                "trust accept refused: the fingerprint does not match the pending key {want}"
            ));
            return None;
        }
        let _ = std::fs::remove_file(self.pending_path());
        self.next_look = None;
        Some(key)
    }
}

fn read_pending(dir: &Path) -> Option<PublicKey> {
    let b = std::fs::read(dir.join(PENDING_FILE)).ok()?;
    PublicKey::parse_claim(&b).ok()
}

/// `mw-posix-node trust pending|accept <fingerprint>`, run as root on the
/// machine (over SSH), against the state directory `dir`. Returns the exit
/// code and prints to stdout/stderr.
pub fn cli(dir: &Path, args: &[String]) -> std::process::ExitCode {
    use std::process::ExitCode;
    let usage = "usage: mw-posix-node trust pending | trust accept <fingerprint>";
    match args {
        [cmd] if cmd == "pending" => match read_pending(dir) {
            Some(k) => {
                println!("pending key {}", hex(k.as_bytes()));
                println!("fingerprint {}", fingerprint(&k));
                ExitCode::SUCCESS
            }
            None => {
                eprintln!("no key pending in {}", dir.display());
                ExitCode::FAILURE
            }
        },
        [cmd, fp] if cmd == "accept" => {
            let Some(k) = read_pending(dir) else {
                eprintln!(
                    "no key pending in {}: propose one first (sys/trust/propose)",
                    dir.display()
                );
                return ExitCode::FAILURE;
            };
            let want = fingerprint(&k);
            if fp.trim().to_ascii_lowercase() != want {
                eprintln!("the pending key's fingerprint is {want}, not {fp}: nothing accepted");
                return ExitCode::FAILURE;
            }
            if let Err(e) = write_atomic(&dir.join(ACCEPT_FILE), want.as_bytes(), 0o644) {
                eprintln!("{}: {e}", dir.join(ACCEPT_FILE).display());
                return ExitCode::FAILURE;
            }
            // The node takes it within a poll and removes both files.
            let deadline = Instant::now() + 10 * ACCEPT_POLL;
            while Instant::now() < deadline {
                if !dir.join(PENDING_FILE).exists() && !dir.join(ACCEPT_FILE).exists() {
                    println!(
                        "claimed: key {} (fingerprint {want}) pinned",
                        hex(k.as_bytes())
                    );
                    return ExitCode::SUCCESS;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            eprintln!(
                "accepted {want}, but the node has not taken it (is mw-posix-node running?); see its log"
            );
            ExitCode::FAILURE
        }
        _ => {
            eprintln!("{usage}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_HEX: &str = "a8866ea55f0b88cf59331422fd253e91db379e3089647af8a20c73c64d48b050";

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mw-provision-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn key() -> PublicKey {
        PublicKey::parse_claim(&unhex(KEY_HEX.as_bytes()).unwrap()).unwrap()
    }

    #[test]
    fn a_proposed_key_is_pending_and_answers_its_fingerprint() {
        let dir = scratch("propose");
        let mut p = Provision::new(&dir);
        let mut out = [0u8; 32];
        let n = p
            .propose(format!(" {KEY_HEX}\n").as_bytes(), false, &mut out)
            .unwrap();
        assert_eq!(std::str::from_utf8(&out[..n]).unwrap(), fingerprint(&key()));
        assert_eq!(p.pending(), Some(key()));
        assert!(p.next_look().is_some());
        // Nothing pinned yet: no accept.
        assert_eq!(p.take_accepted(&mut |_| {}), None);
        assert_eq!(p.pending(), Some(key()), "still pending");
    }

    #[test]
    fn a_claimed_node_or_a_bad_key_takes_no_proposal() {
        let dir = scratch("refuse");
        let mut p = Provision::new(&dir);
        let mut out = [0u8; 32];
        assert_eq!(
            p.propose(KEY_HEX.as_bytes(), true, &mut out),
            Err(HandlerError::Denied)
        );
        for bad in ["", "zz", "a886", &KEY_HEX[..63]] {
            assert_eq!(
                p.propose(bad.as_bytes(), false, &mut out),
                Err(HandlerError::InvalidRequest),
                "{bad:?}"
            );
        }
        assert_eq!(p.pending(), None);
        assert_eq!(p.next_look(), None);
    }

    #[test]
    fn the_matching_accept_hands_over_the_key_and_clears_both_files() {
        let dir = scratch("accept");
        let mut p = Provision::new(&dir);
        p.propose(KEY_HEX.as_bytes(), false, &mut [0u8; 32])
            .unwrap();
        // A wrong fingerprint: refused, logged, the key still pending.
        std::fs::write(dir.join(ACCEPT_FILE), "0123456789abcdef").unwrap();
        let mut logged = Vec::new();
        assert_eq!(p.take_accepted(&mut |m| logged.push(m.to_owned())), None);
        assert_eq!(logged.len(), 1);
        assert!(!dir.join(ACCEPT_FILE).exists());
        assert_eq!(p.pending(), Some(key()));
        // The right one, once the next look is due.
        std::fs::write(dir.join(ACCEPT_FILE), fingerprint(&key())).unwrap();
        p.next_look = Some(Instant::now());
        assert_eq!(p.take_accepted(&mut |_| {}), Some(key()));
        assert!(!dir.join(ACCEPT_FILE).exists() && !dir.join(PENDING_FILE).exists());
        assert_eq!(p.next_look(), None);
    }

    #[test]
    fn a_key_proposed_before_a_restart_is_still_looked_for() {
        let dir = scratch("restart");
        Provision::new(&dir)
            .propose(KEY_HEX.as_bytes(), false, &mut [0u8; 32])
            .unwrap();
        let mut p = Provision::new(&dir);
        assert!(p.next_look().is_some());
    }

    #[test]
    fn the_cli_refuses_a_fingerprint_that_is_not_the_pending_one() {
        let dir = scratch("cli");
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            cli(&dir, &args(&["accept", "0123456789abcdef"])),
            std::process::ExitCode::FAILURE,
            "nothing pending"
        );
        Provision::new(&dir)
            .propose(KEY_HEX.as_bytes(), false, &mut [0u8; 32])
            .unwrap();
        assert_eq!(
            cli(&dir, &args(&["accept", "0123456789abcdef"])),
            std::process::ExitCode::FAILURE
        );
        assert!(!dir.join(ACCEPT_FILE).exists(), "nothing written");
    }
}
