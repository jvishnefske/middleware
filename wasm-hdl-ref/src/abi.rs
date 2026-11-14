//! The import unit and the event entry points.
//!
//! The import unit keeps the subscription and serve tables and the `tick`
//! schedule, checks every guest pointer against linear memory and every
//! key against the payload rules, and hands the board-level effect to a
//! [`Host`] (in gateware: the frame encoder for `publish` and `log`, the
//! bridge rule table for `route_*`, the LED and button). Results and
//! argument checks follow `wasm-host` exactly (`wasm-host/src/link.rs`);
//! the order of checks is the same, so the same bad call gets the same
//! error code.

use middleware::key::{Key, KeyBuf};
use middleware::transport::{HandlerError, RequestMeta, SenderId};
use middleware::wasm_abi::bridge::{self, Bridge};
use middleware::wasm_abi::route::{self, RouteStat};
use middleware::wasm_abi::{
    AbiError, MAX_LOG, MAX_MESSAGE, MAX_SUBSCRIPTIONS, MIN_TICK_MS, Pull, guest_range, serve,
};

use crate::{Core, Trap};

/// The board side of the imports: what `wasm_host::Device` offers, for
/// the imports the core implements. Defaults are `Unsupported`.
pub trait Host {
    /// A `log` line (already truncated to `MAX_LOG` bytes of UTF-8).
    fn log(&mut self, line: &str);
    /// A publication (key valid and not under `sys/`, data at most
    /// `MAX_MESSAGE` bytes); on a two-port board it goes out on both.
    fn publish(&mut self, key: Key<'_>, data: &[u8]) -> Result<(), AbiError>;
    /// Milliseconds since start, wrapping.
    fn now_ms(&mut self) -> u32;
    /// `gpio_output`.
    fn gpio_output(&mut self, pin: u8) -> Result<(), AbiError> {
        let _ = pin;
        Err(AbiError::Unsupported)
    }
    /// `gpio_input`.
    fn gpio_input(&mut self, pin: u8, pull: Pull) -> Result<(), AbiError> {
        let _ = (pin, pull);
        Err(AbiError::Unsupported)
    }
    /// `gpio_write`.
    fn gpio_write(&mut self, pin: u8, high: bool) -> Result<(), AbiError> {
        let _ = (pin, high);
        Err(AbiError::Unsupported)
    }
    /// `gpio_read`.
    fn gpio_read(&mut self, pin: u8) -> Result<bool, AbiError> {
        let _ = pin;
        Err(AbiError::Unsupported)
    }
    /// `serve` of a new key.
    fn serve(&mut self, key: Key<'_>) -> Result<(), AbiError> {
        let _ = key;
        Err(AbiError::Unsupported)
    }
    /// `node_id`.
    fn node_id(&mut self) -> Result<SenderId, AbiError> {
        Err(AbiError::Unsupported)
    }
    /// `route_bridge` (validated).
    fn route_bridge(&mut self, b: &Bridge<'_>) -> Result<u8, AbiError> {
        let _ = b;
        Err(AbiError::Unsupported)
    }
    /// `route_stop`.
    fn route_stop(&mut self, handle: u8) -> Result<(), AbiError> {
        let _ = handle;
        Err(AbiError::Unsupported)
    }
    /// `route_stat`.
    fn route_stat(&mut self, handle: u8, stat: RouteStat) -> Result<u32, AbiError> {
        let _ = (handle, stat);
        Err(AbiError::Unsupported)
    }
}

/// A periodic `tick` (as `wasm_host::abi::Schedule`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    /// Period in ms.
    pub period: u32,
    /// Absolute wrapping time of the next tick.
    pub due: u32,
}

impl Schedule {
    const fn is_due(self, now: u32) -> bool {
        (now.wrapping_sub(self.due) as i32) >= 0
    }

    const fn advance(self, now: u32) -> Schedule {
        let next = Schedule {
            period: self.period,
            due: self.due.wrapping_add(self.period),
        };
        if next.is_due(now) {
            Schedule {
                period: self.period,
                due: now.wrapping_add(self.period),
            }
        } else {
            next
        }
    }
}

/// What [`Core::tick`] did (as `wasm_host::Ticked`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ticked {
    /// `tick` ran.
    Ran,
    /// Not due yet.
    NotDue,
    /// No period set.
    Disabled,
    /// The module exports no `tick`.
    NoTickExport,
}

fn code(r: Result<i32, AbiError>) -> i32 {
    r.unwrap_or_else(AbiError::code)
}

fn ok0(r: Result<(), AbiError>) -> Result<i32, AbiError> {
    r.map(|()| 0)
}

/// `wasm_host::abi::payload_key`.
fn payload_key(bytes: &[u8]) -> Result<Key<'_>, AbiError> {
    let text = core::str::from_utf8(bytes).map_err(|_| AbiError::InvalidKey)?;
    let key = Key::new(text).map_err(|_| AbiError::InvalidKey)?;
    if key.segments().next() == Some("sys") {
        return Err(AbiError::InvalidKey);
    }
    Ok(key)
}

/// `wasm_host::abi::log_line`.
fn log_line(bytes: &[u8]) -> &str {
    let bytes = bytes.get(..MAX_LOG).unwrap_or(bytes);
    match core::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => core::str::from_utf8(bytes.get(..e.valid_up_to()).unwrap_or_default())
            .unwrap_or_default(),
    }
}

fn pin(raw: i32) -> Result<u8, AbiError> {
    u8::try_from(raw).map_err(|_| AbiError::InvalidArgument)
}

fn arg(args: &[i32], i: usize) -> i32 {
    args.get(i).copied().unwrap_or(0)
}

/// Finds `key` in a slot table or takes a free slot (`table_slot`).
fn slot<const N: usize>(
    table: &[Option<KeyBuf<N>>],
    key: Key<'_>,
) -> Result<(usize, bool), AbiError> {
    let used = table.iter().take_while(|s| s.is_some()).count();
    if let Some(i) = table
        .iter()
        .flatten()
        .position(|k| k.as_str() == key.as_str())
    {
        return Ok((i, false));
    }
    if used >= table.len() {
        return Err(AbiError::NoResources);
    }
    Ok((used, true))
}

impl<const MEM: usize> Core<MEM> {
    fn guest(&self, p: i32, n: i32) -> Result<&[u8], AbiError> {
        let r = guest_range(p, n, self.mem_len)?;
        self.mem.get(r).ok_or(AbiError::OutOfBounds)
    }

    /// Runs one import call. Never traps: failures are error codes.
    pub(crate) fn host_call<H: Host>(&mut self, name: &str, a: &[i32], host: &mut H) -> i32 {
        let r: Result<i32, AbiError> = match name {
            "log" => {
                if let Ok(bytes) = self.guest(arg(a, 0), arg(a, 1)) {
                    host.log(log_line(bytes));
                }
                Ok(0)
            }
            "publish" => (|| {
                let key = self.guest(arg(a, 0), arg(a, 1))?;
                let data = self.guest(arg(a, 2), arg(a, 3))?;
                if data.len() > MAX_MESSAGE {
                    return Err(AbiError::InvalidArgument);
                }
                let key = payload_key(key)?;
                ok0(host.publish(key, data))
            })(),
            "subscribe" => (|| {
                let key = payload_key(self.guest(arg(a, 0), arg(a, 1))?)?;
                if self.exports.alloc.is_none() || self.exports.on_message.is_none() {
                    return Err(AbiError::Unsupported);
                }
                let (i, new) = slot(&self.subs, key)?;
                if new {
                    let k = KeyBuf::from_key(key).map_err(|_| AbiError::InvalidKey)?;
                    *self.subs.get_mut(i).ok_or(AbiError::NoResources)? = Some(k);
                }
                Ok(i as i32)
            })(),
            "now_ms" => Ok(host.now_ms() as i32),
            "set_tick" => (|| {
                let period = arg(a, 0);
                let now = host.now_ms();
                self.schedule = if period == 0 {
                    None
                } else if period < MIN_TICK_MS {
                    return Err(AbiError::InvalidArgument);
                } else if self.exports.tick.is_none() {
                    return Err(AbiError::Unsupported);
                } else {
                    let period = period.unsigned_abs();
                    Some(Schedule {
                        period,
                        due: now.wrapping_add(period),
                    })
                };
                Ok(0)
            })(),
            "gpio_output" => ok0(pin(arg(a, 0)).and_then(|p| host.gpio_output(p))),
            "gpio_input" => ok0(pin(arg(a, 0)).and_then(|p| {
                let pull = Pull::from_i32(arg(a, 1)).ok_or(AbiError::InvalidArgument)?;
                host.gpio_input(p, pull)
            })),
            "gpio_write" => ok0(pin(arg(a, 0)).and_then(|p| {
                let high = match arg(a, 1) {
                    0 => false,
                    1 => true,
                    _ => return Err(AbiError::InvalidArgument),
                };
                host.gpio_write(p, high)
            })),
            "gpio_read" => pin(arg(a, 0))
                .and_then(|p| host.gpio_read(p))
                .map(i32::from),
            "serve" => (|| {
                let key = payload_key(self.guest(arg(a, 0), arg(a, 1))?)?;
                if self.exports.alloc.is_none() || self.exports.on_request.is_none() {
                    return Err(AbiError::Unsupported);
                }
                let (i, new) = slot(&self.serves, key)?;
                if new {
                    host.serve(key)?;
                    let k = KeyBuf::from_key(key).map_err(|_| AbiError::InvalidKey)?;
                    *self.serves.get_mut(i).ok_or(AbiError::NoResources)? = Some(k);
                }
                Ok(i as i32)
            })(),
            "node_id" => (|| {
                let r = guest_range(arg(a, 0), serve::NODE_ID_LEN as i32, self.mem_len)?;
                let id = host.node_id()?;
                self.mem
                    .get_mut(r)
                    .ok_or(AbiError::OutOfBounds)?
                    .copy_from_slice(&id.0);
                Ok(0)
            })(),
            "route_bridge" => (|| {
                let (src, dst) = (arg(a, 0), arg(a, 3));
                bridge::links(src, dst)?;
                let key = self.guest(arg(a, 1), arg(a, 2))?;
                let b = bridge::parse(src, key, dst)?;
                let h = host.route_bridge(&b)?;
                if bridge::handle_index(h).is_none() {
                    return Err(AbiError::NoResources);
                }
                Ok(i32::from(h))
            })(),
            "route_stop" => ok0(route::route_handle(arg(a, 0)).and_then(|h| host.route_stop(h))),
            "route_stat" => (|| {
                let h = route::route_handle(arg(a, 0))?;
                let s = RouteStat::from_i32(arg(a, 1))?;
                Ok(i32::try_from(host.route_stat(h, s)?).unwrap_or(i32::MAX))
            })(),
            _ => Err(AbiError::Unsupported),
        };
        code(r)
    }

    /// Runs `init` if exported. A trap poisons the payload.
    pub fn init<H: Host>(&mut self, host: &mut H) -> Result<(), Trap> {
        match self.exports.init {
            Some(f) => self.invoke(f, &[], host).map(drop),
            None if !self.loaded => Err(Trap::NotLoaded),
            None => Ok(()),
        }
    }

    /// Calls `tick(now_ms)` when due (as `wasm_host::Payload::tick`).
    pub fn tick<H: Host>(&mut self, now_ms: u32, host: &mut H) -> Result<Ticked, Trap> {
        if self.poisoned {
            return Err(Trap::Poisoned);
        }
        let Some(f) = self.exports.tick else {
            return Ok(Ticked::NoTickExport);
        };
        match self.schedule {
            None => return Ok(Ticked::Disabled),
            Some(s) if !s.is_due(now_ms) => return Ok(Ticked::NotDue),
            Some(s) => self.schedule = Some(s.advance(now_ms)),
        }
        self.invoke(f, &[now_ms as i32], host).map(|_| Ticked::Ran)
    }

    /// The `now_ms` the next tick is due.
    pub fn next_tick(&self) -> Option<u32> {
        if self.poisoned {
            return None;
        }
        self.schedule.map(|s| s.due)
    }

    /// Delivers a publication on a subscribed key (as
    /// `wasm_host::Payload::deliver`): `Ok(false)` when not subscribed,
    /// too long, or refused by `mw_alloc`.
    pub fn deliver<H: Host>(&mut self, key: Key<'_>, data: &[u8], host: &mut H) -> Result<bool, Trap> {
        if self.poisoned {
            return Err(Trap::Poisoned);
        }
        let Some(slot) = self
            .subs
            .iter()
            .flatten()
            .position(|k| k.as_str() == key.as_str())
        else {
            return Ok(false);
        };
        if data.len() > MAX_MESSAGE {
            return Ok(false);
        }
        let (Some(alloc), Some(on_message)) = (self.exports.alloc, self.exports.on_message) else {
            return Ok(false);
        };
        let len = data.len() as i32;
        let ptr = self.invoke(alloc, &[len], host)?.unwrap_or(0);
        if ptr == 0 {
            return Ok(false);
        }
        let Ok(r) = guest_range(ptr, len, self.mem_len) else {
            return Ok(false);
        };
        let Some(dst) = self.mem.get_mut(r) else {
            return Ok(false);
        };
        dst.copy_from_slice(data);
        self.invoke(on_message, &[slot as i32, ptr, len], host)?;
        Ok(true)
    }

    /// Hands a request to the payload serving `key` (as
    /// `wasm_host::Payload::request`).
    pub fn request<H: Host>(
        &mut self,
        key: Key<'_>,
        body: &[u8],
        meta: &RequestMeta<'_>,
        reply: &mut [u8],
        host: &mut H,
    ) -> Result<Result<usize, HandlerError>, Trap> {
        if self.poisoned {
            return Err(Trap::Poisoned);
        }
        let Some(slot) = self
            .serves
            .iter()
            .flatten()
            .position(|k| k.as_str() == key.as_str())
        else {
            return Ok(Err(HandlerError::NotFound));
        };
        let layout = match serve::layout(body.len(), reply.len()) {
            Ok(l) => l,
            Err(e) => return Ok(Err(e)),
        };
        let (Some(alloc), Some(on_request)) = (self.exports.alloc, self.exports.on_request) else {
            return Ok(Err(HandlerError::Unavailable));
        };
        let ptr = self
            .invoke(alloc, &[layout.alloc_len as i32], host)?
            .unwrap_or(0);
        if ptr == 0 {
            return Ok(Err(HandlerError::Unavailable));
        }
        let Ok(all) = guest_range(ptr, layout.alloc_len as i32, self.mem_len) else {
            return Ok(Err(HandlerError::Unavailable));
        };
        let base = all.start;
        let meta_b = serve::encode_meta(meta);
        let mut write = |at: usize, b: &[u8]| -> Option<()> {
            self.mem.get_mut(at..at + b.len())?.copy_from_slice(b);
            Some(())
        };
        if write(base, &meta_b).is_none() || write(base + layout.req_at, body).is_none() {
            return Ok(Err(HandlerError::Unavailable));
        }
        let args = [
            slot as i32,
            (base + layout.req_at) as i32,
            layout.req_len as i32,
            base as i32,
            (base + layout.reply_at) as i32,
            layout.reply_cap as i32,
        ];
        let rc = self.invoke(on_request, &args, host)?.unwrap_or(0);
        let n = match serve::reply_result(rc, layout.reply_cap) {
            Ok(n) => n,
            Err(e) => return Ok(Err(e)),
        };
        let src = base + layout.reply_at;
        match (reply.get_mut(..n), self.mem.get(src..src + n)) {
            (Some(out), Some(s)) => {
                out.copy_from_slice(s);
                Ok(Ok(n))
            }
            _ => Ok(Err(HandlerError::Internal)),
        }
    }
}

const _: () = assert!(MAX_SUBSCRIPTIONS == 8);
