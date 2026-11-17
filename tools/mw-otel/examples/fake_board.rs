//! A stand-in for a board on the bridge's Zenoh network, for trying
//! `mw-otel-bridge` without hardware:
//!
//! ```text
//! cargo run -p mw-otel --example fake_board -- tcp/127.0.0.1:7447 [seconds]
//! ```
//!
//! It connects to the endpoint, answers `sys/info` (a `DeviceInfo` naming
//! a fake chip), and every 200 ms publishes on `app/demo/counter`, every
//! second on `imu/stat`, and every two seconds logs a line on `sys/log`.
use std::time::Duration;

use middleware::loader::{Caps, DeviceInfo, PayloadStatus, keys};
use middleware::zenoh::{SessionConfig, ZenohTransport};
use middleware::{HandlerError, Key, Publisher, Request, Responder};

const COUNTER: Key<'static> = Key::from_static("app/demo/counter");
const IMU_STAT: Key<'static> = Key::from_static("imu/stat");

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let endpoint = args.next().unwrap_or_else(|| "tcp/127.0.0.1:7447".into());
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    let config = SessionConfig::new()
        .connect(endpoint)
        .build()
        .expect("config");
    let t = ZenohTransport::open(config).await.expect("open");
    let node = t.sender_id();
    eprintln!("fake_board: node {}", middleware::zenoh::wire::zid16(node));

    let mut responder = t.responder(keys::INFO).await.expect("responder");
    tokio::spawn(async move {
        let mut scratch = [0u8; 256];
        let mut info = move |_: &Request<'_>, out: &mut [u8]| -> Result<usize, HandlerError> {
            DeviceInfo {
                abi_version: 5,
                capacity: 128 * 1024,
                status: PayloadStatus::Running,
                chip: "fake-rt1050",
                firmware: "fake_board 0.1",
                node: Some(node),
                caps: Caps::NONE,
            }
            .encode(out)
            .ok_or(HandlerError::BufferTooSmall)
        };
        while responder.serve_one(&mut info, &mut scratch).await.is_ok() {}
    });

    let mut counter_t = t.clone();
    let mut other = t.clone();
    let start = tokio::time::Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let mut n: u32 = 0;
    while start.elapsed() < Duration::from_secs(seconds) {
        tick.tick().await;
        n += 1;
        counter_t
            .publish(COUNTER, n.to_string().as_bytes())
            .await
            .expect("publish");
        if n.is_multiple_of(5) {
            other
                .publish(IMU_STAT, b"steps=5 avg_us=210")
                .await
                .expect("publish");
        }
        if n.is_multiple_of(10) {
            let line = format!(
                "fake_board: tick {n}, uptime {} s",
                start.elapsed().as_secs()
            );
            other
                .publish(keys::LOG, line.as_bytes())
                .await
                .expect("log");
        }
    }
    eprintln!("fake_board: done");
}
