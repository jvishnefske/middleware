//! `mwctl` against the RT1050 service (RT12, as RP V9): the host tool's
//! own `Device` operations, unchanged, over an in-process byte stream.
//!
//! [`Wire`] is what the shell's LPUART link and controller do with the
//! byte stream, minus the UART: it cuts COBS frames with middleware's
//! `Splitter`, decodes requests with `frame::decode_wire`, hands them to
//! the real [`Service`] (over the RAM HyperFlash model), answers a start
//! the way the controller does once a payload runs, and publishes status
//! changes on `sys/payload/status` and notes on `sys/log`, all as wire
//! frames.

extern crate std;

use std::collections::VecDeque;
use std::io;
use std::time::Duration;
use std::vec::Vec as StdVec;

use embedded_storage::nor_flash::NorFlash;
use middleware::loader::trust::Version;
use middleware::loader::{PayloadStatus, keys};
use middleware::serial::frame::{self, Frame, MAX_RAW_FRAME, MAX_WIRE_FRAME, Split, Splitter};
use mwctl::device::{Device, UploadOptions};
use mwctl::link::{Link, Timeouts, Transport};
use mwctl::sign::{Seed, sign_commit};

use super::tests::{FACTORY, FACTS, MAP, NODE, flash, image};
use super::{Facts, Reply, Service};

/// The device end of the byte stream.
struct Wire<F> {
    svc: Service<F>,
    splitter: Splitter<MAX_WIRE_FRAME>,
    tx: VecDeque<u8>,
}

impl<F: NorFlash> Wire<F> {
    fn new(flash: F, facts: Facts) -> Self {
        let (mut svc, _) = Service::open(flash, &MAP, facts, FACTORY).unwrap();
        svc.set_link_ready(true);
        Wire {
            svc,
            splitter: Splitter::new(),
            tx: VecDeque::new(),
        }
    }

    fn emit(&mut self, f: &Frame<'_>) {
        let mut scratch = [0u8; MAX_RAW_FRAME];
        let mut out = [0u8; MAX_WIRE_FRAME];
        let n = frame::encode(f, &mut scratch, &mut out).expect("encodable");
        self.tx.extend(&out[..n]);
    }

    fn on_frame(&mut self, run: &mut [u8]) {
        let Ok(Frame::Request { id, key, payload }) = frame::decode_wire(run) else {
            return;
        };
        let before = self.svc.status();
        let mut out = [0u8; 128];
        let h = self.svc.handle(key, payload, &mut out);
        for note in &h.notes {
            let line = std::format!("{note}");
            self.emit(&Frame::Publish {
                key: keys::LOG,
                payload: line.as_bytes(),
            });
        }
        let result = match h.reply {
            Reply::Now(r) => r.map(|n| out[..n].to_vec()),
            Reply::Start => {
                // The controller loads the image, then answers.
                self.svc.started();
                Ok(StdVec::new())
            }
        };
        if self.svc.status() != before {
            let status = [self.svc.status() as u8];
            self.emit(&Frame::Publish {
                key: keys::STATUS,
                payload: &status,
            });
        }
        let body = result.as_deref().map_err(|e| *e);
        self.emit(&Frame::Response { id, result: body });
    }
}

impl<F: NorFlash> Transport for Wire<F> {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        for &b in bytes {
            if let Split::Frame(_) = self.splitter.push(b) {
                let mut run = self.splitter.frame().to_vec();
                self.on_frame(&mut run);
            }
        }
        Ok(())
    }

    fn read(&mut self, buf: &mut [u8], _timeout: Duration) -> io::Result<usize> {
        let n = buf.len().min(self.tx.len());
        for (dst, src) in buf.iter_mut().zip(self.tx.drain(..n)) {
            *dst = src;
        }
        Ok(n)
    }
}

fn device<F: NorFlash>(wire: Wire<F>) -> Device<Link<Wire<F>>> {
    let t = Timeouts {
        request: Duration::from_millis(50),
        begin: Duration::from_millis(50),
        retries: 1,
    };
    Device::new(Link::new(wire, t))
}

#[test]
fn rt12_mwctl_info_upload_start_and_trust_claim_work_unchanged() {
    let mut f = flash();
    let mut d = device(Wire::new(&mut f, FACTS));
    let info = d.info().unwrap();
    assert_eq!(
        (info.chip.as_str(), info.firmware.as_str()),
        ("rt1050", "0.1.0")
    );
    assert_eq!(
        (info.capacity, info.status, info.node),
        (64 * 1024, PayloadStatus::Empty, Some(NODE))
    );
    assert!(info.runs_payloads());
    let img = image(5000, 11);
    let mut steps = 0;
    d.upload(
        &img,
        UploadOptions {
            start: true,
            ..UploadOptions::default()
        },
        |_| steps += 1,
    )
    .unwrap();
    assert!(steps > 2);
    assert_eq!(d.info().unwrap().status, PayloadStatus::Running);
    d.stop().unwrap();
    assert_eq!(d.info().unwrap().status, PayloadStatus::Stopped);

    // Claim, then only signed uploads are accepted.
    let seed = Seed::new([42; 32]);
    d.trust_claim(seed.public_key()).unwrap();
    assert!(d.trust_info().unwrap().state.is_claimed());
    let signed_img = image(3000, 12);
    assert!(
        d.upload(&signed_img, UploadOptions::default(), |_| {})
            .is_err(),
        "unsigned refused"
    );
    let auth = sign_commit(&signed_img, &seed, Version(7)).ok();
    d.upload(
        &signed_img,
        UploadOptions {
            auth,
            ..UploadOptions::default()
        },
        |_| {},
    )
    .unwrap();
    drop(d);

    // After a reboot the signed image is the installed one.
    let (mut svc, boot) = Service::open(&mut f, &MAP, FACTS, FACTORY).unwrap();
    assert!(matches!(boot.installed, Ok(h) if h.len == 3000));
    assert!(boot.trust.is_some_and(|p| p.trust.is_claimed()));
    let store = svc.store_mut();
    let h = store.header().unwrap();
    let mut buf = std::vec![0u8; h.len as usize];
    store.load(h, &mut buf).unwrap();
    assert_eq!(buf, signed_img);
}

#[test]
fn rt12_mwctl_sees_a_build_without_the_runtime() {
    let facts = Facts {
        firmware: "0.1.0+no-wasm",
        runtime: false,
        ..FACTS
    };
    let mut f = flash();
    let mut d = device(Wire::new(&mut f, facts));
    let info = d.info().unwrap();
    assert_eq!(info.chip, "rt1050");
    assert!(!info.runs_payloads());
    d.upload(&image(2000, 13), UploadOptions::default(), |_| {})
        .unwrap();
    assert!(d.start().is_err(), "no runtime: start is NotFound");
    assert_eq!(d.info().unwrap().status, PayloadStatus::Stopped);
}
