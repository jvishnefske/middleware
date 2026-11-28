use super::*;
use crate::board::{FIRMWARE, PINS};
use crate::link::{RESPONSE_LEN, respond};
use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};
use middleware::Key;
use middleware::loader::BoardInfo;
use middleware::loader::manifest::Manifest;
use middleware::serial::frame::Frame;
use middleware::serial::link::{RECV_BUF_LEN, SerialLink};
use middleware::transport::Requester;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::vec::Vec;

const ID: Identity<'static> = Identity {
    firmware: FIRMWARE,
    node: SenderId([8, 7, 6, 5, 4, 3, 2, 1]),
};

fn call<H: Handler>(h: &mut H, key: &'static str, body: &[u8]) -> Result<Vec<u8>, HandlerError> {
    let mut out = [0u8; RESPONSE_LEN];
    h.handle(&Request::new(Key::from_static(key), body), &mut out)
        .map(|n| out[..n].to_vec())
}

#[test]
fn st4_sys_info_reports_the_chip_firmware_and_node() {
    let bytes = call(&mut System::new(ID), "sys/info", &[]).unwrap();
    let info = DeviceInfo::decode(&bytes).unwrap();
    assert_eq!(info.chip, "stm32g0b1");
    assert!(info.firmware.ends_with("+no-wasm"));
    assert_eq!(info.node, Some(ID.node));
    assert_eq!((info.capacity, info.status), (0, PayloadStatus::Empty));
    assert_eq!(info.caps, Caps::NONE);
}

#[test]
fn st5_sys_board_and_manifest_decode() {
    let bytes = call(&mut System::new(ID), "sys/board", &[]).unwrap();
    let b = BoardInfo::decode(&bytes).unwrap();
    assert_eq!(
        (b.gpio, b.reserved),
        (PINS.gpio_mask(), PINS.reserved_mask())
    );
    let bytes = call(&mut System::new(ID), "sys/manifest", &[]).unwrap();
    let mut expect = [0u8; RESPONSE_LEN];
    let n = Manifest::encode(&MANIFEST, &mut expect).unwrap();
    assert_eq!(bytes, &expect[..n]);
}

#[test]
fn st6_bodies_on_system_keys_are_invalid_and_other_keys_not_found() {
    for key in ["sys/info", "sys/board", "sys/manifest"] {
        assert_eq!(
            call(&mut System::new(ID), key, &[1]),
            Err(HandlerError::InvalidRequest),
            "{key}"
        );
    }
    for key in ["sys/payload/start", "sys/fw/info", "sys", "app/x"] {
        assert_eq!(
            call(&mut System::new(ID), key, &[]),
            Err(HandlerError::NotFound),
            "{key}"
        );
    }
}

#[test]
fn st7_small_buffers_are_reported_not_truncated() {
    let mut out = [0u8; 8];
    let r = System::new(ID).handle(&Request::new(Key::from_static("sys/board"), &[]), &mut out);
    assert_eq!(r, Err(HandlerError::BufferTooSmall));
}

/// An application that answers one key and counts what reaches it.
struct Echo(u32);

impl Handler for Echo {
    fn handle(&mut self, req: &Request<'_>, out: &mut [u8]) -> Result<usize, HandlerError> {
        self.0 += 1;
        if req.key.as_str() != "app/echo" {
            return Err(HandlerError::NotFound);
        }
        let dst = out
            .get_mut(..req.body.len())
            .ok_or(HandlerError::BufferTooSmall)?;
        dst.copy_from_slice(req.body);
        Ok(req.body.len())
    }
}

#[test]
fn st8_node_routes_sys_to_the_system_and_the_rest_to_the_app() {
    let mut node = Node {
        system: System::new(ID),
        app: Echo(0),
    };
    assert!(call(&mut node, "sys/info", &[]).is_ok());
    assert_eq!(
        call(&mut node, "sys/nope", &[]),
        Err(HandlerError::NotFound)
    );
    assert_eq!(node.app.0, 0, "sys/ keys never reach the app");
    assert_eq!(call(&mut node, "app/echo", &[1, 2]), Ok(std::vec![1, 2]));
    assert_eq!(
        call(&mut node, "system/x", &[]),
        Err(HandlerError::NotFound)
    );
    assert_eq!(node.app.0, 2);
    assert!(is_system("sys") && is_system("sys/a/b") && !is_system("system") && !is_system("sy"));
}

#[test]
fn st9_respond_answers_requests_only() {
    let mut node = Node {
        system: System::new(ID),
        app: NoApp,
    };
    let mut out = [0u8; RESPONSE_LEN];
    let k = Key::from_static("app/x");
    assert_eq!(
        respond(
            &mut node,
            Frame::Publish {
                key: k,
                payload: &[]
            },
            &mut out
        ),
        None
    );
    assert_eq!(
        respond(
            &mut node,
            Frame::Response {
                id: 3,
                result: Ok(&[])
            },
            &mut out
        ),
        None
    );
    assert_eq!(
        respond(
            &mut node,
            Frame::Request {
                id: 9,
                key: k,
                payload: &[]
            },
            &mut out
        ),
        Some(Frame::Response {
            id: 9,
            result: Err(HandlerError::NotFound)
        })
    );
}

// ---- end to end over two real `SerialLink`s ------------------------------

type Wire = Rc<RefCell<VecDeque<u8>>>;

/// One end of an in-memory cable; reads come in short pieces like USB
/// packets.
struct End {
    rx: Wire,
    tx: Wire,
}

impl embedded_io_async::ErrorType for End {
    type Error = core::convert::Infallible;
}

impl embedded_io_async::Read for End {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        core::future::poll_fn(|_| {
            let mut rx = self.rx.borrow_mut();
            if rx.is_empty() {
                return Poll::Pending;
            }
            let n = buf.len().min(rx.len()).min(7);
            for (slot, b) in buf.iter_mut().zip(rx.drain(..n)) {
                *slot = b;
            }
            Poll::Ready(Ok(n))
        })
        .await
    }
}

impl embedded_io_async::Write for End {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.tx.borrow_mut().extend(buf);
        Ok(buf.len())
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn cable() -> (End, End) {
    let a: Wire = Rc::default();
    let b: Wire = Rc::default();
    (
        End {
            rx: a.clone(),
            tx: b.clone(),
        },
        End { rx: b, tx: a },
    )
}

/// Polls `host` and `device` in turn until `host` finishes; bounded so a
/// hang fails the test.
fn run_until<T>(host: impl Future<Output = T>, device: impl Future<Output = ()>) -> T {
    let mut cx = Context::from_waker(Waker::noop());
    let mut host = pin!(host);
    let mut device = pin!(device);
    for _ in 0..10_000 {
        if let Poll::Ready(v) = host.as_mut().poll(&mut cx) {
            return v;
        }
        let _ = device.as_mut().poll(&mut cx);
    }
    panic!("host never finished");
}

/// The device loop as `main.rs` runs it.
async fn device(mut link: SerialLink<End, 0>) {
    let mut rx = [0u8; RECV_BUF_LEN];
    let mut out = [0u8; RESPONSE_LEN];
    let mut node = Node {
        system: System::new(ID),
        app: Echo(0),
    };
    loop {
        let Ok(frame) = link.next_frame(&mut rx).await else {
            continue;
        };
        if let Some(resp) = respond(&mut node, frame, &mut out) {
            let _ = link.send(&resp).await;
        }
    }
}

#[test]
fn st10_a_host_reads_the_board_over_the_serial_link() {
    let (host_end, dev_end) = cable();
    let mut host = SerialLink::new(host_end);
    let mut buf = [0u8; RESPONSE_LEN];
    let host_side = async {
        let info = host
            .request(Key::from_static("sys/info"), &[], &mut buf)
            .await
            .unwrap();
        let info = DeviceInfo::decode(info).unwrap();
        assert_eq!((info.chip, info.node), ("stm32g0b1", Some(ID.node)));
        let board = host
            .request(Key::from_static("sys/board"), &[], &mut buf)
            .await
            .unwrap();
        let board = BoardInfo::decode(board).unwrap();
        assert_eq!(board.reserved, PINS.reserved_mask());
        let echo = host
            .request(Key::from_static("app/echo"), b"hi", &mut buf)
            .await
            .unwrap();
        assert_eq!(echo, b"hi");
        assert!(
            host.request(Key::from_static("sys/payload/start"), &[], &mut buf)
                .await
                .is_err()
        );
    };
    run_until(host_side, device(SerialLink::with_inbox(dev_end)));
}
