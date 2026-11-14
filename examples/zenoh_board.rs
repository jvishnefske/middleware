//! FL7, SC7/SC8 and the payload-subscription and signed-stop checks on the
//! bench: the host side of the i.MX RT1050-EVKB's Zenoh session.
//!
//! ```text
//! cargo run --example zenoh_board --features zenoh,trust -- [OPTIONS]
//!   --listen EP        Zenoh endpoint to listen on [tcp/0.0.0.0:7447]; the
//!                      board's sys/zenoh/endpoint points at this machine
//!   --key KEY          key to subscribe to [rt16/state]
//!   --count N          samples to print before the queries [5]
//!   --put KEY TEXT     after the samples, publish TEXT on KEY (a payload's
//!                      subscription), then print N more samples of --key
//!   --stop KEYFILE MAC finally send a signed sys/payload/stop (Ed25519 seed
//!                      file, the board's Ethernet MAC aa:bb:cc:dd:ee:ff as
//!                      the control target; counter = Unix time in ms), then
//!                      an unsigned one, which must be Denied
//! ```
//!
//! It opens a Zenoh peer listening on the endpoint, subscribes to the key
//! with a `ZenohTransport` and prints the samples with the sender id and
//! sequence the board's attachment carries; a sample that is a
//! `middleware::stream::samples` batch is printed decoded (header and the
//! first units). Then it queries that node's `@mw/<node>/sys/info` and
//! `sys/board` and prints the replies.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer as _, SigningKey};
use middleware::loader::trust::{
    ControlCounter, ControlOp, Signature, SignedControl, control_message,
};
use middleware::loader::{DeviceInfo, keys};
use middleware::stream::samples;
use middleware::transport::{RequestOptions, Target};
use middleware::zenoh::{SessionConfig, ZenohTransport};
use middleware::{Key, Publisher, Requester, Subscriber};

struct Args {
    listen: String,
    key: String,
    count: usize,
    put: Option<(String, String)>,
    stop: Option<(String, [u8; 6])>,
}

fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<u8> = s
        .split(':')
        .map(|p| u8::from_str_radix(p, 16).ok())
        .collect::<Option<_>>()?;
    parts.try_into().ok()
}

fn args() -> Args {
    let mut a = Args {
        listen: "tcp/0.0.0.0:7447".into(),
        key: "rt16/state".into(),
        count: 5,
        put: None,
        stop: None,
    };
    let mut it = std::env::args().skip(1);
    let need = |v: Option<String>, what: &str| v.unwrap_or_else(|| panic!("{what} needs a value"));
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--listen" => a.listen = need(it.next(), "--listen"),
            "--key" => a.key = need(it.next(), "--key"),
            "--count" => a.count = need(it.next(), "--count").parse().expect("--count N"),
            "--put" => a.put = Some((need(it.next(), "--put KEY"), need(it.next(), "--put TEXT"))),
            "--stop" => {
                let file = need(it.next(), "--stop KEYFILE");
                let mac = parse_mac(&need(it.next(), "--stop MAC")).expect("--stop MAC aa:bb:cc:dd:ee:ff");
                a.stop = Some((file, mac));
            }
            other => panic!("unknown argument {other}"),
        }
    }
    a
}

/// One line for a received payload: decoded when it is a sample batch.
fn describe(payload: &[u8]) -> String {
    match samples::decode(payload) {
        Ok(b) => {
            let h = b.header;
            let units: Vec<String> = match h.format {
                samples::SampleFormat::Levels => b.levels().map(|(r, p)| format!("{r}/{p}")).collect(),
                _ => b
                    .i16_values()
                    .collect::<Vec<_>>()
                    .chunks(usize::from(h.channels).max(1))
                    .map(|u| u.iter().map(i16::to_string).collect::<Vec<_>>().join(","))
                    .collect(),
            };
            let shown = units.iter().take(4).cloned().collect::<Vec<_>>().join(" ");
            format!(
                "batch {:?} {:?} ch {} {} Hz first {} n {} scale {}: {shown}{}",
                h.kind,
                h.format,
                h.channels,
                h.rate_hz,
                h.first_index,
                h.count,
                h.scale,
                if units.len() > 4 { " ..." } else { "" }
            )
        }
        Err(_) => format!("{payload:02x?}"),
    }
}

async fn print_samples(
    sub: &mut middleware::zenoh::ZenohSubscriber,
    n: usize,
) -> Option<middleware::SenderId> {
    let mut buf = vec![0u8; 4096];
    let mut node = None;
    for _ in 0..n {
        let s = sub.recv(&mut buf).await.expect("sample");
        println!(
            "[sample] {} {} sender {:?} seq {:?} gap {:?}",
            s.key.as_str(),
            describe(s.payload),
            s.meta.sender,
            s.meta.seq,
            s.meta.gap
        );
        node = s.meta.sender.or(node);
    }
    node
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let a = args();
    let key = Key::new(&a.key).expect("a middleware key");
    let config = SessionConfig::new().listen(a.listen.clone()).build().expect("config");
    let mut host = ZenohTransport::open(config)
        .await
        .expect("host session")
        .with_timeout(Duration::from_secs(5));
    println!("listening on {}; waiting for {} ...", a.listen, key.as_str());
    let mut sub = host.subscriber(key).await.expect("subscriber");
    let Some(node) = print_samples(&mut sub, a.count).await else {
        eprintln!("the samples carried no sender id");
        std::process::exit(1);
    };
    if let Some((put_key, text)) = &a.put {
        let k = Key::new(put_key).expect("a middleware key");
        host.publish(k, text.as_bytes()).await.expect("publish");
        println!("[put] {put_key}: {text}");
        print_samples(&mut sub, a.count).await;
    }
    let opts = RequestOptions::DEFAULT.with_target(Target::Node(node));
    let mut out = [0u8; 512];
    match host.request_with(keys::INFO, &[], &mut out, &opts).await {
        Ok(r) => match DeviceInfo::decode(r) {
            Some(i) => println!(
                "[sys/info] chip {} firmware {} capacity {} status {:?} node {:?} caps {:?}",
                i.chip, i.firmware, i.capacity, i.status, i.node, i.caps
            ),
            None => println!("[sys/info] {} bytes, not a DeviceInfo", r.len()),
        },
        Err(e) => println!("[sys/info] error: {e:?}"),
    }
    match host.request_with(keys::BOARD, &[], &mut out, &opts).await {
        Ok(r) => println!("[sys/board] {} bytes", r.len()),
        Err(e) => println!("[sys/board] {e:?}"),
    }
    if let Some((file, mac)) = &a.stop {
        let seed: [u8; 32] = std::fs::read(file)
            .expect("key file")
            .try_into()
            .expect("a 32-byte Ed25519 seed");
        let ms = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_millis();
        let counter = ControlCounter(u64::try_from(ms).expect("counter"));
        let m = control_message(ControlOp::Stop, mac, counter);
        let signature = Signature(SigningKey::from_bytes(&seed).sign(&m).to_bytes());
        let body = SignedControl { counter, signature }.encode();
        match host.request_with(keys::STOP, &[], &mut out, &opts).await {
            Ok(r) => println!("[sys/payload/stop unsigned] accepted?! {r:02x?}"),
            Err(e) => println!("[sys/payload/stop unsigned] {e:?}"),
        }
        match host.request_with(keys::STOP, &body, &mut out, &opts).await {
            Ok(_) => println!("[sys/payload/stop signed, counter {}] ok", counter.0),
            Err(e) => println!("[sys/payload/stop signed] {e:?}"),
        }
        match host.request_with(keys::STOP, &body, &mut out, &opts).await {
            Ok(_) => println!("[sys/payload/stop replayed] accepted?!"),
            Err(e) => println!("[sys/payload/stop replayed] {e:?}"),
        }
    }
}
