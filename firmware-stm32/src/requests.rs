//! Request routing: the system keys, and applications beside them.

use middleware::SenderId;
use middleware::loader::{Caps, DeviceInfo, PayloadStatus};
use middleware::transport::{Handler, HandlerError, Request};
use middleware::wasm_abi::ABI_VERSION;

use crate::board::{CHIP, MANIFEST, board_info};

/// What `sys/info` says besides the chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity<'a> {
    /// Firmware version string (an application names itself here).
    pub firmware: &'a str,
    /// The node id ([`crate::board::node_id`] of the chip's UID).
    pub node: SenderId,
}

/// Answers `sys/info`, `sys/board` and `sys/manifest` (empty bodies);
/// every other key is `NotFound`, a body on a system key `InvalidRequest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct System<'a> {
    identity: Identity<'a>,
}

impl<'a> System<'a> {
    /// The system handler for `identity`.
    pub const fn new(identity: Identity<'a>) -> Self {
        Self { identity }
    }

    /// The `sys/info` record.
    pub const fn info(&self) -> DeviceInfo<'a> {
        DeviceInfo {
            abi_version: ABI_VERSION,
            capacity: 0,
            status: PayloadStatus::Empty,
            chip: CHIP,
            firmware: self.identity.firmware,
            node: Some(self.identity.node),
            caps: Caps::NONE,
        }
    }
}

impl Handler for System<'_> {
    fn handle(&mut self, req: &Request<'_>, out: &mut [u8]) -> Result<usize, HandlerError> {
        let encoded = match req.key.as_str() {
            "sys/info" => self.info().encode(out),
            "sys/board" => board_info().encode(out),
            "sys/manifest" => MANIFEST.encode(out),
            _ => return Err(HandlerError::NotFound),
        };
        if !req.body.is_empty() {
            return Err(HandlerError::InvalidRequest);
        }
        encoded.ok_or(HandlerError::BufferTooSmall)
    }
}

/// An application's handler beside the system keys: keys under `sys/` go
/// to [`System`] and never reach the application; every other key goes to
/// the application.
#[derive(Debug)]
pub struct Node<'a, A> {
    /// The system keys.
    pub system: System<'a>,
    /// The application's keys.
    pub app: A,
}

impl<A: Handler> Handler for Node<'_, A> {
    fn handle(&mut self, req: &Request<'_>, out: &mut [u8]) -> Result<usize, HandlerError> {
        if is_system(req.key.as_str()) {
            self.system.handle(req, out)
        } else {
            self.app.handle(req, out)
        }
    }
}

/// Whether `key` is under `sys` (segment-wise: `sys` and `sys/x`, never
/// `system`).
pub fn is_system(key: &str) -> bool {
    key == "sys" || key.starts_with("sys/")
}

/// The application of a bare base firmware: no keys of its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoApp;

impl Handler for NoApp {
    fn handle(&mut self, _: &Request<'_>, _: &mut [u8]) -> Result<usize, HandlerError> {
        Err(HandlerError::NotFound)
    }
}

#[cfg(test)]
mod tests;
