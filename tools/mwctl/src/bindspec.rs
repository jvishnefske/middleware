//! `mwctl bind` target specs (BD5): `CHANNEL=TARGET` arguments into a
//! binding record for an image.
//!
//! | target                         | binds the channel to                         |
//! |--------------------------------|----------------------------------------------|
//! | `key:<key>`                    | a key on the board's bus                     |
//! | `remote:<node16>:<key>`        | a key, only from that node                   |
//! | `mic:<rate>:<mode>[:<gain>]`   | the microphone at `rate` Hz, `levels` (10 ms blocks, ten per message), `mulaw` or `pcm` (10 ms per message), gain in dB (0) |
//! | `imu:<rate>[:<range>[:mag]]`   | the IMU at `rate` Hz, ±`range` g (2), with the magnetometer; about 100 ms per message |
//! | `adapt:<adapter>:<target>`     | `target` through an adapter: `levels/<block>`, `mulaw`, `decimate/<factor>`, `magnitude` |
//! | `driver:<point>`               | a control-module point of the node's drivers (ABI 8; the node lists them at `sys/manifest/cm`) |

use middleware::Key;
use middleware::loader::binding::{Adapter, BINDING_MAX, Binding, Endpoint, Target};
use middleware::loader::trust::Version;
use middleware::stream::samples::MAX_BATCH_DATA;
use middleware::wasm_abi::route::{AudioMode, ImuConfig, MicConfig, RouteSource, SampleSource};
use middleware::SenderId;

/// One parsed endpoint, owning its key text.
enum Owned {
    Local(RouteSource),
    Key(String),
    Remote(SenderId, String),
    Driver(String),
}

fn number<T: std::str::FromStr>(s: &str, what: &str) -> Result<T, String> {
    s.parse().map_err(|_| format!("{what}: not a number: {s:?}"))
}

fn endpoint(spec: &str) -> Result<Owned, String> {
    let (kind, rest) = spec.split_once(':').ok_or_else(|| format!("target {spec:?} needs kind:..."))?;
    let parts: Vec<&str> = rest.split(':').collect();
    let bad = |e| format!("{spec}: the config is refused ({e:?})");
    Ok(match kind {
        "key" => Owned::Key(rest.to_string()),
        "driver" => Owned::Driver(rest.to_string()),
        "remote" => {
            let (node, key) = rest.split_once(':').ok_or("remote:<node16>:<key>")?;
            let bytes = (0..node.len() / 2)
                .map(|i| u8::from_str_radix(node.get(2 * i..2 * i + 2).unwrap_or("x"), 16))
                .collect::<Result<Vec<u8>, _>>()
                .map_err(|_| format!("node {node:?} is not hex"))?;
            let id: [u8; 8] = bytes.try_into().map_err(|_| format!("node {node:?} needs 16 hex digits"))?;
            Owned::Remote(SenderId(id), key.to_string())
        }
        "mic" => {
            let rate: u16 = number(parts.first().copied().unwrap_or(""), "rate")?;
            let mode = match parts.get(1).copied().unwrap_or("levels") {
                "levels" => AudioMode::Levels,
                "mulaw" => AudioMode::MuLaw,
                "pcm" => AudioMode::Pcm,
                m => return Err(format!("mode {m:?}: levels, mulaw or pcm")),
            };
            let gain: u8 = match parts.get(2) {
                Some(g) => number(g, "gain")?,
                None => 0,
            };
            let per_10ms = rate / 100;
            let (units, block) = match mode {
                AudioMode::Levels => (10, per_10ms.max(1)),
                AudioMode::MuLaw => (per_10ms.min(MAX_BATCH_DATA as u16), 0),
                AudioMode::Pcm => (per_10ms.min(MAX_BATCH_DATA as u16 / 2), 0),
            };
            Owned::Local(RouteSource::Samples(SampleSource::Microphone(
                MicConfig::new(rate, gain, mode, units, block).map_err(bad)?,
            )))
        }
        "imu" => {
            let rate: u16 = number(parts.first().copied().unwrap_or(""), "rate")?;
            let range: u8 = match parts.get(1) {
                Some(r) => number(r, "range")?,
                None => 2,
            };
            let mag = parts.get(2) == Some(&"mag");
            let per = (rate / 10).clamp(1, (MAX_BATCH_DATA / if mag { 12 } else { 6 }) as u16);
            Owned::Local(RouteSource::Samples(SampleSource::Imu(
                ImuConfig::new(rate, range, mag, per).map_err(bad)?,
            )))
        }
        k => return Err(format!("unknown target kind {k:?} (key, remote, mic, imu, adapt)")),
    })
}

fn adapter(spec: &str) -> Result<Adapter, String> {
    let (name, param) = spec.split_once('/').unwrap_or((spec, ""));
    let a = match name {
        "levels" => Adapter::decode(1, number(param, "block")?),
        "mulaw" => Adapter::decode(2, 0),
        "decimate" => Adapter::decode(3, number(param, "factor")?),
        "magnitude" => Adapter::decode(4, 0),
        _ => return Err(format!("unknown adapter {name:?} (levels/<block>, mulaw, decimate/<factor>, magnitude)")),
    };
    a.ok_or_else(|| format!("adapter {spec:?}: parameter out of range"))
}

/// Encodes the binding of `specs` (`CHANNEL=TARGET`, see the module docs)
/// for the image with SHA-256 `image_sha256` at `version`.
pub fn record(image_sha256: [u8; 32], version: u32, specs: &[String]) -> Result<Vec<u8>, String> {
    let mut parsed: Vec<(String, Owned, Option<Adapter>)> = Vec::new();
    for s in specs {
        let (name, target) = s.split_once('=').ok_or_else(|| format!("{s:?}: CHANNEL=TARGET"))?;
        let (inner, a) = match target.strip_prefix("adapt:") {
            Some(rest) => {
                let (a, t) = rest.split_once(':').ok_or("adapt:<adapter>:<target>")?;
                (endpoint(t)?, Some(adapter(a)?))
            }
            None => (endpoint(target)?, None),
        };
        parsed.push((name.to_string(), inner, a));
    }
    let key = |k: &str| match Key::new(k) {
        Ok(key) if middleware::loader::is_system_key(key) => Err(format!("key {k:?} is a sys key")),
        Ok(_) => Ok(()),
        Err(e) => Err(format!("key {k:?}: {e:?}")),
    };
    for (_, o, _) in &parsed {
        match o {
            Owned::Key(k) | Owned::Remote(_, k) => key(k)?,
            Owned::Local(_) | Owned::Driver(_) => {}
        }
    }
    let mut b = Binding::new(image_sha256, Version(version));
    for (name, o, a) in &parsed {
        let ep = match o {
            Owned::Local(s) => Endpoint::Local(*s),
            Owned::Key(k) => Endpoint::Key(Key::new(k).map_err(|e| format!("{e:?}"))?),
            Owned::Remote(n, k) => Endpoint::Remote { node: *n, key: Key::new(k).map_err(|e| format!("{e:?}"))? },
            Owned::Driver(p) => Endpoint::Driver(p),
        };
        let t = match (a, ep) {
            (Some(adapter), inner) => Target::Adapted { inner, adapter: *adapter },
            (None, Endpoint::Local(s)) => Target::Local(s),
            (None, Endpoint::Key(k)) => Target::Key(k),
            (None, Endpoint::Remote { node, key }) => Target::Remote { node, key },
            (None, Endpoint::Driver(p)) => Target::Driver(p),
        };
        b.bind(name, t).map_err(|e| format!("{name}: {e:?}"))?;
    }
    let mut out = [0u8; BINDING_MAX];
    let n = b.encode(&mut out).ok_or("binding too large")?;
    Binding::decode(&out[..n]).map_err(|e| format!("{e:?}"))?;
    Ok(out[..n].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(r: &[u8]) -> Binding<'_> {
        Binding::decode(r).unwrap()
    }

    #[test]
    fn bd5_specs_become_targets() {
        let specs: Vec<String> = [
            "audio=mic:16000:levels:20",
            "level=key:lab/level",
            "peer=remote:0011223344556677:mic/level",
            "accel=imu:100:4:mag",
            "pcm=adapt:levels/160:mic:16000:pcm",
        ]
        .map(String::from)
        .into();
        let r = record([1; 32], 9, &specs).unwrap();
        let b = decode(&r);
        assert_eq!((b.image_sha256, b.version), ([1; 32], Version(9)));
        let Some(Target::Local(RouteSource::Samples(SampleSource::Microphone(m)))) = b.target("audio") else { panic!() };
        assert_eq!((m.rate_hz, m.mode, m.gain_db, m.block, m.samples), (16_000, AudioMode::Levels, 20, 160, 10));
        assert_eq!(b.target("level"), Some(Target::Key(Key::from_static("lab/level"))));
        assert!(matches!(b.target("peer"), Some(Target::Remote { node: SenderId([0, 0x11, ..]), .. })));
        let Some(Target::Local(RouteSource::Samples(SampleSource::Imu(i)))) = b.target("accel") else { panic!() };
        assert_eq!((i.rate_hz, i.range_g, i.mag, i.samples), (100, 4, true, 10));
        assert!(matches!(b.target("pcm"), Some(Target::Adapted { adapter: Adapter::PcmToLevels { block: 160 }, .. })));
    }

    #[test]
    fn bd5_bad_specs_say_why() {
        let one = |s: &str| record([0; 32], 1, &[s.to_string()]).unwrap_err();
        assert!(one("audio").contains("CHANNEL=TARGET"));
        assert!(one("a=mic:44100").contains("refused"));
        assert!(one("a=mic:16000:loud").contains("levels, mulaw or pcm"));
        assert!(one("a=key:sys/info").contains("sys"), "{}", one("a=key:sys/info"));
        assert!(one("a=remote:12:k").contains("16 hex"));
        assert!(one("a=foo:1").contains("unknown target"));
        assert!(one("a=adapt:decimate/1:key:k").contains("out of range"));
        assert!(one("A=key:k").contains("Name"));
    }
}
