//! Logical channels (ABI 8, CH6): name the data a payload reads and writes,
//! and let a binding decide where it comes from.
//!
//! Declare the channels once at module level with [`channels!`](crate::channels!); it builds
//! the `mw.channels` custom section at compile time (a bad declaration
//! fails the build) and defines `CHANNELS`, the declarations as values:
//!
//! ```ignore
//! mw_payload::channels! {
//!     audio: in samples(mic, levels, 8000..=48000),
//!     cmd: in bytes optional,
//!     level: out bytes,
//! }
//! ```
//!
//! An entry is `name: in|out kind [optional]`, where `kind` is `bytes`,
//! `frames`, one of the ABI 9 machine kinds `digital`, `position`,
//! `setpoint`, `segments` (messages in [`machine`](crate::channel::machine)),
//! or `samples(sensor, [format,] lo..=hi)` with `sensor` one of
//! `imu`, `mic`, `any`, `format` one of `i16`, `mulaw`, `levels`, and the
//! rate range in Hz. Entries are required unless marked `optional`.
//!
//! At run time [`Channel::open`] returns the channel the binding resolved:
//! an input is the subscription slot its messages arrive on in
//! `on_message`, an output publishes with [`Output::publish`]. Write the
//! payload's logic against the [`Input`], [`SampleInput`] and [`Output`]
//! traits so it runs the same on a board, in the simulators and in a host
//! unit test with a fake output:
//!
//! ```ignore
//! struct Meter<O: Output> { out: O }
//! impl<O: Output> SampleInput for Meter<O> {
//!     fn on_batch(&mut self, b: &Batch<'_>) { let _ = self.out.publish(&[..]); }
//! }
//! ```

use crate::{AbiError, Slot, check, len_i32, ptr_i32, sys};

pub use middleware::stream::samples::{
    Batch, BatchError, BatchHeader, SampleFormat, SensorKind, encode as encode_batch,
};
pub use middleware::wasm_abi::channels::{
    Channel as Decl, DataKind, Direction, MAX_CHANNELS, OUTPUT_BASE, SECTION, encode, encoded_len,
};
/// The machine channels' messages (ABI 9).
pub use middleware::wasm_abi::machine;

/// An open channel: an input's subscription slot or an output's handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channel {
    handle: i32,
}

impl Channel {
    /// Opens the channel `name` (`channel_open`): `InvalidArgument` when
    /// the image does not declare it, `Unsupported` when the binding left
    /// it unbound (an optional channel) or the host predates ABI 8.
    pub fn open(name: &str) -> Result<Self, AbiError> {
        let b = name.as_bytes();
        let handle = check(sys::channel_open(ptr_i32(b), len_i32(b.len())?))?;
        Ok(Channel { handle })
    }

    /// A channel from a raw handle (for tests and the host stand-ins).
    pub const fn from_handle(handle: i32) -> Self {
        Channel { handle }
    }

    /// The raw handle.
    pub const fn handle(&self) -> i32 {
        self.handle
    }

    /// The input's slot, `None` for an output.
    pub fn slot(&self) -> Option<Slot> {
        if self.handle < OUTPUT_BASE {
            Slot::from_raw(self.handle)
        } else {
            None
        }
    }

    /// Whether a message on `slot` belongs to this input channel.
    pub fn is(&self, slot: Slot) -> bool {
        self.slot() == Some(slot)
    }

    /// Hands `data` to `input` when it arrived on this channel; whether it
    /// did.
    pub fn deliver<I: Input + ?Sized>(&self, slot: Slot, data: &[u8], input: &mut I) -> bool {
        let ours = self.is(slot);
        if ours {
            input.on_data(data);
        }
        ours
    }

    /// Decodes a [`middleware::stream::samples`] batch that arrived on this
    /// channel and hands it to `input`; `Ok(false)` for another channel's
    /// message, the decode error for a malformed batch (dropped).
    pub fn deliver_samples<S: SampleInput + ?Sized>(
        &self,
        slot: Slot,
        data: &[u8],
        input: &mut S,
    ) -> Result<bool, BatchError> {
        if !self.is(slot) {
            return Ok(false);
        }
        let batch = middleware::stream::samples::decode(data)?;
        input.on_batch(&batch);
        Ok(true)
    }
}

/// Something that takes the messages of an input channel.
pub trait Input {
    /// One message.
    fn on_data(&mut self, data: &[u8]);
}

/// Something that takes the sample batches of an input channel.
pub trait SampleInput {
    /// One decoded batch.
    fn on_batch(&mut self, batch: &Batch<'_>);
}

/// Something messages can be published to: an output [`Channel`], or a
/// test's recorder.
pub trait Output {
    /// Publishes one message (at most [`crate::MAX_MESSAGE`] bytes).
    fn publish(&mut self, data: &[u8]) -> Result<(), AbiError>;
}

impl Output for Channel {
    /// `channel_publish`: `InvalidArgument` on an input channel.
    fn publish(&mut self, data: &[u8]) -> Result<(), AbiError> {
        if self.handle < OUTPUT_BASE {
            return Err(AbiError::InvalidArgument);
        }
        check(sys::channel_publish(
            self.handle,
            ptr_i32(data),
            len_i32(data.len())?,
        ))
        .map(drop)
    }
}

/// Declares the payload's channels (see the [module docs](self)).
#[macro_export]
macro_rules! channels {
    ($($name:ident : $dir:ident $kind:ident $(($($arg:tt)*))? $($flag:ident)?),* $(,)?) => {
        /// The channels this payload declares (its `mw.channels` section).
        pub const CHANNELS: &[$crate::channel::Decl<'static>] = &[
            $($crate::__mw_channel!($name, $dir, $kind, ($($($arg)*)?), $($flag)?)),*
        ];
        $crate::__mw_channels_section!(CHANNELS);
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __mw_channel {
    ($name:ident, $dir:ident, bytes, (), $($flag:ident)?) => {
        $crate::channel::Decl::bytes(
            ::core::stringify!($name),
            $crate::__mw_dir!($dir),
            $crate::__mw_required!($($flag)?),
        )
    };
    ($name:ident, $dir:ident, frames, (), $($flag:ident)?) => {
        $crate::channel::Decl::frames(
            ::core::stringify!($name),
            $crate::__mw_dir!($dir),
            $crate::__mw_required!($($flag)?),
        )
    };
    ($name:ident, $dir:ident, digital, (), $($flag:ident)?) => {
        $crate::__mw_machine!($name, $dir, Digital, $($flag)?)
    };
    ($name:ident, $dir:ident, position, (), $($flag:ident)?) => {
        $crate::__mw_machine!($name, $dir, Position, $($flag)?)
    };
    ($name:ident, $dir:ident, setpoint, (), $($flag:ident)?) => {
        $crate::__mw_machine!($name, $dir, Setpoint, $($flag)?)
    };
    ($name:ident, $dir:ident, segments, (), $($flag:ident)?) => {
        $crate::__mw_machine!($name, $dir, Segments, $($flag)?)
    };
    ($name:ident, $dir:ident, samples, ($sensor:ident, $lo:literal ..= $hi:literal), $($flag:ident)?) => {
        $crate::channel::Decl::samples(
            ::core::stringify!($name),
            $crate::__mw_dir!($dir),
            $crate::__mw_sensor!($sensor),
            $lo,
            $hi,
            $crate::__mw_required!($($flag)?),
        )
    };
    ($name:ident, $dir:ident, samples, ($sensor:ident, $fmt:ident, $lo:literal ..= $hi:literal), $($flag:ident)?) => {
        $crate::__mw_channel!($name, $dir, samples, ($sensor, $lo..=$hi), $($flag)?)
            .with_formats(1 << ($crate::__mw_format!($fmt) as u8))
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __mw_machine {
    ($name:ident, $dir:ident, $kind:ident, $($flag:ident)?) => {
        $crate::channel::Decl::machine(
            ::core::stringify!($name),
            $crate::__mw_dir!($dir),
            $crate::channel::DataKind::$kind,
            $crate::__mw_required!($($flag)?),
        )
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __mw_dir {
    (in) => {
        $crate::channel::Direction::In
    };
    (out) => {
        $crate::channel::Direction::Out
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __mw_required {
    () => {
        true
    };
    (optional) => {
        false
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __mw_sensor {
    (imu) => {
        ::core::option::Option::Some($crate::channel::SensorKind::Imu)
    };
    (mic) => {
        ::core::option::Option::Some($crate::channel::SensorKind::Microphone)
    };
    (any) => {
        ::core::option::Option::None
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __mw_format {
    (i16) => {
        $crate::channel::SampleFormat::I16
    };
    (mulaw) => {
        $crate::channel::SampleFormat::MuLaw
    };
    (levels) => {
        $crate::channel::SampleFormat::Levels
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::wasm_abi::channels::Channels;

    crate::channels! {
        audio: in samples(mic, levels, 8000..=48000),
        accel: in samples(imu, 25..=100),
        cmd: in bytes optional,
        level: out bytes,
        frames_in: in frames optional,
    }

    struct Seen(u32);
    impl Input for Seen {
        fn on_data(&mut self, d: &[u8]) {
            self.0 += d.len() as u32;
        }
    }
    struct Rec(std::vec::Vec<u16>);
    impl SampleInput for Rec {
        fn on_batch(&mut self, b: &Batch<'_>) {
            self.0.push(b.header.count);
        }
    }

    #[test]
    fn ch6_the_macro_declares_and_encodes_the_section() {
        assert_eq!(CHANNELS.len(), 5);
        let a = CHANNELS[0];
        assert_eq!(
            (a.name, a.dir, a.sensor),
            ("audio", Direction::In, Some(SensorKind::Microphone))
        );
        assert_eq!(
            (a.formats, a.rate_min, a.rate_max, a.required),
            (1 << 2, 8000, 48000, true)
        );
        assert!(!CHANNELS[2].required);
        assert_eq!(CHANNELS[3].dir, Direction::Out);
        assert_eq!(CHANNELS[4].kind, DataKind::Frames);
        const LEN: usize = encoded_len(CHANNELS);
        let bytes = encode::<LEN>(CHANNELS);
        let parsed = Channels::parse(&bytes).unwrap();
        assert_eq!(
            parsed.iter().copied().collect::<std::vec::Vec<_>>(),
            CHANNELS
        );
    }

    #[test]
    fn ch6_handles_route_messages_and_outputs_refuse_inputs() {
        let input = Channel::from_handle(2);
        let slot = Slot::from_raw(2).unwrap();
        let other = Slot::from_raw(3).unwrap();
        let mut seen = Seen(0);
        assert!(input.deliver(slot, b"abc", &mut seen));
        assert!(!input.deliver(other, b"abc", &mut seen));
        assert_eq!(seen.0, 3);
        let mut out = Channel::from_handle(OUTPUT_BASE + 3);
        assert_eq!(out.slot(), None);
        assert!(!out.is(slot));
        // The host stand-in: no ABI 8 host.
        assert_eq!(out.publish(b"x"), Err(AbiError::Unsupported));
        let mut wrong = input;
        assert_eq!(wrong.publish(b"x"), Err(AbiError::InvalidArgument));
        assert_eq!(Channel::open("audio"), Err(AbiError::Unsupported));
        let mut rec = Rec(std::vec::Vec::new());
        assert!(input.deliver_samples(slot, b"junk", &mut rec).is_err());
        let h = middleware::stream::samples::BatchHeader {
            kind: SensorKind::Microphone,
            format: SampleFormat::I16,
            channels: 1,
            rate_hz: 16_000,
            first_index: 0,
            timestamp_us: 0,
            count: 2,
            range: 0,
            scale: 0,
        };
        let mut buf = [0u8; 64];
        let n = middleware::stream::samples::encode_i16(&h, &[1, 2], &mut buf).unwrap();
        assert_eq!(input.deliver_samples(slot, &buf[..n], &mut rec), Ok(true));
        assert_eq!(input.deliver_samples(other, &buf[..n], &mut rec), Ok(false));
        assert_eq!(rec.0, [2]);
    }
}

#[cfg(test)]
mod machine_tests {
    use super::*;
    use middleware::wasm_abi::channels::Channels;

    crate::channels! {
        x_limit: in digital,
        x_pos: in position,
        spindle: out setpoint optional,
        motion: out segments,
        queue: in segments,
    }

    #[test]
    fn mc3_the_macro_declares_machine_channels() {
        let kinds: std::vec::Vec<_> = CHANNELS.iter().map(|c| (c.name, c.dir, c.kind)).collect();
        assert_eq!(
            kinds,
            [
                ("x_limit", Direction::In, DataKind::Digital),
                ("x_pos", Direction::In, DataKind::Position),
                ("spindle", Direction::Out, DataKind::Setpoint),
                ("motion", Direction::Out, DataKind::Segments),
                ("queue", Direction::In, DataKind::Segments),
            ]
        );
        assert!(!CHANNELS[2].required);
        const LEN: usize = encoded_len(CHANNELS);
        let bytes = encode::<LEN>(CHANNELS);
        assert!(Channels::parse(&bytes).unwrap().iter().eq(CHANNELS.iter()));
        let s = machine::Segment {
            seq: 1,
            x_um: 0,
            z_um: -100,
            feed: 500,
            flags: 0,
        };
        assert_eq!(machine::Segment::decode(&s.encode()), Some(s));
    }
}
