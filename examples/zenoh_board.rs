//! FL7 on the bench: the host side of the i.MX RT1050-EVKB's Zenoh session.
//!
//! `cargo run --example zenoh_board --features zenoh -- [LISTEN] [KEY]`
//! (defaults `tcp/0.0.0.0:7447`, `rt16/state`) opens a Zenoh peer listening
//! on LISTEN (the board's `sys/zenoh/endpoint` points at this machine),
//! subscribes to KEY with a `ZenohTransport`, prints the first samples with
//! the sender id and sequence the board's attachment carries, then queries
//! that node's `@mw/<node>/sys/info` and `sys/board` and prints the replies
//! (decoded `sys/info`).
use std::time::Duration;

use middleware::loader::{DeviceInfo, keys};
use middleware::transport::{RequestOptions, Target};
use middleware::zenoh::{SessionConfig, ZenohTransport};
use middleware::{Key, Requester, Subscriber};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let mut args = std::env::args().skip(1);
    let listen = args.next().unwrap_or_else(|| "tcp/0.0.0.0:7447".into());
    let key = args.next().unwrap_or_else(|| "rt16/state".into());
    let key = Key::new(&key).expect("a middleware key");
    let config = SessionConfig::new().listen(listen.clone()).build().expect("config");
    let mut host = ZenohTransport::open(config)
        .await
        .expect("host session")
        .with_timeout(Duration::from_secs(5));
    println!("listening on {listen}; waiting for {} ...", key.as_str());
    let mut sub = host.subscriber(key).await.expect("subscriber");
    let mut buf = [0u8; 1024];
    let mut node = None;
    for _ in 0..5 {
        let s = sub.recv(&mut buf).await.expect("sample");
        println!(
            "[sample] {} {:02x?} sender {:?} seq {:?} gap {:?}",
            s.key.as_str(),
            s.payload,
            s.meta.sender,
            s.meta.seq,
            s.meta.gap
        );
        node = s.meta.sender;
    }
    let Some(node) = node else {
        eprintln!("the samples carried no sender id");
        std::process::exit(1);
    };
    let opts = RequestOptions::DEFAULT.with_target(Target::Node(node));
    let mut out = [0u8; 512];
    match host.request_with(keys::INFO, &[], &mut out, &opts).await {
        Ok(r) => match DeviceInfo::decode(r) {
            Some(i) => println!(
                "[sys/info] chip {} firmware {} capacity {} status {:?} node {:?}",
                i.chip, i.firmware, i.capacity, i.status, i.node
            ),
            None => println!("[sys/info] {} bytes, not a DeviceInfo", r.len()),
        },
        Err(e) => println!("[sys/info] error: {e:?}"),
    }
    match host.request_with(keys::BOARD, &[], &mut out, &opts).await {
        Ok(r) => println!("[sys/board] {} bytes", r.len()),
        Err(e) => println!("[sys/board] {e:?}"),
    }
}
