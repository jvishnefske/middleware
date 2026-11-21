//! A heap-free Zenoh 1.x client for boards (feature `zenoh-lite`, `no_std`).
//!
//! The `zenoh` crates build without `std` but need `alloc`, `hashbrown`
//! and `getrandom` (whose only hook on bare-metal Arm is a macro expanding
//! to `unsafe` in the caller), so a board runs this client instead: a pure
//! codec and session state machine for the subset a client needs, and a
//! thin async shell over one `embedded-io-async` byte stream (an
//! embassy-net `TcpSocket` on the board, a tokio `TcpStream` in the host
//! interop tests).
//!
//! * [`codec`] — the wire format of protocol version 0x09 (`zenoh` 1.10),
//!   byte-identical to `zenoh-codec` 1.10: InitSyn/Ack, OpenSyn/Ack (with
//!   the cookie), KeepAlive, Close, Frame (reliable and best effort);
//!   Push (Put, Del), Request (Query), Response (Reply, Err),
//!   ResponseFinal, Declare (key expressions, subscribers, queryables,
//!   tokens, final) and Interest; variable-length integers; extensions
//!   skipped when unknown and refused when mandatory; the 2-byte
//!   little-endian length before each batch on TCP.
//! * [`session`] — the transport session as a `Copy` state and pure
//!   transitions: handshake, lease and keep-alive deadlines from a
//!   caller-supplied clock, outgoing and incoming sequence numbers, close,
//!   reconnect with bounded backoff. Malformed and out-of-order input is
//!   refused, never a panic.
//! * [`tables`] — what the client declared and the key expression ids the
//!   router declared, fixed capacity.
//! * [`message`] — [`Unframed`](message::Unframed): the client's byte
//!   stream over a link that keeps message boundaries (a WebSocket, one
//!   batch per message, no length prefix).
//! * [`client`] — [`ZenohLite`], the async shell: the transport traits
//!   ([`Publisher`](crate::Publisher), [`Subscriber`](crate::Subscriber),
//!   [`Subscribe`](crate::Subscribe), [`Requester`](crate::Requester),
//!   [`Gather`](crate::Gather), [`Responder`](crate::Responder)) and an
//!   event loop ([`ZenohLite::next_event`]) for firmware.
//!
//! # Scope
//!
//! Client mode over a single TCP stream to one router or peer listener.
//! Not implemented: scouting (the endpoint is given), fragmentation
//! (every message must fit one batch; larger ones are refused, received
//! fragments are dropped and counted), QoS priorities (one channel at the
//! default priority), compression, shared memory, authentication,
//! multilink, peer mode and liveliness tokens (decoded, ignored).
//!
//! # Interoperation with the host backend
//!
//! Publications and replies carry the middleware attachments of
//! [`zenoh_wire`](crate::zenoh_wire) exactly as `zenoh::ZenohTransport`
//! writes them (sender id, sequence, stream id; reply sender; query
//! requester and deadline), the sender id is the FNV-1a hash of the Zenoh
//! id as on the host, and each queryable is also declared on its per-node
//! key, so a host `ZenohTransport` and its `SeenTable` treat the board like
//! any other node.
//!
//! Queries carry the 0.6 trace extension when the [`RequestOptions`](crate::RequestOptions)
//! hold a trace context or request id (the attachment is built in a fixed
//! buffer, no heap), and the plain 0.5 attachment otherwise. Received
//! queries decode either form; the handler's [`RequestMeta`](crate::RequestMeta)
//! gets the sender, deadline and trace context, and no request id (it
//! would borrow the receive buffer; K19 allows a link to carry none).

pub mod client;
pub mod codec;
pub mod message;
pub mod session;
pub mod tables;

pub use client::{Clock, Event, LiteError, LiteSubscription, QueryEvent, Stats, ZenohLite};
pub use codec::Zid;
pub use session::Config;
