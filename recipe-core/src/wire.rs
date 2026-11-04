//! The phase command contract on the middleware: keys and fixed binary
//! bodies, encoded into caller buffers.
//!
//! A unit serves each of its equipment phases on one request key and
//! publishes the phase's state on another (`<node>` is the unit's 8-byte
//! node id, the middleware `SenderId` that `sys/info` reports, as 16
//! lowercase hex digits; `<em>` and `<phase>` are identifiers):
//!
//! | key                                   | kind              | body |
//! |---------------------------------------|-------------------|------|
//! | `unit/<node>/phase/<em>.<phase>`      | request / reply   | [`encode_command`] / [`encode_ack`] |
//! | `unit/<node>/phase/<em>.<phase>/state`| publish           | [`encode_report`] |
//!
//! The engine sends each [`crate::engine::Effect::Send`] with
//! `Requester::request_with(key, body, buf, &RequestOptions::DEFAULT
//! .with_timeout(timeout_ms).with_target(Target::Node(node)))`: the
//! timeout bounds the call and reaches the unit as
//! `RequestMeta::deadline_ms`, and `Target::Node` keeps a backend that can
//! address nodes from asking anyone else (the key names the node as well,
//! so a key-routed backend reaches the same unit). The reply is the
//! acknowledgement; a unit's `HandlerError` maps to
//! [`crate::engine::AckError`] (`NotFound` → `NotFound`, `InvalidRequest`
//! → `InvalidRequest`, `Unavailable` → `Unavailable`, `DeadlineExceeded`
//! → `DeadlineExceeded`, anything else and transport errors →
//! `Transport`). A unit publishes a report on every state change and as a
//! heartbeat; a report naming a command's id acknowledges it too, so a
//! unit that can only publish still completes the handshake.
//!
//! Bodies start with [`VERSION`]; integers are little-endian:
//!
//! * command: `version, command code (1..=6), id: u32, n: u8, n × i32`
//!   (`n` = 0 except for Start, which carries the step's arguments in the
//!   phase class's parameter order, at most [`MAX_PARAMS`]);
//! * acknowledgement: `version, 0 accepted | 1 rejected, state code, id:
//!   u32` (the state after the command, or the unchanged state);
//! * report: `version, state code, last applied id: u32, n: u8, n × i32`
//!   (`n` result values, e.g. a Measure phase's reading; at most
//!   [`MAX_PARAMS`]).
//!
//! [`serve`] is a unit's side of a command as a pure function: decode,
//! check Start's arguments against the phase's ranges, step the phase
//! table, and answer.

use crate::engine::Ack;
use crate::phase::{self, Command, PhaseState};
use crate::{MAX_PARAMS, ident_ok};

/// Body format version.
pub const VERSION: u8 = 1;
/// The discovery service kind units announce (instance = unit id; TXT
/// `class=<unit class>`).
pub const SERVICE_KIND: &str = "s88/unit";
/// Longest key [`command_key`] / [`state_key`] produce (two 24-byte
/// identifiers).
pub const MAX_KEY: usize = 5 + 16 + 7 + 24 + 1 + 24 + 6;
/// Longest command or report body.
pub const MAX_BODY: usize = 7 + 4 * MAX_PARAMS;
/// Length of an acknowledgement.
pub const ACK_LEN: usize = 7;

/// Why encoding or decoding failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// The output buffer is too small.
    BufferTooSmall,
    /// An identifier is not one, or a key does not have this shape.
    BadKey,
    /// The body is too short, too long, of another version or has an
    /// unknown code.
    BadBody,
    /// More than [`MAX_PARAMS`] values.
    TooManyValues,
}

/// A decoded phase key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhaseKey<'a> {
    /// The unit's node id.
    pub node: [u8; 8],
    /// The equipment module.
    pub em: &'a str,
    /// The phase.
    pub phase: &'a str,
    /// True for the `/state` key.
    pub state: bool,
}

/// A decoded command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandBody {
    /// The command.
    pub command: Command,
    /// Its id.
    pub id: u32,
    /// Start's arguments (the first `n`).
    pub args: [i32; MAX_PARAMS],
    /// Number of arguments.
    pub n: usize,
}

/// A decoded state report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// The phase's state.
    pub state: PhaseState,
    /// The last command id it applied.
    pub last_id: u32,
    /// Result values (the first `n`).
    pub values: [i32; MAX_PARAMS],
    /// Number of values.
    pub n: usize,
}

struct Writer<'b> {
    buf: &'b mut [u8],
    len: usize,
}

impl<'b> Writer<'b> {
    fn new(buf: &'b mut [u8]) -> Self {
        Writer { buf, len: 0 }
    }

    fn bytes(&mut self, b: &[u8]) -> Result<(), WireError> {
        let end = self.len + b.len();
        self.buf
            .get_mut(self.len..end)
            .ok_or(WireError::BufferTooSmall)?
            .copy_from_slice(b);
        self.len = end;
        Ok(())
    }

    fn done(self) -> &'b [u8] {
        &self.buf[..self.len]
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn key<'b>(
    node: [u8; 8],
    em: &str,
    phase: &str,
    state: bool,
    buf: &'b mut [u8],
) -> Result<&'b str, WireError> {
    if !ident_ok(em) || !ident_ok(phase) {
        return Err(WireError::BadKey);
    }
    let mut w = Writer::new(buf);
    w.bytes(b"unit/")?;
    for b in node {
        w.bytes(&[HEX[usize::from(b >> 4)], HEX[usize::from(b & 15)]])?;
    }
    w.bytes(b"/phase/")?;
    w.bytes(em.as_bytes())?;
    w.bytes(b".")?;
    w.bytes(phase.as_bytes())?;
    if state {
        w.bytes(b"/state")?;
    }
    core::str::from_utf8(w.done()).map_err(|_| WireError::BadKey)
}

/// `unit/<node>/phase/<em>.<phase>`, the phase's request key.
pub fn command_key<'b>(
    node: [u8; 8],
    em: &str,
    phase: &str,
    buf: &'b mut [u8],
) -> Result<&'b str, WireError> {
    key(node, em, phase, false, buf)
}

/// `unit/<node>/phase/<em>.<phase>/state`, the phase's report key.
pub fn state_key<'b>(
    node: [u8; 8],
    em: &str,
    phase: &str,
    buf: &'b mut [u8],
) -> Result<&'b str, WireError> {
    key(node, em, phase, true, buf)
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

/// Splits a [`command_key`] or [`state_key`] back into its parts.
pub fn parse_key(k: &str) -> Result<PhaseKey<'_>, WireError> {
    let mut parts = k.split('/');
    let (Some("unit"), Some(node), Some("phase"), Some(name)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(WireError::BadKey);
    };
    let state = match parts.next() {
        None => false,
        Some("state") => true,
        Some(_) => return Err(WireError::BadKey),
    };
    if parts.next().is_some() || node.len() != 16 {
        return Err(WireError::BadKey);
    }
    let mut id = [0u8; 8];
    for (i, pair) in node.as_bytes().chunks(2).enumerate() {
        let hi = hex_digit(pair[0]).ok_or(WireError::BadKey)?;
        let lo = hex_digit(pair[1]).ok_or(WireError::BadKey)?;
        id[i] = hi << 4 | lo;
    }
    let (em, phase) = name.split_once('.').ok_or(WireError::BadKey)?;
    if !ident_ok(em) || !ident_ok(phase) {
        return Err(WireError::BadKey);
    }
    Ok(PhaseKey {
        node: id,
        em,
        phase,
        state,
    })
}

fn values(w: &mut Writer<'_>, v: &[i32]) -> Result<(), WireError> {
    if v.len() > MAX_PARAMS {
        return Err(WireError::TooManyValues);
    }
    w.bytes(&[v.len() as u8])?;
    for x in v {
        w.bytes(&x.to_le_bytes())?;
    }
    Ok(())
}

fn read_values(body: &[u8]) -> Result<([i32; MAX_PARAMS], usize), WireError> {
    let (&n, rest) = body.split_first().ok_or(WireError::BadBody)?;
    let n = usize::from(n);
    if n > MAX_PARAMS || rest.len() != 4 * n {
        return Err(WireError::BadBody);
    }
    let mut out = [0i32; MAX_PARAMS];
    for (slot, chunk) in out.iter_mut().zip(rest.chunks_exact(4)) {
        *slot = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    Ok((out, n))
}

fn read_u32(b: &[u8]) -> Result<u32, WireError> {
    match b {
        [a, b, c, d, ..] => Ok(u32::from_le_bytes([*a, *b, *c, *d])),
        _ => Err(WireError::BadBody),
    }
}

/// A command body; `args` only for Start (others must pass none).
pub fn encode_command<'b>(
    command: Command,
    id: u32,
    args: &[i32],
    buf: &'b mut [u8],
) -> Result<&'b [u8], WireError> {
    if command != Command::Start && !args.is_empty() {
        return Err(WireError::TooManyValues);
    }
    let mut w = Writer::new(buf);
    w.bytes(&[VERSION, command.code()])?;
    w.bytes(&id.to_le_bytes())?;
    values(&mut w, args)?;
    Ok(w.done())
}

/// Decodes a command body.
pub fn decode_command(body: &[u8]) -> Result<CommandBody, WireError> {
    let [VERSION, code, rest @ ..] = body else {
        return Err(WireError::BadBody);
    };
    let command = Command::from_code(*code).ok_or(WireError::BadBody)?;
    let id = read_u32(rest)?;
    let (args, n) = read_values(&rest[4..])?;
    if command != Command::Start && n != 0 {
        return Err(WireError::BadBody);
    }
    Ok(CommandBody {
        command,
        id,
        args,
        n,
    })
}

/// An acknowledgement body.
pub fn encode_ack(
    accepted: bool,
    state: PhaseState,
    id: u32,
    buf: &mut [u8],
) -> Result<&[u8], WireError> {
    let mut w = Writer::new(buf);
    w.bytes(&[VERSION, u8::from(!accepted), state.code()])?;
    w.bytes(&id.to_le_bytes())?;
    Ok(w.done())
}

/// Decodes an acknowledgement: the command id and the engine's [`Ack`].
pub fn decode_ack(body: &[u8]) -> Result<(u32, Ack), WireError> {
    let [VERSION, verdict, state, rest @ ..] = body else {
        return Err(WireError::BadBody);
    };
    let state = PhaseState::from_code(*state).ok_or(WireError::BadBody)?;
    if rest.len() != 4 {
        return Err(WireError::BadBody);
    }
    let id = read_u32(rest)?;
    match verdict {
        0 => Ok((id, Ack::Accepted(state))),
        1 => Ok((id, Ack::Rejected(state))),
        _ => Err(WireError::BadBody),
    }
}

/// A state report body.
pub fn encode_report<'b>(
    state: PhaseState,
    last_id: u32,
    vals: &[i32],
    buf: &'b mut [u8],
) -> Result<&'b [u8], WireError> {
    let mut w = Writer::new(buf);
    w.bytes(&[VERSION, state.code()])?;
    w.bytes(&last_id.to_le_bytes())?;
    values(&mut w, vals)?;
    Ok(w.done())
}

/// Decodes a state report.
pub fn decode_report(body: &[u8]) -> Result<Report, WireError> {
    let [VERSION, state, rest @ ..] = body else {
        return Err(WireError::BadBody);
    };
    let state = PhaseState::from_code(*state).ok_or(WireError::BadBody)?;
    let last_id = read_u32(rest)?;
    let (values, n) = read_values(&rest[4..])?;
    Ok(Report {
        state,
        last_id,
        values,
        n,
    })
}

/// Why [`serve`] refused a command (the unit answers with this
/// `HandlerError`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeError {
    /// Malformed body, wrong argument count, or an argument outside the
    /// phase's range: `HandlerError::InvalidRequest`.
    InvalidRequest,
}

/// What [`serve`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Served {
    /// The phase's state after the command.
    pub state: PhaseState,
    /// The command (for the unit to run the new state's logic).
    pub command: CommandBody,
    /// Whether the phase table accepted it.
    pub accepted: bool,
}

/// A unit's side of one command for a phase in `state` that accepts
/// `ranges` (one per class parameter): the new state and the reply to send
/// with [`encode_ack`].
pub fn serve(state: PhaseState, ranges: &[(i32, i32)], body: &[u8]) -> Result<Served, ServeError> {
    let cmd = decode_command(body).map_err(|_| ServeError::InvalidRequest)?;
    if cmd.command == Command::Start {
        if cmd.n != ranges.len() {
            return Err(ServeError::InvalidRequest);
        }
        for (v, (lo, hi)) in cmd.args.iter().zip(ranges) {
            if v < lo || v > hi {
                return Err(ServeError::InvalidRequest);
            }
        }
    }
    let (to, effect) = phase::step(state, phase::Input::Command(cmd.command));
    Ok(Served {
        state: to,
        command: cmd,
        accepted: effect != phase::Effect::Rejected,
    })
}

#[cfg(test)]
mod tests;
