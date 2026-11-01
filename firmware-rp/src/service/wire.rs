//! `mwctl` against the service (requirement V9): the host tool's own
//! `Device` operations, unchanged, over an in-process byte stream.
//!
//! [`Wire`] is what the shell's CDC link and controller do with the byte
//! stream, minus USB: it cuts COBS frames with middleware's `Splitter`,
//! decodes requests with `frame::decode_wire`, hands them to the real
//! [`Service`] (over the RAM flash model), answers a start the way the
//! controller does once a payload runs, and publishes status changes on
//! `sys/payload/status` and notes on `sys/log`, all as wire frames.

extern crate std;

use std::collections::VecDeque;
use std::io;
use std::time::Duration;
use std::vec::Vec as StdVec;

use middleware::loader::{PayloadStatus, keys};
use middleware::loader::trust::Version;
use middleware::serial::frame::{self, Frame, MAX_RAW_FRAME, MAX_WIRE_FRAME, Split, Splitter};
use mwctl::device::{Device, UploadOptions};
use mwctl::link::{Link, Timeouts, Transport};
use mwctl::sign::{Seed, sign_commit};

use super::tests::{FACTS, Flash, MAP, NODE, flash, image};
use super::{Facts, Reply, Service};
use crate::fw_update::{Absent, FwUpdate, Unsupported};

/// The device end of the byte stream.
struct Wire<'f, U> {
    svc: Service<&'f mut Flash, U>,
    splitter: Splitter<MAX_WIRE_FRAME>,
    tx: VecDeque<u8>,
}

impl<'f, U: FwUpdate> Wire<'f, U> {
    fn new(flash: &'f mut Flash, facts: Facts, fw: U) -> Self {
        let (svc, _) = Service::open(flash, &MAP, facts, fw);
        Wire { svc, splitter: Splitter::new(), tx: VecDeque::new() }
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
            self.emit(&Frame::Publish { key: keys::LOG, payload: line.as_bytes() });
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
            self.emit(&Frame::Publish { key: keys::STATUS, payload: &status });
        }
        let body = result.as_deref().map_err(|e| *e);
        self.emit(&Frame::Response { id, result: body });
    }
}

impl<U: FwUpdate> Transport for Wire<'_, U> {
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

fn device<U: FwUpdate>(wire: Wire<'_, U>) -> Device<Link<Wire<'_, U>>> {
    let t = Timeouts { request: Duration::from_millis(50), begin: Duration::from_millis(50), retries: 1 };
    Device::new(Link::new(wire, t))
}

#[test]
fn v9_mwctl_info_upload_and_start_work_unchanged() {
    let mut f = flash();
    let mut d = device(Wire::new(&mut f, FACTS, Unsupported));
    let info = d.info().unwrap();
    assert_eq!((info.chip.as_str(), info.firmware.as_str()), ("rp2350", "0.1.0"));
    assert_eq!((info.capacity, info.status, info.node), (28 * 1024, PayloadStatus::Empty, Some(NODE)));
    assert!(info.runs_payloads());
    let img = image(5000, 11);
    let mut steps = 0;
    d.upload(&img, UploadOptions { start: true, ..UploadOptions::default() }, |_| steps += 1).unwrap();
    assert!(steps > 2);
    assert_eq!(d.info().unwrap().status, PayloadStatus::Running);
    d.stop().unwrap();
    assert_eq!(d.info().unwrap().status, PayloadStatus::Stopped);
    // The stored image is the uploaded one, after a reboot too.
    drop(d);
    let (mut svc, boot) = Service::open(&mut *f, &MAP, FACTS, Unsupported);
    assert!(matches!(boot.installed, Some(Ok(h)) if h.len == 5000));
    let store = svc.store_mut().unwrap();
    let h = store.header().unwrap();
    let mut buf = std::vec![0u8; h.len as usize];
    store.load(h, &mut buf).unwrap();
    assert_eq!(buf, img);
}

#[test]
fn v9_mwctl_signed_upload_and_trust_work_unchanged() {
    let seed = Seed::new([42; 32]);
    let mut f = flash();
    let mut d = device(Wire::new(&mut f, FACTS, Unsupported));
    d.trust_claim(seed.public_key()).unwrap();
    assert!(d.trust_info().unwrap().state.is_claimed());
    let img = image(3000, 12);
    // Unsigned: the device refuses, the CLI reports it.
    assert!(d.upload(&img, UploadOptions::default(), |_| {}).is_err());
    let auth = sign_commit(&img, &seed, Version(7));
    d.upload(&img, UploadOptions { auth, ..UploadOptions::default() }, |_| {}).unwrap();
    assert_eq!(d.info().unwrap().status, PayloadStatus::Stopped);
    let replay = sign_commit(&img, &seed, Version(7));
    assert!(d.upload(&img, UploadOptions { auth: replay, ..UploadOptions::default() }, |_| {}).is_err());
}

#[test]
fn v11_mwctl_sees_a_no_wasm_rp2040() {
    let facts = Facts { chip: "rp2040", firmware: "0.1.0+no-wasm", runtime: false, ..FACTS };
    let mut f = flash();
    let mut d = device(Wire::new(&mut f, facts, Absent));
    let info = d.info().unwrap();
    assert_eq!(info.chip, "rp2040");
    assert!(!info.runs_payloads());
    let img = image(2000, 13);
    d.upload(&img, UploadOptions::default(), |_| {}).unwrap();
    assert!(d.start().is_err(), "no runtime: start is NotFound");
    assert_eq!(d.info().unwrap().status, PayloadStatus::Stopped);
}
