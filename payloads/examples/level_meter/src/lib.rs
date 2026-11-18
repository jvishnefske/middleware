//! Level meter (ABI 7 channels): the loudest block of each microphone
//! level batch, published on an output channel.
//!
//! The payload names no key and no route. It declares two channels:
//!
//! * `audio: in samples(mic, levels, 8000..=48000)`: microphone level
//!   batches (RMS and peak per block) at any audio rate;
//! * `level: out bytes optional`: one text message per batch,
//!   `rms <R> peak <P> blocks <N> from <I>`: the largest RMS and peak of the
//!   batch's `N` blocks (`i16` full scale, 0..=32768) and its first block
//!   index (a gap between messages is lost audio).
//!
//! A binding decides where `audio` comes from: the board's own microphone
//! levels route, a PCM route through the runtime's levels adapter, or
//! another node's `mic/level` key. The metering itself is [`Meter`],
//! generic over [`Output`] so the host tests drive it with a recorder.
#![no_std]
#![forbid(unsafe_code)]

use mw_payload::channel::{Batch, Channel, Output, SampleFormat, SampleInput};
use mw_payload::{LogLine, Payload, Slot};

mw_payload::channels! {
    audio: in samples(mic, levels, 8000..=48000),
    level: out bytes optional,
}

/// The metering logic: one message per level batch on `out`.
pub struct Meter<O: Output> {
    out: Option<O>,
    /// Batches seen.
    pub batches: u32,
    /// The loudest RMS seen so far.
    pub loudest: u16,
}

impl<O: Output> Meter<O> {
    /// A meter publishing on `out` (or only counting, without one).
    pub const fn new(out: Option<O>) -> Self {
        Meter { out, batches: 0, loudest: 0 }
    }

    /// The output, for tests.
    pub fn output(&self) -> Option<&O> {
        self.out.as_ref()
    }
}

/// `rms <R> peak <P> blocks <N> from <I>` into `buf`; its length.
pub fn format_level(buf: &mut [u8; 64], rms: u16, peak: u16, blocks: u16, from: u64) -> usize {
    let mut at = 0;
    let mut put = |s: &[u8]| {
        for &b in s {
            if at < buf.len() {
                buf[at] = b;
                at += 1;
            }
        }
    };
    let num = |v: u64, put: &mut dyn FnMut(&[u8])| {
        let mut d = [0u8; 20];
        let mut i = d.len();
        let mut v = v;
        loop {
            i -= 1;
            d[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        put(&d[i..]);
    };
    put(b"rms ");
    num(rms.into(), &mut put);
    put(b" peak ");
    num(peak.into(), &mut put);
    put(b" blocks ");
    num(blocks.into(), &mut put);
    put(b" from ");
    num(from, &mut put);
    at
}

impl<O: Output> SampleInput for Meter<O> {
    fn on_batch(&mut self, b: &Batch<'_>) {
        if b.header.format != SampleFormat::Levels {
            return;
        }
        let (rms, peak) = b.levels().fold((0, 0), |(r, p), (rr, pp)| (r.max(rr), p.max(pp)));
        self.batches = self.batches.wrapping_add(1);
        self.loudest = self.loudest.max(rms);
        if let Some(out) = self.out.as_mut() {
            let mut buf = [0u8; 64];
            let n = format_level(&mut buf, rms, peak, b.header.count, b.header.first_index);
            let _ = out.publish(&buf[..n]);
        }
    }
}

/// The payload: the opened channels and the meter.
pub struct LevelMeter {
    audio: Option<Channel>,
    meter: Meter<Channel>,
}

fn opened(name: &str) -> Option<Channel> {
    match Channel::open(name) {
        Ok(c) => Some(c),
        Err(e) => {
            let mut l = LogLine::new();
            l.push_str("level_meter: ").push_str(name).push_str(" not bound (").push_u32(e.code().unsigned_abs()).push_str(")");
            l.log();
            None
        }
    }
}

impl Payload for LevelMeter {
    fn init() -> Self {
        let audio = opened("audio");
        let level = opened("level");
        mw_payload::log("level_meter: started");
        LevelMeter { audio, meter: Meter::new(level) }
    }

    fn on_message(&mut self, slot: Slot, data: &[u8]) {
        if let Some(a) = self.audio {
            let _ = a.deliver_samples(slot, data, &mut self.meter);
        }
    }
}

#[cfg(target_arch = "wasm32")]
mw_payload::payload!(LevelMeter);

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use mw_payload::AbiError;
    use mw_payload::channel::{DataKind, Direction, SensorKind};
    use std::vec::Vec;

    struct Rec(Vec<Vec<u8>>);
    impl Output for Rec {
        fn publish(&mut self, data: &[u8]) -> Result<(), AbiError> {
            self.0.push(data.to_vec());
            Ok(())
        }
    }

    fn levels(first: u64, pairs: &[(u16, u16)]) -> Vec<u8> {
        let h = middleware_header(first, pairs.len() as u16);
        let data: Vec<u8> = pairs.iter().flat_map(|(r, p)| [r.to_le_bytes(), p.to_le_bytes()].concat()).collect();
        let mut out = [0u8; 512];
        let n = mw_payload::channel::encode_batch(&h, &data, &mut out).unwrap();
        out[..n].to_vec()
    }

    fn middleware_header(first: u64, count: u16) -> mw_payload::channel::BatchHeader {
        mw_payload::channel::BatchHeader {
            kind: SensorKind::Microphone,
            format: SampleFormat::Levels,
            channels: 1,
            rate_hz: 16_000,
            first_index: first,
            timestamp_us: 0,
            count,
            range: 20,
            scale: 160,
        }
    }

    #[test]
    fn declares_a_levels_input_and_an_optional_output() {
        assert_eq!(CHANNELS.len(), 2);
        let a = CHANNELS[0];
        assert_eq!((a.name, a.dir, a.kind, a.sensor), ("audio", Direction::In, DataKind::Samples, Some(SensorKind::Microphone)));
        assert!(a.required && !CHANNELS[1].required);
    }

    #[test]
    fn publishes_the_loudest_block_of_each_batch() {
        let mut m = Meter::new(Some(Rec(Vec::new())));
        let input = Channel::from_handle(0);
        let slot = Slot::from_raw(0).unwrap();
        assert_eq!(input.deliver_samples(slot, &levels(30, &[(100, 900), (2500, 7000), (40, 50)]), &mut m), Ok(true));
        assert_eq!(input.deliver_samples(slot, &levels(33, &[(10, 20)]), &mut m), Ok(true));
        let msgs: Vec<_> = m.output().unwrap().0.iter().map(|v| std::string::String::from_utf8(v.clone()).unwrap()).collect();
        assert_eq!(msgs, ["rms 2500 peak 7000 blocks 3 from 30", "rms 10 peak 20 blocks 1 from 33"]);
        assert_eq!((m.batches, m.loudest), (2, 2500));
        // Without the optional output it only counts.
        let mut quiet: Meter<Rec> = Meter::new(None);
        input.deliver_samples(slot, &levels(0, &[(5, 6)]), &mut quiet).unwrap();
        assert_eq!(quiet.batches, 1);
    }
}
