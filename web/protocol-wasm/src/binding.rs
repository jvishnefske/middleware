//! Binding checks for the browser (BD5): a payload's channel declarations
//! against a node's manifest and a proposed binding, as issues with
//! one-click fixes (the `pins` module's pattern).
//!
//! [`check`] looks at every declared channel and every binding entry:
//!
//! * a required channel without a target, or an entry naming no declared
//!   channel;
//! * an output bound to a local source (outputs bind to keys);
//! * a local source of the wrong kind for the channel (frames for a samples
//!   channel, the IMU for a microphone channel, a format the channel does
//!   not take);
//! * a local source the node does not offer (its manifest has no such
//!   source, or not at that rate, range, mode or gain), or outside the
//!   channel's rate range;
//! * a remote target naming a node the board has not heard (when its
//!   `sys/peers` is known);
//! * an adapter that does not take its local source, or whose output the
//!   channel does not take (`middleware::stream::adapt::Adapter::output`);
//! * ABI 8: a machine channel bound to anything but a key or a driver
//!   point, a driver point bound to a channel that is not a machine one,
//!   or a driver point on a node whose manifest lists no control modules.
//!   Whether the point exists, and fits, is the node's check (its
//!   `sys/manifest/cm` list, [`middleware::loader::control`]).
//!
//! Where an obvious target exists, the issue carries it as a [`Fix`]:
//! [`suggest`] picks a local source the manifest offers and the channel
//! takes (the lowest offered rate in the channel's range, 10 ms level
//! blocks, about 100 ms per message).

use middleware::loader::binding::{Adapter, Binding, Endpoint, Target};
use middleware::loader::manifest::Manifest;
use middleware::stream::adapt::Shape;
use middleware::stream::samples::{MAX_BATCH_DATA, SampleFormat, SensorKind};
use middleware::wasm_abi::channels::{Channel, Channels, DataKind, Direction};
use middleware::wasm_abi::route::{
    AudioMode, IMU_RANGES_G, ImuConfig, MicConfig, RouteSource, SampleSource,
};

/// A one-click fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fix {
    /// Bind the channel to this local source.
    Local(RouteSource),
    /// Bind the channel to this key (the channel's name).
    KeyNamed,
    /// Remove the entry.
    Remove,
}

/// One problem with a binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// The channel (or the entry's name).
    pub channel: String,
    /// What is wrong, in plain language.
    pub message: String,
    /// The obvious fix, if any.
    pub fix: Option<Fix>,
}

fn issue(channel: &str, message: String, fix: Option<Fix>) -> Issue {
    Issue {
        channel: channel.to_string(),
        message,
        fix,
    }
}

/// The sensor, format, channels and rate a sampled source publishes.
fn shape(s: &SampleSource) -> (SensorKind, SampleFormat, u8, u32) {
    match s {
        SampleSource::Imu(c) => (
            SensorKind::Imu,
            SampleFormat::I16,
            c.channels(),
            c.rate_hz.into(),
        ),
        SampleSource::Microphone(c) => {
            (SensorKind::Microphone, c.mode.format(), 1, c.rate_hz.into())
        }
    }
}

fn mode_for(format: SampleFormat) -> AudioMode {
    match format {
        SampleFormat::Levels => AudioMode::Levels,
        SampleFormat::MuLaw => AudioMode::MuLaw,
        SampleFormat::I16 => AudioMode::Pcm,
    }
}

/// A microphone config at `rate` in `mode`: 10 ms level blocks, ten per
/// message; about 10 ms of µ-law or PCM per message (as much as one batch
/// holds). `None` when no such batch exists (PCM above 24 kHz).
fn mic_config(rate: u16, mode: AudioMode) -> Option<MicConfig> {
    let per_10ms = rate / 100;
    let (units, block) = match mode {
        AudioMode::Levels => (10, per_10ms.max(1)),
        AudioMode::MuLaw => (per_10ms.min(MAX_BATCH_DATA as u16), 0),
        AudioMode::Pcm => (per_10ms.min(MAX_BATCH_DATA as u16 / 2), 0),
    };
    MicConfig::new(rate, 0, mode, units, block).ok()
}

/// A local source the manifest offers and `decl` takes, if any.
pub fn suggest(manifest: &Manifest, decl: &Channel<'_>) -> Option<RouteSource> {
    if decl.kind != DataKind::Samples || decl.dir != Direction::In {
        return None;
    }
    let in_range = |r: u16| u32::from(r) >= decl.rate_min && u32::from(r) <= decl.rate_max;
    let wants = |s: SensorKind| decl.sensor.is_none_or(|d| d == s);
    if wants(SensorKind::Microphone)
        && let Some(mic) = manifest.mic
    {
        for rate in mic.rates_hz().filter(|r| in_range(*r)) {
            for format in [SampleFormat::Levels, SampleFormat::MuLaw, SampleFormat::I16] {
                let mode = mode_for(format);
                let Some(cfg) = mic_config(rate, mode) else {
                    continue;
                };
                let source = SampleSource::Microphone(cfg);
                let (s, f, c, r) = shape(&source);
                if mic.accepts(&cfg) && decl.accepts_samples(s, f, c, r) {
                    return Some(RouteSource::Samples(source));
                }
            }
        }
    }
    if wants(SensorKind::Imu)
        && let Some(imu) = manifest.imu
    {
        let mag = decl.channels == 6;
        let range = imu.ranges_g().next().unwrap_or(IMU_RANGES_G[0]);
        let rates: Vec<u16> = if mag {
            imu.mag_rates_hz().collect()
        } else {
            imu.rates_hz().collect()
        };
        for rate in rates.into_iter().filter(|r| in_range(*r)) {
            let per_msg =
                (rate / 10).clamp(1, (MAX_BATCH_DATA / (if mag { 12 } else { 6 })) as u16);
            let Ok(cfg) = ImuConfig::new(rate, range, mag, per_msg) else {
                continue;
            };
            let source = SampleSource::Imu(cfg);
            let (s, f, c, r) = shape(&source);
            if imu.accepts(&cfg) && decl.accepts_samples(s, f, c, r) {
                return Some(RouteSource::Samples(source));
            }
        }
    }
    None
}

/// What is wrong with binding `decl` to the local `source` (through
/// `adapter`) on a node with `manifest`, if anything.
fn local_problem(
    manifest: &Manifest,
    decl: &Channel<'_>,
    source: &RouteSource,
    adapter: Option<Adapter>,
) -> Option<String> {
    if let Some(a) = adapter {
        let RouteSource::Samples(s) = source else {
            return Some(format!("{a:?} takes sample batches, not frames"));
        };
        let (sensor, format, channels, rate_hz) = shape(s);
        let Some(out) = a.output(Shape {
            sensor,
            format,
            channels,
            rate_hz,
        }) else {
            return Some(format!("{a:?} does not take {sensor:?} {format:?}"));
        };
        if decl.kind == DataKind::Samples
            && !decl.accepts_samples(out.sensor, out.format, out.channels, out.rate_hz)
        {
            return Some(format!(
                "the channel does not take what {a:?} makes ({:?} at {} Hz)",
                out.format, out.rate_hz
            ));
        }
        if !manifest.accepts_samples(s) {
            return Some(format!(
                "the node does not offer this {sensor:?} config ({rate_hz} Hz)"
            ));
        }
        return None;
    }
    match (decl.kind, source) {
        (
            k @ (DataKind::Digital | DataKind::Position | DataKind::Setpoint | DataKind::Segments),
            _,
        ) => Some(format!(
            "a {} channel binds to a driver point or a key, not a local source",
            k.name()
        )),
        (DataKind::Bytes, _) => None,
        (DataKind::Frames, RouteSource::Frames(src, cfg)) => match manifest.frames {
            Some(f) if f.accepts(src.kind(), cfg) => None,
            _ => Some("the node does not offer this frame source".into()),
        },
        (DataKind::Frames, RouteSource::Samples(_)) => {
            Some("a frames channel takes a frame source".into())
        }
        (DataKind::Samples, RouteSource::Frames(..)) => {
            Some("a samples channel takes a sampled source".into())
        }
        (DataKind::Samples, RouteSource::Samples(s)) => {
            let (sensor, format, channels, rate) = shape(s);
            if decl.sensor.is_some_and(|d| d != sensor) {
                return Some(format!(
                    "the channel takes {:?} samples, not {sensor:?}",
                    decl.sensor?
                ));
            }
            if rate < decl.rate_min || rate > decl.rate_max {
                return Some(format!(
                    "{rate} Hz is outside the channel's {}..={} Hz",
                    decl.rate_min, decl.rate_max
                ));
            }
            if !decl.accepts_samples(sensor, format, channels, rate) {
                return Some(format!(
                    "the channel does not take {format:?} with {channels} channels"
                ));
            }
            if !manifest.accepts_samples(s) {
                return Some(match (s, manifest.imu, manifest.mic) {
                    (SampleSource::Imu(_), None, _) | (SampleSource::Microphone(_), _, None) => {
                        format!("the node has no {sensor:?} source")
                    }
                    _ => format!("the node does not offer this {sensor:?} config ({rate} Hz)"),
                });
            }
            None
        }
    }
}

/// Every issue of `binding` for a payload declaring `channels` on a node
/// with `manifest` (see the module docs).
pub fn check(
    manifest: &Manifest,
    channels: &Channels<'_>,
    binding: &Binding<'_>,
    peers: Option<&[middleware::SenderId]>,
) -> Vec<Issue> {
    let mut out = Vec::new();
    for decl in channels.iter() {
        let name = decl.name;
        match binding.target(name) {
            None if decl.required => out.push(issue(
                name,
                format!("{name} is required: bind it"),
                match decl.dir {
                    Direction::In => suggest(manifest, decl).map(Fix::Local),
                    Direction::Out => Some(Fix::KeyNamed),
                },
            )),
            None => {}
            Some(Target::Local(_) | Target::Adapted { .. }) if decl.dir == Direction::Out => out
                .push(issue(
                    name,
                    format!("{name} is an output: bind it to a key"),
                    Some(Fix::KeyNamed),
                )),
            Some(
                t @ (Target::Local(_)
                | Target::Adapted {
                    inner: Endpoint::Local(_),
                    ..
                }),
            ) => {
                let (Endpoint::Local(source), adapter) = t.parts() else {
                    continue;
                };
                if let Some(problem) = local_problem(manifest, decl, &source, adapter) {
                    let fix = suggest(manifest, decl)
                        .filter(|s| *s != source)
                        .map(Fix::Local);
                    out.push(issue(name, format!("{name}: {problem}"), fix));
                }
            }
            Some(Target::Driver(_)) if !decl.kind.is_machine() => out.push(issue(
                name,
                format!(
                    "{name} carries {}: only machine channels bind to driver points",
                    decl.kind.name()
                ),
                None,
            )),
            Some(Target::Driver(point)) if manifest.control.is_none() => out.push(issue(
                name,
                format!("{name}: the node lists no control modules, so it has no point {point}"),
                None,
            )),
            Some(Target::Remote { node, .. }) if peers.is_some_and(|p| !p.contains(&node)) => {
                let hex: String = node.0.iter().map(|b| format!("{b:02x}")).collect();
                out.push(issue(
                    name,
                    format!("{name}: the board has not heard node {hex}"),
                    None,
                ));
            }
            Some(_) => {}
        }
    }
    for (name, _) in binding.entries() {
        if channels.get(name).is_none() {
            out.push(issue(
                name,
                format!("the payload declares no channel {name}"),
                Some(Fix::Remove),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::Key;
    use middleware::loader::manifest::{ImuSource, MicSource};
    use middleware::loader::trust::Version;
    use middleware::wasm_abi::channels::{encode, encoded_len};

    const DECLS: &[Channel<'static>] = &[
        Channel::samples(
            "audio",
            Direction::In,
            Some(SensorKind::Microphone),
            8_000,
            48_000,
            true,
        )
        .with_formats(1 << SampleFormat::Levels as u8),
        Channel::samples(
            "accel",
            Direction::In,
            Some(SensorKind::Imu),
            25,
            100,
            false,
        ),
        Channel::bytes("level", Direction::Out, true),
    ];
    const LEN: usize = encoded_len(DECLS);
    static SECTION: [u8; LEN] = encode::<LEN>(DECLS);

    fn evkb() -> Manifest {
        Manifest::new(7)
            .with_imu(ImuSource::ALL)
            .with_mic(MicSource {
                rates: 0b0110,
                modes: 0b111,
                max_gain_db: 30,
            })
    }

    fn mic(rate: u16, mode: AudioMode) -> RouteSource {
        RouteSource::Samples(SampleSource::Microphone(mic_config(rate, mode).unwrap()))
    }

    fn binding<'a>(entries: &[(&'a str, Target<'a>)]) -> Binding<'a> {
        let mut b = Binding::new([0; 32], Version(1));
        for (n, t) in entries {
            b.bind(n, *t).unwrap();
        }
        b
    }

    #[test]
    fn bd5_suggestions_fit_the_channel_and_the_node() {
        let m = evkb();
        // The lowest offered rate in range, as levels.
        assert_eq!(suggest(&m, &DECLS[0]), Some(mic(16_000, AudioMode::Levels)));
        let s = suggest(&m, &DECLS[1]).unwrap();
        assert!(
            matches!(s, RouteSource::Samples(SampleSource::Imu(c)) if c.rate_hz == 50 && !c.mag)
        );
        assert_eq!(suggest(&m, &DECLS[2]), None);
        assert_eq!(suggest(&Manifest::new(7), &DECLS[0]), None);
        assert_eq!(mic_config(48_000, AudioMode::Pcm), None);
    }

    #[test]
    fn bd5_issues_name_the_channel_and_offer_a_fix() {
        let m = evkb();
        let c = Channels::parse(&SECTION).unwrap();
        let level = ("level", Target::Key(Key::from_static("lab/level")));
        let good = binding(&[
            ("audio", Target::Local(mic(16_000, AudioMode::Levels))),
            level,
        ]);
        assert_eq!(check(&m, &c, &good, None), []);
        let issues = check(&m, &c, &binding(&[]), None);
        assert_eq!(issues.len(), 2);
        assert_eq!(
            (issues[0].channel.as_str(), issues[0].fix),
            ("audio", Some(Fix::Local(mic(16_000, AudioMode::Levels))))
        );
        assert_eq!(issues[1].fix, Some(Fix::KeyNamed));
        let one =
            |t: Target<'static>| check(&m, &c, &binding(&[("audio", t), level]), None).remove(0);
        // A rate the node lacks, a mode the channel refuses, the IMU.
        let i = one(Target::Local(mic(8_000, AudioMode::Levels)));
        assert!(i.message.contains("does not offer"), "{}", i.message);
        assert_eq!(i.fix, Some(Fix::Local(mic(16_000, AudioMode::Levels))));
        assert!(
            one(Target::Local(mic(16_000, AudioMode::MuLaw)))
                .message
                .contains("does not take")
        );
        let imu = RouteSource::Samples(SampleSource::Imu(ImuConfig::new(50, 2, false, 5).unwrap()));
        assert!(one(Target::Local(imu)).message.contains("takes Microphone"));
        let none = check(
            &Manifest::new(7),
            &c,
            &binding(&[
                ("audio", Target::Local(mic(16_000, AudioMode::Levels))),
                level,
            ]),
            None,
        );
        assert!(
            none[0].message.contains("no Microphone source") && none[0].fix.is_none(),
            "{none:?}"
        );
        // An output on a source; an entry for nothing.
        let out = check(
            &m,
            &c,
            &binding(&[
                ("audio", Target::Local(mic(16_000, AudioMode::Levels))),
                ("level", Target::Local(mic(16_000, AudioMode::Levels))),
            ]),
            None,
        );
        assert_eq!(
            (out[0].channel.as_str(), out[0].fix),
            ("level", Some(Fix::KeyNamed))
        );
        let extra = check(
            &m,
            &c,
            &binding(&[
                ("audio", Target::Local(mic(16_000, AudioMode::Levels))),
                level,
                ("lvl", Target::Key(Key::from_static("x"))),
            ]),
            None,
        );
        assert_eq!(
            (extra[0].channel.as_str(), extra[0].fix),
            ("lvl", Some(Fix::Remove))
        );
        // PR4: a remote node the board has not heard.
        let node = middleware::SenderId([7; 8]);
        let remote = binding(&[
            (
                "audio",
                Target::Remote {
                    node,
                    key: Key::from_static("mic/level"),
                },
            ),
            level,
        ]);
        assert_eq!(check(&m, &c, &remote, None), []);
        assert_eq!(check(&m, &c, &remote, Some(&[node])), []);
        let unheard = check(&m, &c, &remote, Some(&[]));
        assert!(
            unheard[0]
                .message
                .contains("has not heard node 0707070707070707"),
            "{unheard:?}"
        );
        // AD3: PCM through the levels adapter fits a levels channel; a
        // µ-law adapter on a levels source, or µ-law into it, does not.
        let adapted = |inner, adapter| {
            binding(&[
                (
                    "audio",
                    Target::Adapted {
                        inner: Endpoint::Local(inner),
                        adapter,
                    },
                ),
                level,
            ])
        };
        let pcm = mic(16_000, AudioMode::Pcm);
        assert_eq!(
            check(
                &m,
                &c,
                &adapted(pcm, Adapter::PcmToLevels { block: 160 }),
                None
            ),
            []
        );
        let i = check(
            &m,
            &c,
            &adapted(mic(16_000, AudioMode::Levels), Adapter::PcmToMuLaw),
            None,
        );
        assert!(
            i[0].message.contains("does not take Microphone Levels"),
            "{i:?}"
        );
        let i = check(&m, &c, &adapted(pcm, Adapter::PcmToMuLaw), None);
        assert!(
            i[0].message.contains("does not take what PcmToMuLaw makes"),
            "{i:?}"
        );
    }
}
