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
//!    its state directory) and answers its [`fingerprint`]. A pending key
//!    pins nothing and authorises nothing. While one is pending, a proposal
//!    of another key is refused (`Unavailable`; the same key is answered
//!    again), so the pending key cannot be swapped under the operator; root
//!    drops it with `trust clear`. A claimed node refuses (`Denied`).
//! 2. **Accept**, on the machine, as root (over SSH):
//!    `mw-posix-node trust accept <fingerprint>` ([`cli`]). The fingerprint
//!    is for the operator's eyes; the accept file ([`ACCEPT_FILE`]) carries
//!    the whole key the operator confirmed. The running node, which looks
//!    for it once a second while a key is pending, pins the pending key
//!    through its local claim path only when it is that same key, byte for
//!    byte, and removes both files. Anything else claims nothing (the
//!    accept file is removed and the pending key kept).
//!
//! Root on the machine stands for physical access here, as the serial port
//! does: only someone who can write the node's state directory turns a
//! proposed key into the pinned one, and only the key they confirmed.
//!
//! The state directory belongs to the node's service user, so the root CLI
//! never follows a link there: it reads the pending key with `O_NOFOLLOW`
//! (a regular file of a key's size, nothing else) and writes the accept
//! file through a new, exclusively created temporary file (`O_EXCL`), renamed
//! into place.

use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use middleware::loader::trust::PublicKey;
use middleware::transport::HandlerError;
use sha2::{Digest, Sha256};

/// The key a public key is proposed on.
pub const PROPOSE: &str = "sys/trust/propose";
/// The pending key's file in the state directory: the key's wire bytes.
pub const PENDING_FILE: &str = "trust-pending";
/// The accept file: the confirmed key's wire bytes.
pub const ACCEPT_FILE: &str = "trust-accept";
/// Hex digits of a fingerprint.
pub const FINGERPRINT_LEN: usize = 16;
/// How often a node with a pending key looks for an accept.
pub const ACCEPT_POLL: Duration = Duration::from_secs(1);
/// The most a key file may hold (a P-256 key, 33 bytes).
const KEY_FILE_MAX: u64 = 33;

/// The key's fingerprint, for the operator to compare: the first 8 bytes of
/// SHA-256 over its wire bytes, as 16 lowercase hex digits. (The accept
/// binds the whole key, not this.)
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

/// Equal bytes, compared in time that depends only on the lengths.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// Writes `data` to `path` with `mode`: into a new temporary file beside it,
/// created exclusively (`O_EXCL`: an existing name or a link there fails,
/// nothing is followed), fsynced, then renamed over `path` (a link at `path`
/// is replaced, not followed).
fn write_atomic(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let name = path
        .file_name()
        .map_or_else(Default::default, |n| n.to_string_lossy());
    let tmp = path.with_file_name(format!(".{name}.{}.{nanos}.tmp", std::process::id()));
    let written = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// A key file's bytes: a regular file (never through a link) of at most a
/// key's size.
fn read_key_file(path: &Path) -> Option<Vec<u8>> {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let meta = f.metadata().ok()?;
    if !meta.is_file() || meta.len() > KEY_FILE_MAX {
        return None;
    }
    let mut b = Vec::new();
    Read::take(&mut f, KEY_FILE_MAX + 1)
        .read_to_end(&mut b)
        .ok()?;
    Some(b)
}

fn read_pending(dir: &Path) -> Option<PublicKey> {
    PublicKey::parse_claim(&read_key_file(&dir.join(PENDING_FILE))?).ok()
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
    /// unless the node is `claimed` (`Denied`) or another key is pending
    /// (`Unavailable`); answers the fingerprint (ASCII) in `out`.
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
        match self.pending() {
            Some(p) if same(p.as_bytes(), key.as_bytes()) => {} // proposed again
            Some(_) => return Err(HandlerError::Unavailable),
            None => write_atomic(&self.pending_path(), key.as_bytes(), 0o600)
                .map_err(|_| HandlerError::Internal)?,
        }
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
    /// accept names exactly that key (both files are then removed), or
    /// `None`. Any other accept is removed and logged through `log`.
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
        if !self.accept_path().exists() {
            return None; // not accepted yet
        }
        let accept = read_key_file(&self.accept_path());
        let _ = std::fs::remove_file(self.accept_path());
        if !accept.is_some_and(|a| same(&a, key.as_bytes())) {
            log(&format!(
                "trust accept refused: it is not the pending key {}",
                fingerprint(&key)
            ));
            return None;
        }
        let _ = std::fs::remove_file(self.pending_path());
        self.next_look = None;
        Some(key)
    }
}

/// `mw-posix-node trust pending | accept <fingerprint> | clear`, run as root
/// on the machine (over SSH), against the state directory `dir`. Returns the
/// exit code and prints to stdout/stderr.
pub fn cli(dir: &Path, args: &[String]) -> std::process::ExitCode {
    use std::process::ExitCode;
    let usage = "usage: mw-posix-node trust pending | trust accept <fingerprint> | trust clear";
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
        [cmd] if cmd == "clear" => {
            for f in [PENDING_FILE, ACCEPT_FILE] {
                match std::fs::remove_file(dir.join(f)) {
                    Ok(()) => println!("removed {f}"),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => {
                        eprintln!("{}: {e}", dir.join(f).display());
                        return ExitCode::FAILURE;
                    }
                }
            }
            ExitCode::SUCCESS
        }
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
            // The whole key the operator confirmed: the node pins only that.
            if let Err(e) = write_atomic(&dir.join(ACCEPT_FILE), k.as_bytes(), 0o644) {
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
        // Another key (or a fingerprint, the old format): refused, logged,
        // the key still pending.
        std::fs::write(dir.join(ACCEPT_FILE), [7u8; 32]).unwrap();
        let mut logged = Vec::new();
        assert_eq!(p.take_accepted(&mut |m| logged.push(m.to_owned())), None);
        assert_eq!(logged.len(), 1);
        assert!(!dir.join(ACCEPT_FILE).exists());
        assert_eq!(p.pending(), Some(key()));
        std::fs::write(dir.join(ACCEPT_FILE), fingerprint(&key())).unwrap();
        p.next_look = Some(Instant::now());
        assert_eq!(p.take_accepted(&mut |m| logged.push(m.to_owned())), None);
        assert_eq!(logged.len(), 2);
        // The whole key, once the next look is due.
        std::fs::write(dir.join(ACCEPT_FILE), key().as_bytes()).unwrap();
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

    #[test]
    fn a_pending_key_is_not_replaced_by_another_proposal() {
        let dir = scratch("swap");
        let mut p = Provision::new(&dir);
        let mut out = [0u8; 32];
        p.propose(KEY_HEX.as_bytes(), false, &mut out).unwrap();
        let other = "11".repeat(32);
        assert_eq!(
            p.propose(other.as_bytes(), false, &mut out),
            Err(HandlerError::Unavailable)
        );
        assert_eq!(p.pending(), Some(key()), "still the first key");
        // The same key again is answered.
        assert!(p.propose(KEY_HEX.as_bytes(), false, &mut out).is_ok());
        // Root clears it; then another key may be proposed.
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            cli(&dir, &args(&["clear"])),
            std::process::ExitCode::SUCCESS
        );
        assert!(p.propose(other.as_bytes(), false, &mut out).is_ok());
    }

    #[test]
    fn key_files_are_never_read_through_a_link() {
        let dir = scratch("link");
        let secret = dir.join("secret");
        std::fs::write(&secret, [9u8; 32]).unwrap(); // a key-sized file
        std::os::unix::fs::symlink(&secret, dir.join(PENDING_FILE)).unwrap();
        assert_eq!(read_pending(&dir), None, "not followed");
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            cli(&dir, &args(&["pending"])),
            std::process::ExitCode::FAILURE
        );
        // Nor anything larger than a key.
        std::fs::remove_file(dir.join(PENDING_FILE)).unwrap();
        std::fs::write(dir.join(PENDING_FILE), [1u8; 64]).unwrap();
        assert_eq!(read_pending(&dir), None);
    }

    #[test]
    fn a_link_where_a_key_file_goes_is_replaced_not_written_through() {
        let dir = scratch("write-link");
        let target = dir.join("target");
        std::fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, dir.join(ACCEPT_FILE)).unwrap();
        write_atomic(&dir.join(ACCEPT_FILE), &[5u8; 32], 0o644).unwrap();
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"keep",
            "the target is untouched"
        );
        let meta = std::fs::symlink_metadata(dir.join(ACCEPT_FILE)).unwrap();
        assert!(
            meta.file_type().is_file(),
            "the link was replaced by the file"
        );
    }
}
