//! The ABI 5 LED-strip decisions: WS2812 pulse encoding, how many LEDs the
//! RMT channel can send in one go, the latch wait, and the strip table.
//!
//! The shell (`src/shell/ledstrip.rs`) owns RMT channel 0 and only moves
//! codes to the hardware; everything it decides comes from here, host
//! tested without a chip:
//!
//! * [`encode`] / [`encode_into`] turn a frame of G, R, B bytes into raw
//!   RMT pulse codes, one per bit, most significant bit first, with the
//!   WS2812 timing at [`RMT_HZ`] ([`T0H_NS`] / [`T0L_NS`] for a 0,
//!   [`T1H_NS`] / [`T1L_NS`] for a 1). The last code's low phase has
//!   length 0, which the RMT reads as the end marker; the line idles low,
//!   so that bit still ends correctly. A frame that does not fit the
//!   output is refused ([`EncodeError`]), nothing is truncated.
//! * [`capacity`] is the chain length one transmission can carry when the
//!   whole frame must sit in channel RAM (`blocks × words_per_block`
//!   codes, [`CODES_PER_LED`] per LED), so the CPU never refills the RAM
//!   while a radio interrupt could delay it.
//! * [`frame_us`] and [`deadline_us`] bound a transmission in time;
//!   [`latch_wait`] is how long to stay idle before a frame that follows
//!   the previous one too closely, at most
//!   [`RESET_US`](middleware::wasm_abi::ledstrip::RESET_US), wrapping safe.
//! * [`Strips`] is the payload's strip table: which [`StripHandle`] drives
//!   which [`LedStripSpec`], with [`Strips::open`] (reopening a pin keeps
//!   its handle and takes the new length), [`Strips::frame`] (the checks
//!   before a write) and [`Strips::close`] as functions returning a new
//!   table. The board's [`Limits`] decide how many strips and LEDs it
//!   drives. Errors follow the ABI: a further strip is `NoResources`, a
//!   chain longer than the board sends `Unsupported`, a handle that is not
//!   open `Busy`, a frame longer than its strip `InvalidArgument`.
//!
//! Pin checks and claims are not here: the device checks the pin against
//! the chip profile and [`crate::periph::claims`] first.

use middleware::wasm_abi::AbiError;
use middleware::wasm_abi::ledstrip::{BYTES_PER_LED, LedStripSpec, MAX_LEDS, MAX_LEDSTRIPS, RESET_US};

/// RMT tick rate the codes are timed for (80 MHz, 12.5 ns per tick).
pub const RMT_HZ: u32 = 80_000_000;

/// High time of a 0 bit, in nanoseconds.
pub const T0H_NS: u32 = 400;
/// Low time of a 0 bit, in nanoseconds.
pub const T0L_NS: u32 = 850;
/// High time of a 1 bit, in nanoseconds.
pub const T1H_NS: u32 = 800;
/// Low time of a 1 bit, in nanoseconds.
pub const T1L_NS: u32 = 450;

/// Pulse codes per LED: one per bit of its three bytes.
pub const CODES_PER_LED: usize = BYTES_PER_LED * 8;

/// Time on the wire per LED, in microseconds (24 bits of 1.25 µs).
pub const LED_US: u32 = 30;

/// Slack added to a frame's wire time for its deadline, in microseconds:
/// covers starting the channel and the poll loop's granularity.
pub const DEADLINE_MARGIN_US: u32 = 200;

/// RMT ticks for `ns` nanoseconds at [`RMT_HZ`] (all WS2812 phases are
/// whole ticks).
pub const fn ticks(ns: u32) -> u16 {
    (ns as u64 * RMT_HZ as u64 / 1_000_000_000) as u16
}

/// Bit 31 of a pulse code: level of the second phase.
const LEVEL2: u32 = 1 << 31;
/// Bit 15 of a pulse code: level of the first phase.
const LEVEL1: u32 = 1 << 15;

/// A raw RMT pulse code: bit 31 level 2, bits 30–16 length 2, bit 15
/// level 1, bits 14–0 length 1 (esp-hal's `PulseCode` layout).
const fn code(high1: bool, len1: u16, high2: bool, len2: u16) -> u32 {
    (if high2 { LEVEL2 } else { 0 })
        | (((len2 as u32) & 0x7FFF) << 16)
        | (if high1 { LEVEL1 } else { 0 })
        | ((len1 as u32) & 0x7FFF)
}

/// A 0 bit: high [`T0H_NS`], then low [`T0L_NS`].
pub const ZERO: u32 = code(true, ticks(T0H_NS), false, ticks(T0L_NS));
/// A 1 bit: high [`T1H_NS`], then low [`T1L_NS`].
pub const ONE: u32 = code(true, ticks(T1H_NS), false, ticks(T1L_NS));
/// A 0 bit ending the frame (low length 0, the end marker).
pub const ZERO_LAST: u32 = code(true, ticks(T0H_NS), false, 0);
/// A 1 bit ending the frame (low length 0, the end marker).
pub const ONE_LAST: u32 = code(true, ticks(T1H_NS), false, 0);

/// Why a frame was not encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// The frame has no bytes.
    Empty,
    /// The output holds fewer codes than the frame needs (8 per byte).
    TooLong,
}

/// Encodes `grb` (any bytes, sent in order, each most significant bit
/// first) into `out`, one code per bit, built by `make` from the raw code;
/// returns the number of codes written. The last code ends the frame
/// ([`ZERO_LAST`] / [`ONE_LAST`]). Codes past the returned count are left
/// as they were. Bounded: `8 × grb.len()` steps.
pub fn encode_into<T>(grb: &[u8], out: &mut [T], make: impl Fn(u32) -> T) -> Result<usize, EncodeError> {
    let n = grb.len().checked_mul(8).ok_or(EncodeError::TooLong)?;
    if n == 0 {
        return Err(EncodeError::Empty);
    }
    let out = out.get_mut(..n).ok_or(EncodeError::TooLong)?;
    let bits = grb.iter().flat_map(|&byte| (0..8u8).rev().map(move |b| byte & (1 << b) != 0));
    let last = n - 1;
    for (i, (slot, one)) in out.iter_mut().zip(bits).enumerate() {
        *slot = make(match (one, i == last) {
            (false, false) => ZERO,
            (true, false) => ONE,
            (false, true) => ZERO_LAST,
            (true, true) => ONE_LAST,
        });
    }
    Ok(n)
}

/// [`encode_into`] with raw `u32` codes.
pub fn encode(grb: &[u8], out: &mut [u32]) -> Result<usize, EncodeError> {
    encode_into(grb, out, |c| c)
}

/// LEDs one transmission carries from `blocks` RMT memory blocks of
/// `words_per_block` codes each, capped at [`MAX_LEDS`].
pub const fn capacity(blocks: usize, words_per_block: usize) -> usize {
    let leds = blocks.saturating_mul(words_per_block) / CODES_PER_LED;
    if leds < MAX_LEDS { leds } else { MAX_LEDS }
}

/// Wire time of a frame of `leds` LEDs, in microseconds.
pub const fn frame_us(leds: usize) -> u32 {
    let leds = if leds < MAX_LEDS { leds } else { MAX_LEDS };
    leds as u32 * LED_US
}

/// How long a transmission of `leds` LEDs may take before the shell gives
/// up on it: [`frame_us`] plus [`DEADLINE_MARGIN_US`].
pub const fn deadline_us(leds: usize) -> u32 {
    frame_us(leds) + DEADLINE_MARGIN_US
}

/// A microsecond timestamp from a wrapping 32-bit clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Micros(pub u32);

/// Microseconds to stay idle before the next frame so the chain latches
/// the previous one: `RESET_US` minus the time since `last_end` (when the
/// previous frame left the wire, `None` before the first frame), never
/// more than `RESET_US`. The clock may wrap; an interval is read modulo
/// 2³², so a gap of more than about 71 minutes may wait once more than
/// needed but never longer than `RESET_US`.
pub const fn latch_wait(last_end: Option<Micros>, now: Micros) -> u32 {
    match last_end {
        None => 0,
        Some(Micros(end)) => {
            let elapsed = now.0.wrapping_sub(end);
            RESET_US.saturating_sub(elapsed)
        }
    }
}

/// A strip handle, `0..MAX_LEDSTRIPS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct StripHandle(u8);

impl StripHandle {
    /// A handle from an ABI handle value; `Busy` outside `0..MAX_LEDSTRIPS`
    /// (no such strip is open).
    pub const fn new(h: u8) -> Result<Self, AbiError> {
        if (h as usize) < MAX_LEDSTRIPS { Ok(StripHandle(h)) } else { Err(AbiError::Busy) }
    }

    /// The handle value.
    pub const fn get(self) -> u8 {
        self.0
    }

    const fn index(self) -> usize {
        self.0 as usize
    }
}

/// What a board drives: how many strips at once and how long a chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    strips: u8,
    leds: u8,
}

impl Limits {
    /// `strips` strips (`1..=MAX_LEDSTRIPS`) of at most `leds` LEDs
    /// (`1..=MAX_LEDS`); `None` outside those ranges.
    pub const fn new(strips: usize, leds: usize) -> Option<Self> {
        if strips == 0 || strips > MAX_LEDSTRIPS || leds == 0 || leds > MAX_LEDS {
            None
        } else {
            Some(Limits { strips: strips as u8, leds: leds as u8 })
        }
    }

    /// Strips the board drives at once.
    pub const fn strips(self) -> usize {
        self.strips as usize
    }

    /// Longest chain the board sends in one go.
    pub const fn leds(self) -> usize {
        self.leds as usize
    }
}

/// The payload's open strips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Strips {
    limits: Limits,
    slots: [Option<LedStripSpec>; MAX_LEDSTRIPS],
}

impl Strips {
    /// No strip open, on a board with `limits`.
    pub const fn new(limits: Limits) -> Self {
        Strips { limits, slots: [None; MAX_LEDSTRIPS] }
    }

    /// The board's limits.
    pub const fn limits(&self) -> Limits {
        self.limits
    }

    /// The handle of the strip open on `pin`, if any.
    pub fn on_pin(&self, pin: u8) -> Option<StripHandle> {
        self.slots
            .iter()
            .position(|s| s.is_some_and(|s| s.pin == pin))
            .and_then(|i| u8::try_from(i).ok())
            .map(StripHandle)
    }

    /// The strip open as `handle`; `Busy` if none.
    pub fn get(&self, handle: StripHandle) -> Result<LedStripSpec, AbiError> {
        self.slots.get(handle.index()).copied().flatten().ok_or(AbiError::Busy)
    }

    /// Opens `spec`: on its pin's existing handle with the new length, or
    /// on the lowest free handle the board drives. `Unsupported` for a
    /// chain longer than [`Limits::leds`], `NoResources` when every strip
    /// the board drives is open on another pin. The table is unchanged on
    /// error.
    pub fn open(self, spec: LedStripSpec) -> Result<(Self, StripHandle), AbiError> {
        if spec.count.get() > self.limits.leds() {
            return Err(AbiError::Unsupported);
        }
        let handle = match self.on_pin(spec.pin) {
            Some(h) => h,
            None => self
                .slots
                .iter()
                .take(self.limits.strips())
                .position(Option::is_none)
                .and_then(|i| u8::try_from(i).ok())
                .map(StripHandle)
                .ok_or(AbiError::NoResources)?,
        };
        let mut slots = self.slots;
        if let Some(slot) = slots.get_mut(handle.index()) {
            *slot = Some(spec);
        }
        Ok((Strips { slots, ..self }, handle))
    }

    /// The checks before `ledstrip_write`: the strip open as `handle`
    /// (`Busy` if none) takes a frame of `len` bytes (`InvalidArgument`
    /// when it covers more LEDs than the strip has). Returns the strip.
    pub fn frame(&self, handle: StripHandle, len: usize) -> Result<LedStripSpec, AbiError> {
        let spec = self.get(handle)?;
        if len == 0 || !len.is_multiple_of(BYTES_PER_LED) {
            return Err(AbiError::InvalidArgument);
        }
        spec.check_frame(len)?;
        Ok(spec)
    }

    /// Closes `handle` and returns the strip it drove (its pin goes back
    /// to the payload); `Busy` if it is not open.
    pub fn close(self, handle: StripHandle) -> Result<(Self, LedStripSpec), AbiError> {
        let spec = self.get(handle)?;
        let mut slots = self.slots;
        if let Some(slot) = slots.get_mut(handle.index()) {
            *slot = None;
        }
        Ok((Strips { slots, ..self }, spec))
    }

    /// The open strips, by handle.
    pub fn open_strips(&self) -> impl Iterator<Item = (StripHandle, LedStripSpec)> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| Some((StripHandle(u8::try_from(i).ok()?), (*s)?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::wasm_abi::ledstrip::MAX_FRAME;

    fn spec(pin: i32, count: i32) -> LedStripSpec {
        LedStripSpec::parse(pin, count).expect("valid spec")
    }

    #[test]
    fn v3_timing_is_whole_ticks_at_80_mhz() {
        assert_eq!(RMT_HZ, 80_000_000);
        assert_eq!([ticks(T0H_NS), ticks(T0L_NS), ticks(T1H_NS), ticks(T1L_NS)], [32, 68, 64, 36]);
        // 12.5 ns per tick: each phase is exact.
        for ns in [T0H_NS, T0L_NS, T1H_NS, T1L_NS] {
            assert_eq!(u32::from(ticks(ns)) * 25, ns * 2);
        }
        // Both bits last 1.25 µs; 24 bits are LED_US.
        assert_eq!(T0H_NS + T0L_NS, 1250);
        assert_eq!(T1H_NS + T1L_NS, 1250);
        assert_eq!(1250 * CODES_PER_LED as u32 / 1000, LED_US);
    }

    #[test]
    fn v3_code_values_match_the_rmt_layout() {
        // level1 high (bit 15), length1 in bits 14-0, level2 low, length2
        // in bits 30-16.
        assert_eq!(ZERO, (68 << 16) | 0x8000 | 32);
        assert_eq!(ZERO, 0x0044_8020);
        assert_eq!(ONE, 0x0024_8040);
        assert_eq!(ZERO_LAST, 0x0000_8020);
        assert_eq!(ONE_LAST, 0x0000_8040);
        assert_eq!(code(false, 1, true, 2), 0x8002_0001);
    }

    #[test]
    fn v3_bytes_are_sent_msb_first_with_an_end_marker() {
        let mut out = [0u32; 16];
        assert_eq!(encode(&[0x80, 0x01], &mut out), Ok(16));
        let mut want = [ZERO; 16];
        want[0] = ONE;
        want[15] = ONE_LAST;
        assert_eq!(out, want);
        // Only the last code ends the frame.
        assert!(out[..15].iter().all(|c| c >> 16 & 0x7FFF != 0));
        assert_eq!(out[15] >> 16 & 0x7FFF, 0);

        let mut out = [0u32; 8];
        assert_eq!(encode(&[0x01], &mut out), Ok(8));
        assert_eq!(out, [ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ONE_LAST]);
        assert_eq!(encode(&[0x80], &mut out), Ok(8));
        assert_eq!(out, [ONE, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO_LAST]);
    }

    #[test]
    fn v3_a_whole_led_is_24_codes_in_grb_order() {
        let mut out = [0u32; CODES_PER_LED];
        assert_eq!(encode(&[0xFF, 0x00, 0xAA], &mut out), Ok(CODES_PER_LED));
        assert!(out[..8].iter().all(|&c| c == ONE));
        assert!(out[8..16].iter().all(|&c| c == ZERO));
        assert_eq!(&out[16..23], &[ONE, ZERO, ONE, ZERO, ONE, ZERO, ONE]);
        assert_eq!(out[23], ZERO_LAST);
    }

    #[test]
    fn v3_a_frame_that_does_not_fit_is_refused() {
        let mut out = [7u32; 15];
        assert_eq!(encode(&[0x80, 0x01], &mut out), Err(EncodeError::TooLong));
        assert_eq!(out, [7; 15], "nothing written");
        assert_eq!(encode(&[], &mut out), Err(EncodeError::Empty));
        // A larger buffer is fine; the tail is untouched.
        let mut out = [7u32; 10];
        assert_eq!(encode(&[0x00], &mut out), Ok(8));
        assert_eq!(&out[8..], &[7, 7]);
        // A whole MAX_LEDS frame fits MAX_LEDS × 24 codes.
        let mut big = [0u32; MAX_LEDS * CODES_PER_LED];
        assert_eq!(encode(&[0x55; MAX_FRAME], &mut big), Ok(MAX_LEDS * CODES_PER_LED));
    }

    #[test]
    fn v3_encode_into_builds_the_callers_code_type() {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        struct Code(u32);
        let mut out = [Code(0); 8];
        assert_eq!(encode_into(&[0x80], &mut out, Code), Ok(8));
        assert_eq!(out[0], Code(ONE));
        assert_eq!(out[7], Code(ZERO_LAST));
    }

    #[test]
    fn v4_capacity_is_what_fits_channel_ram() {
        // ESP32-S3 / C3: 48 words per block; ESP32: 64.
        assert_eq!(capacity(4, 48), 8);
        assert_eq!(capacity(4, 64), 10);
        assert_eq!(capacity(1, 48), 2);
        assert_eq!(capacity(8, 64), 21);
        assert_eq!(capacity(0, 64), 0);
        assert_eq!(capacity(100, 64), MAX_LEDS, "capped");
        assert_eq!(capacity(usize::MAX, usize::MAX), MAX_LEDS, "no overflow");
    }

    #[test]
    fn v3_deadline_covers_the_wire_time() {
        assert_eq!(frame_us(8), 240);
        assert_eq!(frame_us(1), 30);
        assert_eq!(frame_us(MAX_LEDS), 1920);
        assert_eq!(frame_us(usize::MAX), 1920, "capped");
        assert_eq!(deadline_us(8), 240 + DEADLINE_MARGIN_US);
    }

    #[test]
    fn v3_latch_wait_is_bounded_and_wrapping_safe() {
        assert_eq!(latch_wait(None, Micros(5)), 0, "first frame");
        assert_eq!(latch_wait(Some(Micros(1000)), Micros(1000)), RESET_US);
        assert_eq!(latch_wait(Some(Micros(1000)), Micros(1100)), RESET_US - 100);
        assert_eq!(latch_wait(Some(Micros(1000)), Micros(1000 + RESET_US)), 0);
        assert_eq!(latch_wait(Some(Micros(1000)), Micros(50_000)), 0);
        // Across the wrap.
        assert_eq!(latch_wait(Some(Micros(u32::MAX - 49)), Micros(50)), RESET_US - 100);
        assert_eq!(latch_wait(Some(Micros(u32::MAX - 49)), Micros(RESET_US)), 0);
        // Never more than RESET_US, whatever the inputs.
        for (end, now) in [(0, u32::MAX), (u32::MAX, 0), (12345, 12344), (7, 7)] {
            assert!(latch_wait(Some(Micros(end)), Micros(now)) <= RESET_US);
        }
    }

    #[test]
    fn v4_limits_are_validated() {
        assert!(Limits::new(1, 8).is_some());
        assert!(Limits::new(MAX_LEDSTRIPS, MAX_LEDS).is_some());
        assert_eq!(Limits::new(0, 8), None);
        assert_eq!(Limits::new(MAX_LEDSTRIPS + 1, 8), None);
        assert_eq!(Limits::new(1, 0), None);
        assert_eq!(Limits::new(1, MAX_LEDS + 1), None);
    }

    #[test]
    fn v4_handles_are_bounded() {
        assert_eq!(StripHandle::new(0).map(StripHandle::get), Ok(0));
        assert_eq!(StripHandle::new(MAX_LEDSTRIPS as u8), Err(AbiError::Busy));
    }

    #[test]
    fn v4_open_reopen_and_close() {
        let limits = Limits::new(1, 8).unwrap();
        let t = Strips::new(limits);
        let h0 = StripHandle::new(0).unwrap();
        assert_eq!(t.get(h0), Err(AbiError::Busy));
        let (t, h) = t.open(spec(45, 8)).unwrap();
        assert_eq!(h, h0);
        assert_eq!(t.get(h), Ok(spec(45, 8)));
        assert_eq!(t.on_pin(45), Some(h));
        // Reopening the pin keeps the handle and takes the new length.
        let (t, again) = t.open(spec(45, 3)).unwrap();
        assert_eq!(again, h);
        assert_eq!(t.get(h).map(|s| s.count.get()), Ok(3));
        // A second pin: the board drives one strip.
        assert_eq!(t.open(spec(4, 1)), Err(AbiError::NoResources));
        // Longer than the board sends: Unsupported, table unchanged.
        assert_eq!(t.open(spec(45, 9)), Err(AbiError::Unsupported));
        assert_eq!(t.get(h).map(|s| s.count.get()), Ok(3));
        let (t, closed) = t.close(h).unwrap();
        assert_eq!(closed, spec(45, 3));
        assert_eq!(t.close(h), Err(AbiError::Busy));
        assert_eq!(t.on_pin(45), None);
        // The handle is free again for another pin.
        let (t, h) = t.open(spec(4, 1)).unwrap();
        assert_eq!(h, h0);
        assert_eq!(t.open_strips().count(), 1);
    }

    #[test]
    fn v4_two_strip_boards_hand_out_two_handles() {
        let t = Strips::new(Limits::new(2, MAX_LEDS).unwrap());
        let (t, a) = t.open(spec(1, 64)).unwrap();
        let (t, b) = t.open(spec(2, 1)).unwrap();
        assert_eq!((a.get(), b.get()), (0, 1));
        assert_eq!(t.open(spec(3, 1)), Err(AbiError::NoResources));
        let (t, _) = t.close(a).unwrap();
        let (t, c) = t.open(spec(3, 1)).unwrap();
        assert_eq!(c, a, "lowest free handle");
        assert_eq!(
            t.open_strips().map(|(h, s)| (h.get(), s.pin)).collect::<heapless::Vec<_, 2>>(),
            [(0, 3), (1, 2)]
        );
    }

    #[test]
    fn v4_frames_are_checked_against_the_strip() {
        let (t, h) = Strips::new(Limits::new(1, 8).unwrap()).open(spec(45, 4)).unwrap();
        assert_eq!(t.frame(h, 12), Ok(spec(45, 4)));
        assert_eq!(t.frame(h, 3), Ok(spec(45, 4)), "a shorter frame updates the first LEDs");
        assert_eq!(t.frame(h, 15), Err(AbiError::InvalidArgument), "longer than the strip");
        assert_eq!(t.frame(h, 0), Err(AbiError::InvalidArgument));
        assert_eq!(t.frame(h, 4), Err(AbiError::InvalidArgument));
        let (t, _) = t.close(h).unwrap();
        assert_eq!(t.frame(h, 12), Err(AbiError::Busy));
    }
}
