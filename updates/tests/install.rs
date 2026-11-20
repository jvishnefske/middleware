//! UP6: a payload install survives a commit resent after it took (the
//! link timed out on a slow store and sent it again; the resend meets an
//! idle loader and is refused). Against a real posix-node over its local
//! link.

use std::time::{Duration, Instant, SystemTime};

use middleware::Key;
use middleware::loader::PayloadStatus;
use middleware::loader::keys;
use middleware::loader::release::CertKind;
use middleware::loader::trust::{TrustState, Version};
use mw_posix_node::service::CHIP;
use mw_updates::publish::{prepare, sign};
use mw_updates::updater::install_payload;
use mwctl::device::Device;
use mwctl::error::Error;
use mwctl::link::{Endpoint, Event, Link, Timeouts};
use mwctl::sign::Seed;

mod common;

/// Sends every `sys/loader/commit` twice and answers with the second
/// reply, as a link does when the first reply is late.
struct ResendCommit<E>(E);

impl<E: Endpoint> Endpoint for ResendCommit<E> {
    fn request(&mut self, key: Key<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
        if key == keys::COMMIT {
            let first = self.0.request(key, body);
            assert!(first.is_ok(), "the first commit takes: {first:?}");
        }
        self.0.request(key, body)
    }

    fn take_events(&mut self) -> Vec<Event> {
        self.0.take_events()
    }

    fn next_event(&mut self, deadline: Instant) -> Result<Option<Event>, Error> {
        self.0.next_event(deadline)
    }

    fn chunk_size(&self) -> usize {
        self.0.chunk_size()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn up6_a_commit_resent_after_it_took_still_installs_and_starts() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("updates-resend-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (_pi, chan) = common::node(&dir, middleware::SenderId([4; 8])).await;
    let root = Seed::new([5; 32]);
    let img = common::demo();
    let p = prepare(CertKind::Payload, &img, Some(Version(3)), SystemTime::now()).unwrap();
    let release = sign(CertKind::Payload, "test", CHIP, &p, &root).unwrap();
    tokio::task::block_in_place(|| {
        let mut plain = Device::new(Link::new(chan.clone(), Timeouts::default()));
        plain.trust_claim(root.public_key()).unwrap();
        let mut d = Device::new(ResendCommit(Link::new(chan.clone(), Timeouts::default())));
        install_payload(&mut d, &release, &p.image).expect("installed despite the resend");
        let t = plain.trust_info().unwrap();
        assert!(matches!(
            t.state,
            TrustState::Claimed {
                min_version: Version(3),
                ..
            }
        ));
        // Give the start a moment to land.
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(plain.info().unwrap().status, PayloadStatus::Running);
    });
}
