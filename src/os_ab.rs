//! The Linux A/B root (design.md, "Linux A/B root (OS)"): the KD240's
//! squashfs store in two raw slots of a card, the boot state that says
//! which one boots, and the `sys/os/*` update protocol that writes the
//! inactive one.
//!
//! This module is the hardware-independent, `no_std` part:
//!
//! * [`bootstate`] — the two CRC-32 boot-state sectors, byte for byte what
//!   the KD240's `boot.scr` reads and writes (kd240/fleet/proto/src/
//!   bootstate.rs), and the U-Boot side modelled so a host test can run the
//!   trial and the rollback;
//! * [`wire`] — the `sys/os/*` request and response bodies and the signed
//!   message, in the same layout as `sys/fw/*` with its own key prefix and
//!   domain tag.
//!
//! The update machine, the slot devices and the record of the anti-rollback
//! floor are in the posix-node crate (`posix-node/src/os.rs`), which has
//! the files and the block devices.

pub mod bootstate;
pub mod wire;
