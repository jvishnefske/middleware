//! ABI 7 channels in the runtime (CH3–CH5, BD4): a module's declarations,
//! their resolution against a binding before `init`, and the channel
//! table `channel_open` and `channel_publish` read.
//!
//! [`Compiled::new`](crate::Compiled::new) reads the module's `mw.channels`
//! section ([`middleware::wasm_abi::channels`]) while the image is still
//! in memory and keeps owned copies ([`Decl`]). At instantiation
//! [`plan`] decides, purely, what each declared channel becomes under a
//! [`Binding`]:
//!
//! | declaration | binding target | plan |
//! |-------------|----------------|------|
//! | in          | `Local(source)` | start the source route on the channel's internal key `ch/<name>` and subscribe to it |
//! | in          | `Key(key)`     | subscribe to `key` |
//! | in          | `Remote { node, key }` | subscribe to `key`, deliver only what `node` sent ([`Payload::deliver_from`](crate::Payload::deliver_from)) |
//! | in          | `Adapted`      | refused for now ([`BindError::Unsupported`]) |
//! | out         | `Key(key)` or `Remote { key, .. }` | `channel_publish` publishes on `key` |
//! | out         | `Local` or `Adapted` | refused ([`BindError::Direction`]) |
//! | required    | none           | refused ([`BindError::Unbound`]) |
//! | optional    | none           | unbound: `channel_open` answers `Unsupported` |
//!
//! A local source must fit the declaration: a samples channel takes a
//! sampled source whose sensor, format, channels and rate it accepts, a
//! frames channel a frame source, a bytes channel anything. Two input channels may not share a key, and the
//! bound keys count against the payload's subscriptions.
//! [`Payload::instantiate_bound`](crate::Payload::instantiate_bound)
//! carries the plan out: it starts each local route through the device
//! (with the same I2C ownership rules as `route_source`), records the
//! subscriptions, and only then calls `init`.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;
use middleware::loader::binding::{Binding, Target};
use middleware::stream::samples::{SampleFormat, SensorKind};
use middleware::wasm_abi::channels::{Channel, DataKind, Direction};
use middleware::wasm_abi::route::{RouteSource, SampleSource};
use middleware::{Key, SenderId};

pub use middleware::wasm_abi::channels::{ChannelError, OUTPUT_BASE};

/// One declared channel, owned (the image is freed after compiling).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decl {
    /// The name.
    pub name: String,
    /// In or out.
    pub dir: Direction,
    /// What it carries.
    pub kind: DataKind,
    /// The declaration's sample constraints and flag, as declared.
    channel: ChannelFields,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChannelFields {
    sensor: Option<SensorKind>,
    formats: u8,
    channels: u8,
    rate_min: u32,
    rate_max: u32,
    required: bool,
}

impl Decl {
    /// An owned copy of `c`.
    pub fn from_channel(c: &Channel<'_>) -> Self {
        Decl {
            name: c.name.to_string(),
            dir: c.dir,
            kind: c.kind,
            channel: ChannelFields {
                sensor: c.sensor,
                formats: c.formats,
                channels: c.channels,
                rate_min: c.rate_min,
                rate_max: c.rate_max,
                required: c.required,
            },
        }
    }

    /// The declaration as the ABI type.
    pub fn as_channel(&self) -> Channel<'_> {
        let f = self.channel;
        Channel {
            name: &self.name,
            dir: self.dir,
            kind: self.kind,
            sensor: f.sensor,
            formats: f.formats,
            channels: f.channels,
            rate_min: f.rate_min,
            rate_max: f.rate_max,
            required: f.required,
        }
    }

    /// Whether the payload cannot start without it.
    pub fn required(&self) -> bool {
        self.channel.required
    }
}

/// Why a binding could not be applied. Each names the channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindError {
    /// A required channel has no target.
    Unbound(String),
    /// A local or adapted target on an output channel.
    Direction(String),
    /// The local source (after its adapter) does not fit the declaration.
    Mismatch(String),
    /// A target this runtime does not resolve yet (remote and adapted
    /// streams).
    Unsupported(String),
    /// Two input channels on one key.
    SharedKey(String),
    /// More bound keys than subscription slots.
    NoResources(String),
    /// An input channel in a module without `mw_alloc` and `on_message`.
    NoDelivery(String),
    /// The device refused the local route (its code).
    Route(String, middleware::wasm_abi::AbiError),
}

impl BindError {
    /// The channel concerned.
    pub fn channel(&self) -> &str {
        match self {
            BindError::Unbound(n)
            | BindError::Direction(n)
            | BindError::Mismatch(n)
            | BindError::Unsupported(n)
            | BindError::SharedKey(n)
            | BindError::NoResources(n)
            | BindError::NoDelivery(n)
            | BindError::Route(n, _) => n,
        }
    }
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BindError::Unbound(n) => write!(f, "channel {n} is required and not bound"),
            BindError::Direction(n) => write!(f, "channel {n} is an output: bind it to a key"),
            BindError::Mismatch(n) => write!(f, "channel {n} does not take the bound source"),
            BindError::Unsupported(n) => write!(f, "channel {n}: this runtime cannot resolve its target"),
            BindError::SharedKey(n) => write!(f, "channel {n} shares its key with another input"),
            BindError::NoResources(n) => write!(f, "channel {n}: no subscription slot left"),
            BindError::NoDelivery(n) => {
                write!(f, "channel {n} is an input but the module exports no on_message")
            }
            BindError::Route(n, e) => write!(f, "channel {n}: the source route failed ({e:?})"),
        }
    }
}

/// What [`plan`] decided for one channel, in declaration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Optional and unbound.
    Unbound,
    /// Input: subscribe to `key`, after starting `route` on it for a
    /// local source; deliver only from `from` when set.
    Input {
        /// The subscribed key.
        key: String,
        /// The local source route to start on `key` first.
        route: Option<RouteSource>,
        /// Only this sender's messages are delivered.
        from: Option<SenderId>,
    },
    /// Output: `channel_publish` publishes on `key`.
    Output {
        /// The key.
        key: String,
    },
}

/// The sensor, format, channels and rate of what a sampled source
/// publishes.
pub const fn source_shape(s: &SampleSource) -> (SensorKind, SampleFormat, u8, u32) {
    match s {
        SampleSource::Imu(c) => (SensorKind::Imu, SampleFormat::I16, c.channels(), c.rate_hz as u32),
        SampleSource::Microphone(c) => (SensorKind::Microphone, c.mode.format(), 1, c.rate_hz as u32),
    }
}

/// Whether `decl` takes what `source` publishes.
fn fits(decl: &Decl, source: &RouteSource) -> Result<(), BindError> {
    let name = || decl.name.clone();
    match (decl.kind, source) {
        (DataKind::Bytes, _) | (DataKind::Frames, RouteSource::Frames(..)) => Ok(()),
        (DataKind::Samples, RouteSource::Samples(s)) => {
            let (sensor, format, channels, rate) = source_shape(s);
            if decl.as_channel().accepts_samples(sensor, format, channels, rate) {
                Ok(())
            } else {
                Err(BindError::Mismatch(name()))
            }
        }
        _ => Err(BindError::Mismatch(name())),
    }
}

/// The key a local channel's route publishes on, `ch/<name>`.
pub fn internal_key(name: &str) -> String {
    let mut k = String::from("ch/");
    k.push_str(name);
    k
}

/// Decides what each of `decls` becomes under `binding` (none: every
/// channel unbound), checking directions, shapes, shared keys and the
/// subscription limit (`free_slots` left) — see the module docs.
/// `delivers`: the module exports `mw_alloc` and `on_message`.
pub fn plan(
    decls: &[Decl],
    binding: Option<&Binding<'_>>,
    delivers: bool,
    free_slots: usize,
) -> Result<Vec<Step>, BindError> {
    let mut steps = Vec::with_capacity(decls.len());
    let mut inputs: Vec<String> = Vec::new();
    for d in decls {
        let name = || d.name.clone();
        let Some(target) = binding.and_then(|b| b.target(&d.name)) else {
            if d.required() {
                return Err(BindError::Unbound(name()));
            }
            steps.push(Step::Unbound);
            continue;
        };
        let step = match d.dir {
            Direction::Out => match target {
                Target::Key(k) | Target::Remote { key: k, .. } => Step::Output {
                    key: k.as_str().to_string(),
                },
                Target::Local(_) | Target::Adapted { .. } => return Err(BindError::Direction(name())),
            },
            Direction::In => {
                if !delivers {
                    return Err(BindError::NoDelivery(name()));
                }
                let (key, route, from) = match target {
                    Target::Local(source) => {
                        fits(d, &source)?;
                        (internal_key(&d.name), Some(source), None)
                    }
                    Target::Key(k) => (k.as_str().to_string(), None, None),
                    Target::Remote { node, key } => (key.as_str().to_string(), None, Some(node)),
                    Target::Adapted { .. } => return Err(BindError::Unsupported(name())),
                };
                if inputs.contains(&key) {
                    return Err(BindError::SharedKey(name()));
                }
                if inputs.len() == free_slots {
                    return Err(BindError::NoResources(name()));
                }
                inputs.push(key.clone());
                Step::Input { key, route, from }
            }
        };
        steps.push(step);
    }
    Ok(steps)
}

/// One resolved channel at run time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// Optional and unbound.
    Unbound,
    /// Delivered on subscription slot `slot`.
    Input {
        /// The subscription slot (`on_message`'s first argument).
        slot: usize,
        /// Only this sender's messages are delivered.
        from: Option<SenderId>,
    },
    /// Published on `key`.
    Output {
        /// The key.
        key: String,
    },
}

/// The channels of a running payload: declarations and what they
/// resolved to, in declaration order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelTable {
    /// The declarations.
    pub decls: Vec<Decl>,
    /// What each became.
    pub resolved: Vec<Resolved>,
}

impl ChannelTable {
    /// `channel_open(name)`: the input's slot or the output's handle;
    /// `InvalidArgument` for a name not declared, `Unsupported` for an
    /// unbound one.
    pub fn open(&self, name: &[u8]) -> Result<i32, middleware::wasm_abi::AbiError> {
        use middleware::wasm_abi::AbiError;
        let i = self
            .decls
            .iter()
            .position(|d| d.name.as_bytes() == name)
            .ok_or(AbiError::InvalidArgument)?;
        match self.resolved.get(i) {
            Some(Resolved::Input { slot, .. }) => i32::try_from(*slot).map_err(|_| AbiError::NoResources),
            Some(Resolved::Output { .. }) => Ok(OUTPUT_BASE + i as i32),
            _ => Err(AbiError::Unsupported),
        }
    }

    /// Whether a message on `slot` from `sender` may be delivered: always
    /// unless a remote channel listens on the slot and `sender` is not its
    /// node (an unknown sender included).
    pub fn admits(&self, slot: usize, sender: Option<SenderId>) -> bool {
        self.resolved.iter().all(|r| match r {
            Resolved::Input { slot: s, from: Some(node) } if *s == slot => sender == Some(*node),
            _ => true,
        })
    }

    /// The key an output handle publishes on; `InvalidArgument` for any
    /// other handle.
    pub fn output_key(&self, handle: i32) -> Result<Key<'_>, middleware::wasm_abi::AbiError> {
        use middleware::wasm_abi::AbiError;
        let i = handle
            .checked_sub(OUTPUT_BASE)
            .and_then(|i| usize::try_from(i).ok())
            .ok_or(AbiError::InvalidArgument)?;
        match self.resolved.get(i) {
            Some(Resolved::Output { key }) => Key::new(key).map_err(|_| AbiError::InvalidKey),
            _ => Err(AbiError::InvalidArgument),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::loader::binding::Binding;
    use middleware::loader::trust::Version;
    use middleware::stream::samples::SampleFormat;
    use middleware::wasm_abi::route::{AudioMode, ImuConfig, MicConfig, SourceConfig};

    fn decls() -> Vec<Decl> {
        [
            Channel::samples("audio", Direction::In, Some(SensorKind::Microphone), 8_000, 16_000, true)
                .with_formats(1 << SampleFormat::Levels as u8),
            Channel::bytes("cmd", Direction::In, false),
            Channel::bytes("level", Direction::Out, false),
        ]
        .iter()
        .map(Decl::from_channel)
        .collect()
    }

    fn levels(rate: u16) -> RouteSource {
        RouteSource::Samples(SampleSource::Microphone(
            MicConfig::new(rate, 0, AudioMode::Levels, 10, 160).unwrap(),
        ))
    }

    fn pcm() -> RouteSource {
        RouteSource::Samples(SampleSource::Microphone(
            MicConfig::new(16_000, 0, AudioMode::Pcm, 160, 0).unwrap(),
        ))
    }

    fn bind<'a>(entries: &[(&'a str, Target<'a>)]) -> Binding<'a> {
        let mut b = Binding::new([0; 32], Version(1));
        for (n, t) in entries {
            b.bind(n, *t).unwrap();
        }
        b
    }

    #[test]
    fn bd4_local_and_key_targets_become_routes_subscriptions_and_outputs() {
        let b = bind(&[
            ("audio", Target::Local(levels(16_000))),
            ("level", Target::Key(Key::from_static("lab/level"))),
        ]);
        let steps = plan(&decls(), Some(&b), true, 8).unwrap();
        assert_eq!(
            steps,
            [
                Step::Input {
                    key: "ch/audio".into(),
                    route: Some(levels(16_000)),
                    from: None,
                },
                Step::Unbound,
                Step::Output { key: "lab/level".into() },
            ]
        );
    }

    #[test]
    fn bd4_bad_bindings_name_their_channel() {
        let d = decls();
        let err = |entries: &[(&str, Target<'_>)], delivers, slots| plan(&d, Some(&bind(entries)), delivers, slots).unwrap_err();
        assert_eq!(plan(&d, None, true, 8), Err(BindError::Unbound("audio".into())));
        let audio = ("audio", Target::Local(levels(16_000)));
        assert_eq!(err(&[("audio", Target::Local(levels(32_000)))], true, 8), BindError::Mismatch("audio".into()));
        assert_eq!(err(&[("audio", Target::Local(pcm()))], true, 8), BindError::Mismatch("audio".into()));
        let imu = RouteSource::Samples(SampleSource::Imu(ImuConfig::new(100, 2, false, 10).unwrap()));
        assert_eq!(err(&[("audio", Target::Local(imu))], true, 8), BindError::Mismatch("audio".into()));
        let frames = RouteSource::Frames(
            middleware::wasm_abi::route::Source::TestPattern,
            SourceConfig::parse(&[8, 0, 8, 0, 0, 0, 100, 0, 64, 0]).unwrap(),
        );
        assert_eq!(err(&[("audio", Target::Local(frames))], true, 8), BindError::Mismatch("audio".into()));
        assert_eq!(err(&[audio, ("level", Target::Local(levels(16_000)))], true, 8), BindError::Direction("level".into()));
        let k = Target::Key(Key::from_static("ch/audio"));
        assert_eq!(err(&[audio, ("cmd", k)], true, 8), BindError::SharedKey("cmd".into()));
        assert_eq!(
            err(&[audio, ("cmd", Target::Key(Key::from_static("x")))], true, 1),
            BindError::NoResources("cmd".into())
        );
        assert_eq!(err(&[audio], false, 8), BindError::NoDelivery("audio".into()));
        let remote = Target::Remote {
            node: SenderId([1; 8]),
            key: Key::from_static("mic/level"),
        };
        let steps = plan(&d, Some(&bind(&[("audio", remote), ("level", remote)])), true, 8).unwrap();
        assert_eq!(
            steps[0],
            Step::Input { key: "mic/level".into(), route: None, from: Some(SenderId([1; 8])) }
        );
        assert_eq!(steps[2], Step::Output { key: "mic/level".into() });
        let adapted = Target::Adapted {
            inner: middleware::loader::binding::Endpoint::Local(pcm()),
            adapter: middleware::loader::binding::Adapter::PcmToLevels { block: 160 },
        };
        assert_eq!(err(&[("audio", adapted)], true, 8), BindError::Unsupported("audio".into()));
        assert_eq!(BindError::Unbound("x".into()).channel(), "x");
    }

    #[test]
    fn ch5_the_table_opens_inputs_by_slot_and_outputs_by_handle() {
        let t = ChannelTable {
            decls: decls(),
            resolved: alloc::vec![
                Resolved::Input { slot: 2, from: Some(SenderId([5; 8])) },
                Resolved::Unbound,
                Resolved::Output { key: "lab/level".into() },
            ],
        };
        use middleware::wasm_abi::AbiError;
        assert_eq!(t.open(b"audio"), Ok(2));
        assert_eq!(t.open(b"cmd"), Err(AbiError::Unsupported));
        assert_eq!(t.open(b"level"), Ok(OUTPUT_BASE + 2));
        assert_eq!(t.open(b"nope"), Err(AbiError::InvalidArgument));
        assert_eq!(t.output_key(OUTPUT_BASE + 2).map(|k| k.as_str()), Ok("lab/level"));
        for h in [2, OUTPUT_BASE, OUTPUT_BASE + 1, OUTPUT_BASE + 9, -1] {
            assert_eq!(t.output_key(h), Err(AbiError::InvalidArgument), "{h}");
        }
        // PR3: the remote input on slot 2 admits only its node.
        assert!(t.admits(2, Some(SenderId([5; 8]))));
        assert!(!t.admits(2, Some(SenderId([6; 8]))));
        assert!(!t.admits(2, None));
        assert!(t.admits(1, None));
    }
}
