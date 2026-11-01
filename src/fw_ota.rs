//! A/B firmware over-the-air update for the RP2350 (Raspberry Pi Pico 2),
//! over the serial loader framing (USB CDC with `mwctl fw`).
//!
//! The RP2350 bootrom does the hard part (RP2350 datasheet §5.1 "Bootrom
//! concepts": partition tables, A/B partitions, versions, try before you
//! buy; §5.4 "Bootrom APIs": `get_sys_info`, `reboot`, `explicit_buy`,
//! `pick_ab_partition`; §5.9 "Metadata block details"). Two partitions of
//! the partition table are linked as an A/B pair; the bootrom boots the
//! valid image with the higher version. A new image is written into the
//! partition that is *not* running, flagged try-before-you-buy (TBYB), and
//! booted once with a flash-update reboot; unless the new firmware calls
//! `explicit_buy` within the bootrom's watchdog window, the next boot
//! skips it and the previous image runs again. (The Pico 1's RP2040 bootrom
//! has none of this; it is out of scope by design.)
//!
//! This module is the hardware-independent part, always built, `no_std`
//! and allocation free:
//!
//! * [`wire`] — `sys/fw/*` request and response bodies and the signed
//!   message;
//! * [`machine`] — the pure state machine [`FwUpdate`];
//! * [`picobin`] — finds an image's IMAGE_DEF and version in its metadata
//!   blocks (host and device);
//! * [`record`] — the persisted anti-rollback floor and update in flight;
//! * [`platform`] — the [`FwPlatform`] trait the RP2350 integration
//!   implements;
//! * `service` (feature `trust`) — `FwService`, the shell that runs a
//!   request against a platform (SHA-256 and Ed25519 need `trust`);
//! * `sim` (feature `fw-sim`, host only) — an in-memory bootrom and flash
//!   model implementing [`FwPlatform`], for host tests.
//!
//! # Protocol
//!
//! Requests on the existing serial framing (`serial::frame`); all
//! integers little-endian.
//!
//! | key              | request body                               | response body |
//! |------------------|--------------------------------------------|---------------|
//! | `sys/fw/info`    | empty                                      | [`wire::FwInfo`] (36 bytes) |
//! | `sys/fw/begin`   | `[len u32][version u32][sha256 32]`        | `[slot u8]` (0 A, 1 B) |
//! | `sys/fw/chunk`   | `[offset u32][1..=512 bytes]`              | empty |
//! | `sys/fw/commit`  | empty, or `[signature 64]`                 | empty |
//! | `sys/fw/reboot`  | empty                                      | empty; the device resets ~100 ms later |
//! | `sys/fw/confirm` | empty                                      | empty |
//! | `sys/fw/abort`   | empty                                      | empty |
//!
//! * `begin` targets the inactive slot (the device chooses and answers it).
//!   `version` is the image's IMAGE_DEF `major << 16 | minor` and must
//!   exceed both the persisted `min_version` and the running image's
//!   version (`Denied`); `len` must fit the slot (`BufferTooSmall`). The
//!   same `begin` again resumes the transfer (a resent begin, or a host
//!   that reconnected); any other begin restarts it.
//! * `chunk`: `offset` a multiple of [`CHUNK_LEN`], `data` exactly
//!   `min(512, len - offset)` bytes, in any order; anything else (an
//!   overlapping, misaligned or out-of-range chunk) is `InvalidRequest`. A
//!   chunk already received is acknowledged and not written again.
//!   512-byte chunks fit the serial frame (1019 bytes of key and body).
//! * `commit` once every chunk arrived: on a claimed device the
//!   signature is required (`Denied` otherwise), Ed25519 by the pinned
//!   payload key ([`crate::loader::trust`]) over the 64-byte
//!   [`wire::signed_message`]
//!   `b"mw-firmware-v1\0" || len u32 || version u32 || slot u8 || node id 8 || sha256 32`,
//!   so a signature is good for one image, one version, one slot of one
//!   device. An unclaimed device accepts any commit, signed or not (trust
//!   on first use, as for payloads). The device then hashes what it
//!   received and reads the image's IMAGE_DEF: the hash must match, and
//!   the image must be an RP2350 executable flagged TBYB, without a
//!   rollback version, whose version equals the declared one and whose
//!   block loop has a block in the first sector's last page
//!   ([`picobin::SENTINEL_FROM`], so a power cut during the bootrom's buy
//!   cannot leave a torn image bootable) (`InvalidRequest`, the transfer
//!   is abandoned). It persists
//!   `min_version = version` with the pending update, writes the image's
//!   first sector, and is `Committed`.
//! * `reboot` (only when committed): flash-update reboot into the new slot.
//! * `confirm` (only on a trial boot, `Unavailable` until the firmware's
//!   self-test passed): explicit buy; the new image is permanent.
//! * `abort`: abandons a transfer or a committed update (erasing the new
//!   slot's first sector), or rejects a trial (normal reboot into the
//!   previous image). Idempotent.
//!
//! Every request is idempotent on resend (the serial host resends after a
//! timeout with a new correlation id), and a refused request leaves the
//! state unchanged except a commit whose image fails verification.
//!
//! # State machine
//!
//! ```text
//!  Idle ──begin──▶ Receiving ──chunk*──▶ Receiving(complete) ──commit──▶ Verified ──(install)──▶ Committed
//!   ▲  ◀──abort───────┘  ▲──begin (restart/resume)                                  │
//!   │                                                                              reboot
//!   │                                                                                ▼
//!   │                         RolledBack ◀──watchdog/reset/abort (no buy)── Trial (TBYB boot)
//!   │                                                                                │ confirm
//!   └──────────────────────────── next boot ◀────────────────────────────── Confirmed ◀┘
//! ```
//!
//! `Verified` is transient: the image is checked and authorized but
//! installing it (persist, first-sector write) failed; another commit
//! retries. `Trial`, `Confirmed` (until the record is settled) and
//! `RolledBack` are entered at boot from the bootrom's boot info and the
//! record ([`FwUpdate::boot`]).
//!
//! # Where the image goes, and power loss
//!
//! Flash is erased in [`SECTOR`] (4 KiB) units just before a sector's
//! first chunk is written. Sector 0 holds the image's first metadata block
//! (the bootrom looks for it in the first 4 KiB): it is erased at `begin`,
//! its chunks are kept in RAM, and it is written only by a commit that
//! passed every check, after the record is persisted. So **the inactive
//! slot never holds a bootable image that was not verified and
//! authorized**, and the running slot is never erased or written.
//!
//! Every state is recoverable from flash and the bootrom (checked by the
//! power-loss tests of the `fw-sim` model at every operation):
//!
//! | power lost during / after             | next boot                                   |
//! |---------------------------------------|---------------------------------------------|
//! | `begin`, chunks, `commit` before the record is saved | old image; record has nothing pending → `Idle` (start over) |
//! | after the record is saved, before or while sector 0 is written | old image; pending, not rebooted, slot not intact → `Idle`, slot's sector 0 erased, `min_version` stays raised |
//! | after sector 0, before `reboot`       | old image; pending, slot intact → `Committed` (reboot again) |
//! | `reboot` requested, trial running, or a trial reset without a buy | old image (the bootrom skips the unbought TBYB image) → `RolledBack` |
//! | during `confirm`'s buy (first sector being rewritten) | old image (the new one's first sector is torn) → `RolledBack` |
//! | after the buy, before the record is settled | new image → `Confirmed`, record settled |
//!
//! A commit's version is used up once its record is saved, even if the
//! update does not complete: the next attempt needs a higher version, so a
//! captured signed commit can never be replayed.

pub mod machine;
pub mod picobin;
pub mod platform;
pub mod record;
#[cfg(feature = "trust")]
pub mod service;
#[cfg(feature = "fw-sim")]
pub mod sim;
pub mod wire;

pub use machine::{Effect, Env, Facts, FwUpdate};
pub use platform::{BootInfo, FwPlatform};
pub use record::FwRecord;
pub use wire::{Command, FwInfo, StateCode, keys};

/// Largest `sys/fw/chunk` data.
pub const CHUNK_LEN: usize = 512;
/// Flash erase unit.
pub const SECTOR: u32 = 4096;
/// Chunks per sector.
pub const CHUNKS_PER_SECTOR: u32 = SECTOR / CHUNK_LEN as u32;
/// Largest image accepted (4 MiB, the Pico 2's whole flash).
pub const MAX_IMAGE: u32 = 4 * 1024 * 1024;
/// Chunks of the largest image (the size of [`machine::ChunkSet`]).
pub const MAX_CHUNKS: usize = MAX_IMAGE as usize / CHUNK_LEN;

/// One partition of the A/B pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Slot {
    /// The A partition (the pair's first).
    A = 0,
    /// The B partition (linked to A).
    B = 1,
}

impl Slot {
    /// The other slot of the pair.
    pub const fn other(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }

    /// Parses the wire byte.
    pub const fn from_byte(b: u8) -> Option<Slot> {
        match b {
            0 => Some(Slot::A),
            1 => Some(Slot::B),
            _ => None,
        }
    }

    /// `"A"` or `"B"`.
    pub const fn name(self) -> &'static str {
        match self {
            Slot::A => "A",
            Slot::B => "B",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn y1_slots_and_sizes() {
        assert_eq!(Slot::A.other(), Slot::B);
        assert_eq!(Slot::B.other(), Slot::A);
        assert_eq!(Slot::from_byte(1), Some(Slot::B));
        assert_eq!(Slot::from_byte(2), None);
        assert_eq!((Slot::A.name(), Slot::B.name()), ("A", "B"));
        assert_eq!(CHUNKS_PER_SECTOR, 8);
        // A chunk request fits one serial frame: key + offset + data within
        // the shared key/body limit.
        let body_room = crate::serial::frame::MAX_RAW_FRAME - crate::serial::frame::FRAME_OVERHEAD;
        assert!(keys::CHUNK.as_str().len() + 4 + CHUNK_LEN <= body_room);
    }
}
