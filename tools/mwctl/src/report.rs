//! Human-readable output.
//!
//! * [`format_info`] — `sys/info` as aligned `name: value` lines, noting a
//!   protocol-only (`+no-wasm`) firmware and an ABI mismatch.
//! * [`format_trust`] — `sys/trust/info`: the state, the last signed
//!   version and the pinned key in hex.
//! * [`format_board`] — `sys/board`: limits, flags and each pin class as a
//!   GPIO list, then the PWM table.
//! * [`format_manifest`] — `sys/manifest`: import groups, capabilities,
//!   buses, each source with its rates, links and limits, one per line.
//! * [`format_event`] — one line per publication: `sys/log` as text,
//!   `sys/payload/status` by name, anything else as UTF-8 or hex; noise runs
//!   as their length and printable characters.
//! * [`format_fw_info`] — `sys/fw/info`: update state, running slot, each
//!   slot's version and TBYB flag, the anti-rollback floor and the update
//!   in progress.
//! * [`format_samples`] — a publication that is a
//!   [`middleware::stream::samples`] batch (`monitor --samples`): one line
//!   with the key, the header and the units.
//! * [`status_name`] — the payload status names used by the web page.

use crate::device::Info;
use crate::link::Event;
use middleware::SenderId;
use middleware::loader::trust::{TrustCode, TrustInfo, TrustState};
use middleware::loader::{
    BOARD_PWM_REF_TICK, BOARD_RUNTIME, BOARD_USB_JTAG, BoardInfo, Caps, PayloadStatus, keys,
};
use middleware::wasm_abi::ABI_VERSION;
use std::fmt::Write as _;

/// The name of a payload status.
pub const fn status_name(s: PayloadStatus) -> &'static str {
    match s {
        PayloadStatus::Empty => "empty",
        PayloadStatus::Stopped => "stopped",
        PayloadStatus::Running => "running",
        PayloadStatus::Trapped => "trapped",
        PayloadStatus::Loading => "loading",
    }
}

/// `sys/info` for printing.
pub fn format_info(i: &Info) -> String {
    let abi = if i.abi_version == ABI_VERSION {
        format!("v{}", i.abi_version)
    } else {
        format!("v{} (mwctl expects v{ABI_VERSION})", i.abi_version)
    };
    let runtime = if i.runs_payloads() {
        "WASM (payloads run)"
    } else {
        "none: protocol only, payloads are stored but not run"
    };
    let node = match i.node {
        Some(SenderId(b)) => format!(
            "node:             {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}\n",
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]
        ),
        None => String::new(),
    };
    let serves = if i.caps.contains(Caps::SERVE) {
        "requests:         routed to the payload (serve)\n"
    } else {
        ""
    };
    let bridges = if i.caps.contains(Caps::BRIDGE) {
        "links:            bridged by rules (route_bridge)\n"
    } else {
        ""
    };
    let sources = match (
        i.caps.contains(Caps::SOURCE_IMU),
        i.caps.contains(Caps::SOURCE_MICROPHONE),
    ) {
        (true, true) => "sources:          imu, microphone (route_source)\n",
        (true, false) => "sources:          imu (route_source)\n",
        (false, true) => "sources:          microphone (route_source)\n",
        (false, false) => "",
    };
    format!(
        "chip:             {}\nfirmware:         {}\n{node}runtime:          {runtime}\n{serves}{bridges}{sources}ABI:              {abi}\npayload capacity: {} bytes\npayload status:   {}\n",
        i.chip,
        i.firmware,
        i.capacity,
        status_name(i.status)
    )
}

/// `sys/net/addr` for printing.
pub fn format_net(link: u8, a: &middleware::loader::NetAddr) -> String {
    let ip = |o: [u8; 4]| std::net::Ipv4Addr::from(o).to_string();
    let m = a.mac;
    let peer = if a.pinned() {
        format!("{}:{} (pinned)", ip(a.peer_ip), a.peer_port)
    } else {
        "learned (the latest unicast sender)".to_string()
    };
    format!(
        "link:             {link}\nip:               {}\nmac:              {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\npeer:             {peer}\n",
        ip(a.ip),
        m[0],
        m[1],
        m[2],
        m[3],
        m[4],
        m[5]
    )
}

fn pin_list(mask: u64) -> String {
    let pins: Vec<String> = (0..64)
        .filter(|n| mask >> n & 1 == 1)
        .map(|n: u32| n.to_string())
        .collect();
    if pins.is_empty() {
        "-".into()
    } else {
        pins.join(" ")
    }
}

/// `sys/board` for printing.
pub fn format_board(b: &BoardInfo<'_>) -> String {
    let yes = |f: u8| if b.flags & f != 0 { "yes" } else { "no" };
    let mut s = format!(
        "ABI:              v{}\nmax image:        {} bytes\nmax memory:       {} bytes\nruntime:          {}\nUSB-Serial-JTAG:  {}\nconsole UART:     tx {} rx {}\n",
        b.abi_version,
        b.max_image,
        b.max_memory,
        yes(BOARD_RUNTIME),
        yes(BOARD_USB_JTAG),
        b.uart_tx,
        b.uart_rx,
    );
    for (name, mask) in [
        ("gpio", b.gpio),
        ("reserved", b.reserved),
        ("input only", b.input_only),
        ("strapping", b.strapping),
        ("ADC1", b.adc),
    ] {
        let _ = writeln!(s, "{:<18}{}", format!("{name}:"), pin_list(mask));
    }
    let _ = write!(
        s,
        "PWM:              {} bits, REF_TICK {}",
        b.pwm_max_bits,
        yes(BOARD_PWM_REF_TICK)
    );
    for (i, pin) in b.pwm_pins.iter().enumerate() {
        let _ = write!(
            s,
            "{}({},{})={pin}",
            if i == 0 { "; " } else { " " },
            i / 2,
            i % 2
        );
    }
    s.push('\n');
    s
}

/// A binding record for printing: the image, the version, one line per
/// channel; `None` when it does not decode.
pub fn format_binding(record: &[u8]) -> Option<String> {
    use middleware::loader::binding::{Binding, Endpoint, Target};
    use middleware::wasm_abi::route::{RouteSource, SampleSource};
    let b = Binding::decode(record).ok()?;
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let endpoint = |e: &Endpoint<'_>| match e {
        Endpoint::Key(k) => format!("key {}", k.as_str()),
        Endpoint::Driver(p) => format!("driver point {p}"),
        Endpoint::Remote { node, key } => format!("key {} from {}", key.as_str(), hex(&node.0)),
        Endpoint::Local(RouteSource::Samples(SampleSource::Microphone(m))) => format!(
            "microphone {} Hz {:?}, gain {} dB, {} units per message{}",
            m.rate_hz,
            m.mode,
            m.gain_db,
            m.samples,
            if m.block > 0 {
                format!(", {}-sample blocks", m.block)
            } else {
                String::new()
            }
        ),
        Endpoint::Local(RouteSource::Samples(SampleSource::Imu(c))) => format!(
            "IMU {} Hz ±{} g{}, {} samples per message",
            c.rate_hz,
            c.range_g,
            if c.mag { " with magnetometer" } else { "" },
            c.samples
        ),
        Endpoint::Local(RouteSource::Frames(s, c)) => {
            format!("{:?} {}x{}", s.kind(), c.width, c.height)
        }
    };
    let mut s = format!(
        "image:   {}\nversion: {}\n",
        hex(&b.image_sha256),
        b.version.0
    );
    for (name, t) in b.entries() {
        let line = match t {
            Target::Adapted { inner, adapter } => {
                format!("{} through {adapter:?}", endpoint(&inner))
            }
            other => endpoint(&other.parts().0),
        };
        let _ = writeln!(s, "{name}: {line}");
    }
    Some(s)
}

/// `sys/manifest` for printing (MF4): one `name: value` line per record
/// the node sent.
pub fn format_manifest(m: &middleware::loader::manifest::Manifest) -> String {
    use middleware::loader::manifest::Groups;
    let join = |v: Vec<String>| {
        if v.is_empty() {
            "-".to_owned()
        } else {
            v.join(" ")
        }
    };
    let named = |mask: u8, names: &[&str]| {
        join(
            names
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, n)| (*n).to_owned())
                .collect(),
        )
    };
    let plural = |n: u32, one: &str| {
        if n == 1 {
            format!("1 {one}")
        } else {
            format!("{n} {one}es")
        }
    };
    let mut s = format!("ABI:              v{}\n", m.abi);
    if let Some(node) = m.node {
        let _ = writeln!(
            s,
            "node:             {}",
            node.0
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
    }
    let groups = Groups::NAMED
        .iter()
        .filter(|(g, _)| m.groups.contains(*g))
        .map(|(_, n)| (*n).to_owned())
        .collect();
    let _ = writeln!(s, "groups:           {}", join(groups));
    let caps = Caps::NAMED
        .iter()
        .filter(|(c, _)| m.caps().contains(*c))
        .map(|(_, n)| (*n).to_owned())
        .collect();
    let _ = writeln!(s, "caps:             {}", join(caps));
    if let Some(i) = m.i2c {
        let _ = writeln!(
            s,
            "I2C:              {}, {}-{} Hz",
            plural(i.buses.into(), "bus"),
            i.min_hz,
            i.max_hz
        );
    }
    if let Some(sp) = m.spi {
        let _ = writeln!(s, "SPI:              {}", plural(sp.buses.into(), "bus"));
    }
    let hz = |v: Vec<u16>| join(v.iter().map(|r| r.to_string()).collect());
    if let Some(i) = m.imu {
        let ranges: Vec<String> = i.ranges_g().map(|g| g.to_string()).collect();
        let _ = write!(
            s,
            "IMU:              {} Hz; ±{} g",
            hz(i.rates_hz().collect()),
            ranges.join("/")
        );
        let mag: Vec<u16> = i.mag_rates_hz().collect();
        if mag.is_empty() {
            s.push_str("; no magnetometer\n");
        } else {
            let _ = writeln!(s, "; with magnetometer {} Hz", hz(mag));
        }
    }
    if let Some(mc) = m.mic {
        let _ = writeln!(
            s,
            "microphone:       {} Hz; {}; gain 0-{} dB",
            hz(mc.rates_hz().collect()),
            named(mc.modes, &["levels", "mulaw", "pcm"]),
            mc.max_gain_db
        );
    }
    if let Some(f) = m.frames {
        let _ = writeln!(
            s,
            "frames:           {}; {}; up to {}x{}",
            named(f.kinds, &["test-pattern", "camera"]),
            named(f.formats, &["rgb565", "jpeg"]),
            f.max_width,
            f.max_height
        );
    }
    if let Some(sk) = m.sink_spi {
        let _ = writeln!(s, "SPI sink:         frames up to {} bytes", sk.max_frame);
    }
    let links = m
        .links()
        .iter()
        .map(|l| {
            format!(
                "{} {}",
                l.id,
                l.kind
                    .name()
                    .map_or_else(|| format!("kind{}", l.kind.0), str::to_owned)
            )
        })
        .collect();
    let _ = writeln!(s, "links:            {}", join(links));
    if let Some(l) = m.limits {
        let _ = writeln!(
            s,
            "limits:           {} routes, {} subscriptions, {} channels, {}-byte messages",
            l.routes, l.subscriptions, l.channels, l.max_message
        );
    }
    if let Some(b) = &m.board {
        let _ = writeln!(
            s,
            "board:            {}",
            String::from_utf8_lossy(b.as_bytes())
        );
    }
    if let Some(c) = m.control {
        let _ = writeln!(
            s,
            "control modules:  {} points on {} page{} of sys/manifest/cm",
            c.points,
            c.pages,
            if c.pages == 1 { "" } else { "s" }
        );
    }
    s
}

/// `sys/fw/info` for printing.
pub fn format_fw_info(i: &middleware::fw_ota::FwInfo) -> String {
    use crate::fw::format_version;
    use middleware::fw_ota::Slot;
    let slot = |s: Slot| {
        let info = i.slots[s as usize];
        let what = match info.version {
            None => "empty".to_owned(),
            Some(v) if info.tbyb => format!("{} (TBYB, not bought)", format_version(v)),
            Some(v) => format_version(v),
        };
        let running = if i.running == s { " <- running" } else { "" };
        format!("slot {}:           {what}{running}\n", s.name())
    };
    let target = match i.target {
        Some(t) => format!(
            "update:           {} {} into slot {}{}\n",
            i.state.name(),
            format_version(i.version),
            t.name(),
            if i.len > 0 {
                format!(", {}/{} bytes", i.received, i.len)
            } else {
                String::new()
            }
        ),
        None => format!("update:           {}\n", i.state.name()),
    };
    let b = i.node.0;
    format!(
        "{}{}{target}min version:      {} (a new image must be newer)\nself-test:        {}\nnode:             {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}\n",
        slot(Slot::A),
        slot(Slot::B),
        format_version(i.min_version),
        if i.self_test_ok {
            "passed"
        } else {
            "not passed"
        },
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7]
    )
}

/// `sys/trust/info` for printing.
pub fn format_trust(t: &TrustInfo) -> String {
    let state = match t.code() {
        TrustCode::Unclaimed => "unclaimed (any upload is accepted; the first claim pins a key)",
        TrustCode::Claimed => "claimed",
        TrustCode::RekeyOpen => "claimed, re-key window open (a new claim is accepted)",
    };
    match t.state {
        TrustState::Unclaimed => format!("trust:            {state}\n"),
        TrustState::Claimed { key, min_version } => format!(
            "trust:            {state}\nlast version:     {}\npinned key:       {} ({})\n",
            min_version.0,
            crate::sign::hex(key.as_bytes()),
            key.kind().name()
        ),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for (n, b) in bytes.iter().enumerate() {
        if n > 0 {
            s.push(' ');
        }
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn printable(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                char::from(b)
            } else {
                '.'
            }
        })
        .collect()
}

/// One line for a publication that decodes as a sample batch; `None` for
/// anything else (then [`format_event`] applies):
///
/// `[samples] KEY KIND FORMAT ch=C rate=HZ first=I t=US n=N range=R scale=S: U U ...`
///
/// where each unit `U` is its channels joined by `,` (`i16` values; µ-law
/// decoded to `i16`; levels as `rms/peak`). A line `mic_capture.py` and
/// other scripts parse, so keep its shape.
pub fn format_samples(e: &Event) -> Option<String> {
    use middleware::stream::samples::{SampleFormat, SensorKind, decode, mulaw_decode};
    let Event::Publish { key, payload } = e else {
        return None;
    };
    if key.starts_with("sys/") {
        return None;
    }
    let b = decode(payload).ok()?;
    let h = b.header;
    let kind = match h.kind {
        SensorKind::Imu => "imu",
        SensorKind::Microphone => "mic",
    };
    let channel_values: Vec<String> = match h.format {
        SampleFormat::I16 => b.i16_values().map(|v| v.to_string()).collect(),
        SampleFormat::MuLaw => b
            .data
            .iter()
            .map(|c| mulaw_decode(*c).to_string())
            .collect(),
        SampleFormat::Levels => b.levels().map(|(r, p)| format!("{r}/{p}")).collect(),
    };
    let format = match h.format {
        SampleFormat::I16 => "i16",
        SampleFormat::MuLaw => "mulaw",
        SampleFormat::Levels => "levels",
    };
    let per_unit = usize::from(h.channels).max(1);
    let units: Vec<String> = channel_values
        .chunks(per_unit)
        .map(|u| u.join(","))
        .collect();
    Some(format!(
        "[samples] {key} {kind} {format} ch={} rate={} first={} t={} n={} range={} scale={}: {}",
        h.channels,
        h.rate_hz,
        h.first_index,
        h.timestamp_us,
        h.count,
        h.range,
        h.scale,
        units.join(" ")
    ))
}

/// One line describing `e`.
pub fn format_event(e: &Event) -> String {
    match e {
        Event::Publish { key, payload } if key == keys::LOG.as_str() => {
            // Control characters (an ANSI escape from a radio-supplied
            // name, say) are shown escaped, never sent to the terminal.
            let text = String::from_utf8_lossy(payload);
            let line: String = text
                .trim_end()
                .chars()
                .flat_map(|c| {
                    let escaped = c.is_control().then(|| c.escape_default());
                    escaped
                        .into_iter()
                        .flatten()
                        .chain((!c.is_control()).then_some(c))
                })
                .collect();
            format!("[log] {line}")
        }
        Event::Publish { key, payload } if key == keys::STATUS.as_str() => {
            match payload.as_slice() {
                [b] => match PayloadStatus::from_byte(*b) {
                    Some(s) => format!("[status] {}", status_name(s)),
                    None => format!("[status] unknown ({b})"),
                },
                other => format!("[status] malformed ({})", hex(other)),
            }
        }
        Event::Publish { key, payload } => match std::str::from_utf8(payload) {
            Ok(text) if !text.chars().any(char::is_control) => format!("[pub] {key}: {text}"),
            _ => format!("[pub] {key}: {}", hex(payload)),
        },
        Event::Noise(bytes) => format!("[noise] {} byte(s): {}", bytes.len(), printable(bytes)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mf4_manifest_lines_name_every_record() {
        use middleware::loader::manifest::*;
        let m = Manifest::new(6)
            .with_node(SenderId([0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77]))
            .with_groups(Groups::GPIO.with(Groups::I2C).with(Groups::SERVE))
            .with_i2c(I2cBuses {
                buses: 1,
                min_hz: 10_000,
                max_hz: 1_000_000,
            })
            .with_imu(ImuSource {
                rates: ImuSource::rates_mask(&[50, 100, 200, 400, 800]),
                ranges: 0b111,
                mag_rates: ImuSource::rates_mask(&[50, 100, 200, 400]),
            })
            .with_mic(MicSource {
                rates: 0b1111,
                modes: 0b101,
                max_gain_db: 30,
            })
            .with_link(Link {
                id: 0,
                kind: LinkKind::SERIAL,
            })
            .with_link(Link {
                id: 2,
                kind: LinkKind(9),
            })
            .with_limits(Limits {
                routes: 2,
                ..Limits::ABI
            })
            .with_board(BoardName::new("ZynqMP KD240 revA/B/1").unwrap())
            .with_control(ControlModules {
                points: 10,
                pages: 1,
            });
        let s = format_manifest(&m);
        for line in [
            "ABI:              v6\n",
            "node:             0011223344556677\n",
            "groups:           gpio i2c serve\n",
            "caps:             serve source_imu source_microphone\n",
            "I2C:              1 bus, 10000-1000000 Hz\n",
            "IMU:              50 100 200 400 800 Hz; ±2/4/8 g; with magnetometer 50 100 200 400 Hz\n",
            "microphone:       8000 16000 32000 48000 Hz; levels pcm; gain 0-30 dB\n",
            "links:            0 serial 2 kind9\n",
            "limits:           2 routes, 8 subscriptions, 16 channels, 512-byte messages\n",
            "board:            ZynqMP KD240 revA/B/1\n",
            "control modules:  10 points on 1 page of sys/manifest/cm\n",
        ] {
            assert!(s.contains(line), "{line:?} in\n{s}");
        }
        assert!(!s.contains("SPI"));
        let bare = format_manifest(&Manifest::new(6));
        assert_eq!(
            bare,
            "ABI:              v6\ngroups:           -\ncaps:             -\nlinks:            -\n"
        );
    }

    #[test]
    fn samples_lines_decode_imu_pcm_mulaw_and_levels_batches() {
        use middleware::stream::samples::fake::{imu_batch, mic_batch};
        use middleware::wasm_abi::route::{AudioMode, ImuConfig, MicConfig};
        let mut out = [0u8; middleware::stream::samples::MAX_BATCH];
        let publish = |key: &str, b: &[u8]| Event::Publish {
            key: key.into(),
            payload: b.to_vec(),
        };

        let imu = ImuConfig::new(50, 2, false, 2).unwrap();
        let n = imu_batch(&imu, 10, 1234, 2, &mut out).unwrap();
        let line = format_samples(&publish("imu/raw", &out[..n])).unwrap();
        let batch = middleware::stream::samples::decode(&out[..n]).unwrap();
        let v: Vec<i16> = batch.i16_values().collect();
        let want = format!(
            "[samples] imu/raw imu i16 ch=3 rate=50 first=10 t=1234 n=2 range=2 scale=61: {},{},{} {},{},{}",
            v[0], v[1], v[2], v[3], v[4], v[5]
        );
        assert_eq!(line, want);

        let pcm = MicConfig::new(16000, 0, AudioMode::Pcm, 160, 0).unwrap();
        let n = mic_batch(&pcm, 0, 0, 160, &mut out).unwrap();
        let line = format_samples(&publish("mic/pcm", &out[..n])).unwrap();
        assert!(
            line.starts_with(
                "[samples] mic/pcm mic i16 ch=1 rate=16000 first=0 t=0 n=160 range=0 scale=0: "
            ),
            "{line}"
        );
        assert_eq!(line.split(": ").nth(1).unwrap().split(' ').count(), 160);

        let mu = MicConfig::new(8000, 0, AudioMode::MuLaw, 80, 0).unwrap();
        let n = mic_batch(&mu, 0, 0, 80, &mut out).unwrap();
        assert!(
            format_samples(&publish("mic/mu", &out[..n]))
                .unwrap()
                .contains(" mulaw ch=1 ")
        );

        let lv = MicConfig::new(8000, 0, AudioMode::Levels, 2, 400).unwrap();
        let n = mic_batch(&lv, 0, 0, 2, &mut out).unwrap();
        let line = format_samples(&publish("mic/level", &out[..n])).unwrap();
        assert!(
            line.contains(" levels ch=1 rate=8000 ") && line.contains("scale=400: "),
            "{line}"
        );
        let units: Vec<&str> = line.split(": ").nth(1).unwrap().split(' ').collect();
        assert_eq!(units.len(), 2);
        assert!(units.iter().all(|u| u.split('/').count() == 2));

        // Not a batch, or a system key: left to format_event.
        assert_eq!(format_samples(&publish("app/x", b"hello")), None);
        assert_eq!(format_samples(&publish("sys/log", &out[..n])), None);
        assert_eq!(format_samples(&Event::Noise(vec![1, 2])), None);
    }

    #[test]
    fn info_notes_protocol_only_firmware() {
        let i = Info {
            abi_version: ABI_VERSION,
            capacity: 65536,
            status: PayloadStatus::Stopped,
            chip: "esp8266".into(),
            firmware: "0.1.0+no-wasm".into(),
            node: None,
            caps: Caps::NONE,
        };
        let s = format_info(&i);
        assert!(!s.contains("requests:"), "{s}");
        assert!(!s.contains("node:"), "{s}");
        assert!(s.contains("chip:             esp8266\n"), "{s}");
        assert!(s.contains("protocol only"), "{s}");
        assert!(s.contains("65536 bytes"), "{s}");
        assert!(s.contains("stopped"), "{s}");
        let s = format_info(&Info {
            firmware: "0.1.0".into(),
            abi_version: ABI_VERSION + 1,
            ..i.clone()
        });
        assert!(s.contains("WASM (payloads run)"), "{s}");
        assert!(s.contains("mwctl expects"), "{s}");
        let s = format_info(&Info {
            node: Some(SenderId::from_mac([0x24, 0x6f, 0x28, 0xaa, 0xbb, 0xcc])),
            ..i.clone()
        });
        assert!(
            s.contains("firmware:         0.1.0+no-wasm\nnode:             246f28aabbcc0000\n"),
            "{s}"
        );
        let s = format_info(&Info {
            caps: Caps::SERVE,
            ..i.clone()
        });
        assert!(!s.contains("bridged"));
        assert!(!s.contains("sources:"), "{s}");
        let b = format_info(&Info {
            caps: Caps::BRIDGE,
            ..i.clone()
        });
        assert!(
            b.contains("links:            bridged by rules (route_bridge)\n"),
            "{b}"
        );
        assert!(!b.contains("sources:"), "{b}");
        // An EVKB with both sample sources: caps 13 = SERVE | IMU | MIC.
        let rt = format_info(&Info {
            caps: Caps(13),
            ..i.clone()
        });
        assert!(
            rt.contains("sources:          imu, microphone (route_source)\n"),
            "{rt}"
        );
        assert!(!rt.contains("bridged"), "{rt}");
        let imu = format_info(&Info {
            caps: Caps::SERVE.with(Caps::SOURCE_IMU),
            ..i.clone()
        });
        assert!(
            imu.contains("sources:          imu (route_source)\n"),
            "{imu}"
        );
        let mic = format_info(&Info {
            caps: Caps::SOURCE_MICROPHONE,
            ..i
        });
        assert!(
            mic.contains("sources:          microphone (route_source)\n"),
            "{mic}"
        );
        assert!(s.contains("runtime:          none: protocol only, payloads are stored but not run\nrequests:         routed to the payload (serve)\n"), "{s}");
    }

    #[test]
    fn board_lists_pin_classes_and_pwm_table() {
        let b = BoardInfo {
            abi_version: 3,
            max_image: 81920,
            max_memory: 131072,
            gpio: 0b1111,
            reserved: 0b10,
            input_only: 0,
            strapping: 0b1,
            adc: 0,
            pwm_max_bits: 14,
            flags: BOARD_RUNTIME | BOARD_USB_JTAG,
            uart_tx: 43,
            uart_rx: 44,
            pwm_pins: &[2, 3],
        };
        let s = format_board(&b);
        assert!(s.contains("gpio:             0 1 2 3\n"), "{s}");
        assert!(s.contains("strapping:        0\n"), "{s}");
        assert!(s.contains("input only:       -\n"), "{s}");
        assert!(s.contains("USB-Serial-JTAG:  yes"), "{s}");
        assert!(s.contains("14 bits, REF_TICK no; (0,0)=2 (0,1)=3\n"), "{s}");
    }

    #[test]
    fn trust_info_shows_state_version_and_key() {
        use middleware::loader::trust::{PublicKey, Version};
        let s = format_trust(&TrustState::Unclaimed.info(false));
        assert!(s.starts_with("trust:            unclaimed"), "{s}");
        let claimed = TrustState::Claimed {
            key: PublicKey::Ed25519([0xab; 32]),
            min_version: Version(42),
        };
        let s = format_trust(&claimed.info(false));
        assert!(s.contains("trust:            claimed\n"), "{s}");
        assert!(s.contains("last version:     42"), "{s}");
        assert!(s.contains(&"ab".repeat(32)), "{s}");
        assert!(format_trust(&claimed.info(true)).contains("re-key window open"));
    }

    #[test]
    fn y9_fw_info_names_slots_versions_and_progress() {
        use middleware::fw_ota::Slot;
        use middleware::fw_ota::wire::{FwInfo, SlotInfo, StateCode};
        use middleware::loader::trust::Version;
        let i = FwInfo {
            state: StateCode::Receiving,
            running: Slot::A,
            slots: [
                SlotInfo {
                    version: Some(Version(0x0001_0000)),
                    tbyb: false,
                },
                SlotInfo::default(),
            ],
            min_version: Version(0x0001_0000),
            node: SenderId([1, 2, 3, 4, 5, 6, 7, 8]),
            target: Some(Slot::B),
            version: Version(0x0001_0001),
            len: 2048,
            received: 512,
            self_test_ok: true,
        };
        let s = format_fw_info(&i);
        assert!(s.contains("slot A:           1.0 <- running\n"), "{s}");
        assert!(s.contains("slot B:           empty\n"), "{s}");
        assert!(
            s.contains("update:           receiving 1.1 into slot B, 512/2048 bytes\n"),
            "{s}"
        );
        assert!(s.contains("node:             0102030405060708"), "{s}");
        let t = FwInfo {
            state: StateCode::Trial,
            running: Slot::B,
            slots: [
                i.slots[0],
                SlotInfo {
                    version: Some(Version(0x0001_0001)),
                    tbyb: true,
                },
            ],
            len: 0,
            target: None,
            self_test_ok: false,
            ..i
        };
        let s = format_fw_info(&t);
        assert!(
            s.contains("slot B:           1.1 (TBYB, not bought) <- running"),
            "{s}"
        );
        assert!(
            s.contains("update:           trial\n") && s.contains("not passed"),
            "{s}"
        );
    }

    #[test]
    fn events_are_one_readable_line() {
        let p = |k: &str, v: &[u8]| Event::Publish {
            key: k.into(),
            payload: v.to_vec(),
        };
        assert_eq!(format_event(&p("sys/log", b"hello\n")), "[log] hello");
        assert_eq!(
            format_event(&p("sys/log", b"disc: up x\x1b[2Jy\rz\n")),
            "[log] disc: up x\\u{1b}[2Jy\\rz"
        );
        assert_eq!(
            format_event(&p("sys/log", "caf\u{e9}".as_bytes())),
            "[log] caf\u{e9}"
        );
        assert_eq!(
            format_event(&p("sys/payload/status", &[2])),
            "[status] running"
        );
        assert_eq!(
            format_event(&p("sys/payload/status", &[9])),
            "[status] unknown (9)"
        );
        assert_eq!(format_event(&p("app/t", b"21.5")), "[pub] app/t: 21.5");
        assert_eq!(format_event(&p("app/t", &[0, 255])), "[pub] app/t: 00 ff");
        assert_eq!(
            format_event(&Event::Noise(b"ets\x01Jan".to_vec())),
            "[noise] 7 byte(s): ets.Jan"
        );
    }
}
