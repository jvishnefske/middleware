//! Network messages of the client subset and the Zenoh bodies they carry.
//!
//! | message | id | carries |
//! |---------|----|---------|
//! | [`Push`] | `0x1d` | [`PushBody`]: [`Put`] (`0x01`) or [`Del`] (`0x02`) |
//! | [`Request`] | `0x1c` | [`Query`] (`0x03`) |
//! | [`Response`] | `0x1b` | [`Reply`] (`0x04`, itself a [`PushBody`]) or [`ReplyErr`] (`0x05`) |
//! | [`ResponseFinal`] | `0x1a` | |
//! | [`Interest`] | `0x19` | |
//! | [`Declare`] | `0x1e` | [`DeclareBody`]: key expressions, subscribers, queryables, tokens, final |
//! | OAM | `0x1f` | decoded and skipped ([`NetworkMessage::Oam`]) |
//!
//! Every network message may carry the QoS (`0x21`, an integer: priority,
//! congestion control and express bits; default 5 is omitted), timestamp
//! (`0x42`) and node id (`0x33`, mandatory, default 0 omitted) extensions;
//! Request adds target (`0x34`, mandatory), budget and timeout, Response a
//! responder id. Bodies carry source info and the user attachment
//! (`zenoh` attachments are where the middleware's `mw` metadata goes).
//! Extensions are written in the order `zenoh-codec` 1.10 writes them, so
//! the bytes are identical; the timestamp stack extension (`0x47`) and
//! unknown ones are skipped on decode.

use super::{
    CodecError, Encoding, EntityGlobalId, Reader, Timestamp, WireExpr, Writer, has, mid, read_ext,
    skip_exts, write_ext_z64, write_ext_zbuf, zint_len,
};

/// Network message ids.
pub mod id {
    /// Interest.
    pub const INTEREST: u8 = 0x19;
    /// ResponseFinal.
    pub const RESPONSE_FINAL: u8 = 0x1a;
    /// Response.
    pub const RESPONSE: u8 = 0x1b;
    /// Request.
    pub const REQUEST: u8 = 0x1c;
    /// Push.
    pub const PUSH: u8 = 0x1d;
    /// Declare.
    pub const DECLARE: u8 = 0x1e;
    /// OAM.
    pub const OAM: u8 = 0x1f;
}

/// True if `mid` is a network message id.
pub const fn is_network_id(mid: u8) -> bool {
    mid >= id::INTEREST && mid <= id::OAM
}

const FLAG_N: u8 = 1 << 5;
const FLAG_M: u8 = 1 << 6;
const FLAG_Z: u8 = 1 << 7;

const EXT_QOS: u8 = 0x21;
const EXT_TSTAMP: u8 = 0x42;
const EXT_NODE_ID: u8 = 0x33;
const EXT_TARGET: u8 = 0x34;
const EXT_BUDGET: u8 = 0x25;
const EXT_TIMEOUT: u8 = 0x26;
const EXT_RESPONDER: u8 = 0x43;

/// The default QoS byte (priority `Data`, congestion control `Drop`, not
/// express), which the wire omits.
pub const QOS_DEFAULT: u8 = 5;
/// QoS bit: congestion control `Block`.
pub const QOS_BLOCK: u8 = 0b0000_1000;
/// QoS bit: express.
pub const QOS_EXPRESS: u8 = 0b0001_0000;

/// Source of a sample or query: the entity and its sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceInfo {
    /// The publishing or querying entity.
    pub id: EntityGlobalId,
    /// Its sequence number.
    pub sn: u32,
}

impl SourceInfo {
    fn len(&self) -> usize {
        self.id.len() + zint_len(u64::from(self.sn))
    }

    fn write_ext(&self, w: &mut Writer<'_>, more: bool) -> Result<(), CodecError> {
        w.u8(if more { 0x41 | FLAG_Z } else { 0x41 })?;
        w.zint(self.len() as u64)?;
        self.id.write(w)?;
        w.zint(u64::from(self.sn))
    }

    fn read(body: &[u8]) -> Result<Self, CodecError> {
        let mut r = Reader::new(body);
        let id = EntityGlobalId::read(&mut r)?;
        let sn = r.zint()? as u32;
        Ok(SourceInfo { id, sn })
    }
}

fn write_tstamp_ext(w: &mut Writer<'_>, ts: &Timestamp<'_>, more: bool) -> Result<(), CodecError> {
    w.u8(if more {
        EXT_TSTAMP | FLAG_Z
    } else {
        EXT_TSTAMP
    })?;
    w.zint(ts.len() as u64)?;
    ts.write(w)
}

fn read_tstamp(body: &[u8]) -> Result<Timestamp<'_>, CodecError> {
    let mut r = Reader::new(body);
    Timestamp::read(&mut r)
}

/// QoS, timestamp and node id: the extensions every network message may
/// carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetExts<'a> {
    /// QoS byte ([`QOS_DEFAULT`] when absent).
    pub qos: u8,
    /// Timestamp.
    pub timestamp: Option<Timestamp<'a>>,
    /// Node id (0 when absent).
    pub node_id: u16,
}

impl NetExts<'_> {
    /// No extension.
    pub const DEFAULT: NetExts<'static> = NetExts {
        qos: QOS_DEFAULT,
        timestamp: None,
        node_id: 0,
    };
}

impl Default for NetExts<'_> {
    fn default() -> Self {
        NetExts::DEFAULT
    }
}

/// A payload with its encoding (a query's body).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Value<'a> {
    /// The encoding.
    pub encoding: Encoding<'a>,
    /// The bytes.
    pub payload: &'a [u8],
}

/// A publication: the value of a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Put<'a> {
    /// Source timestamp.
    pub timestamp: Option<Timestamp<'a>>,
    /// Payload encoding.
    pub encoding: Encoding<'a>,
    /// Source info extension.
    pub source: Option<SourceInfo>,
    /// User attachment.
    pub attachment: Option<&'a [u8]>,
    /// The payload.
    pub payload: &'a [u8],
}

/// A deletion of a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Del<'a> {
    /// Source timestamp.
    pub timestamp: Option<Timestamp<'a>>,
    /// Source info extension.
    pub source: Option<SourceInfo>,
    /// User attachment.
    pub attachment: Option<&'a [u8]>,
}

/// What a [`Push`] or a [`Reply`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushBody<'a> {
    /// A value.
    Put(Put<'a>),
    /// A deletion.
    Del(Del<'a>),
}

/// A query: the body of a [`Request`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Query<'a> {
    /// Consolidation: 0 auto (omitted), 1 none, 2 monotonic, 3 latest.
    pub consolidation: u8,
    /// Selector parameters.
    pub parameters: &'a str,
    /// Source info extension.
    pub source: Option<SourceInfo>,
    /// The query payload.
    pub body: Option<Value<'a>>,
    /// User attachment.
    pub attachment: Option<&'a [u8]>,
}

/// A successful answer: the body of a [`Response`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply<'a> {
    /// Consolidation (0 omitted).
    pub consolidation: u8,
    /// The value or deletion.
    pub body: PushBody<'a>,
}

/// An error answer: the body of a [`Response`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReplyErr<'a> {
    /// Payload encoding.
    pub encoding: Encoding<'a>,
    /// Source info extension.
    pub source: Option<SourceInfo>,
    /// The error payload.
    pub payload: &'a [u8],
}

/// What a [`Response`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseBody<'a> {
    /// A reply.
    Reply(Reply<'a>),
    /// An error.
    Err(ReplyErr<'a>),
}

/// Data for subscribers of a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Push<'a> {
    /// The key.
    pub wire_expr: WireExpr<'a>,
    /// Common extensions.
    pub exts: NetExts<'a>,
    /// The value or deletion.
    pub body: PushBody<'a>,
}

/// Which queryables a request goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueryTarget {
    /// The best matching one (default, omitted).
    #[default]
    BestMatching,
    /// Every matching one.
    All,
    /// Every complete matching one.
    AllComplete,
}

/// A query to queryables of a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request<'a> {
    /// Request id, echoed by responses.
    pub id: u32,
    /// The key expression queried.
    pub wire_expr: WireExpr<'a>,
    /// Common extensions.
    pub exts: NetExts<'a>,
    /// Target extension.
    pub target: QueryTarget,
    /// Maximum number of responses.
    pub budget: Option<u32>,
    /// Timeout in milliseconds.
    pub timeout_ms: Option<u64>,
    /// The query.
    pub body: Query<'a>,
}

/// One answer to a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Response<'a> {
    /// The request it answers.
    pub rid: u32,
    /// The key of the answer.
    pub wire_expr: WireExpr<'a>,
    /// Common extensions (no node id).
    pub exts: NetExts<'a>,
    /// Responder id extension.
    pub responder: Option<EntityGlobalId>,
    /// The reply or error.
    pub body: ResponseBody<'a>,
}

/// The end of the answers to a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseFinal<'a> {
    /// The request.
    pub rid: u32,
    /// Common extensions (no node id).
    pub exts: NetExts<'a>,
}

/// What a [`Declare`] declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclareBody<'a> {
    /// A numeric id for a key expression.
    KeyExpr {
        /// The id.
        id: u16,
        /// The expression.
        wire_expr: WireExpr<'a>,
    },
    /// Forget a key expression id.
    UndeclareKeyExpr {
        /// The id.
        id: u16,
    },
    /// A subscriber.
    Subscriber {
        /// Entity id.
        id: u32,
        /// Key expression.
        wire_expr: WireExpr<'a>,
    },
    /// Remove a subscriber.
    UndeclareSubscriber {
        /// Entity id.
        id: u32,
        /// Key expression extension.
        wire_expr: Option<WireExpr<'a>>,
    },
    /// A queryable.
    Queryable {
        /// Entity id.
        id: u32,
        /// Key expression.
        wire_expr: WireExpr<'a>,
        /// Complete flag of the info extension.
        complete: bool,
        /// Distance of the info extension.
        distance: u16,
    },
    /// Remove a queryable.
    UndeclareQueryable {
        /// Entity id.
        id: u32,
        /// Key expression extension.
        wire_expr: Option<WireExpr<'a>>,
    },
    /// A liveliness token.
    Token {
        /// Entity id.
        id: u32,
        /// Key expression.
        wire_expr: WireExpr<'a>,
    },
    /// Remove a token.
    UndeclareToken {
        /// Entity id.
        id: u32,
        /// Key expression extension.
        wire_expr: Option<WireExpr<'a>>,
    },
    /// End of the answers to an interest.
    Final,
}

/// A declaration, possibly answering an [`Interest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Declare<'a> {
    /// The interest this answers (current declarations).
    pub interest_id: Option<u32>,
    /// Common extensions.
    pub exts: NetExts<'a>,
    /// What is declared.
    pub body: DeclareBody<'a>,
}

/// How an interest wants declarations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterestMode {
    /// Stop a future interest.
    Final,
    /// The current declarations, then a final declare.
    Current,
    /// Future declarations.
    Future,
    /// Both.
    CurrentFuture,
}

impl InterestMode {
    /// True if the current declarations are asked for.
    pub const fn is_current(self) -> bool {
        matches!(self, InterestMode::Current | InterestMode::CurrentFuture)
    }
}

/// Interest option bits.
pub mod interest {
    /// Key expressions.
    pub const KEYEXPRS: u8 = 1;
    /// Subscribers.
    pub const SUBSCRIBERS: u8 = 1 << 1;
    /// Queryables.
    pub const QUERYABLES: u8 = 1 << 2;
    /// Tokens.
    pub const TOKENS: u8 = 1 << 3;
    /// Restricted to a key expression (set from the wire expression).
    pub const RESTRICTED: u8 = 1 << 4;
    /// The key expression has a suffix (set from the wire expression).
    pub const NAMED: u8 = 1 << 5;
    /// The key expression uses the sender's mapping.
    pub const MAPPING: u8 = 1 << 6;
    /// Aggregate replies.
    pub const AGGREGATE: u8 = 1 << 7;
}

/// A request for declarations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interest<'a> {
    /// Interest id, echoed by the answering declares.
    pub id: u32,
    /// Mode.
    pub mode: InterestMode,
    /// Option bits ([`interest`]); restricted/named/mapping are derived
    /// from `wire_expr` when writing.
    pub options: u8,
    /// The key expression it is restricted to.
    pub wire_expr: Option<WireExpr<'a>>,
    /// Common extensions.
    pub exts: NetExts<'a>,
}

/// A network message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkMessage<'a> {
    /// See [`Push`].
    Push(Push<'a>),
    /// See [`Request`].
    Request(Request<'a>),
    /// See [`Response`].
    Response(Response<'a>),
    /// See [`ResponseFinal`].
    ResponseFinal(ResponseFinal<'a>),
    /// See [`Declare`].
    Declare(Declare<'a>),
    /// See [`Interest`].
    Interest(Interest<'a>),
    /// An OAM message (decoded and ignored).
    Oam,
}

// ---------------------------------------------------------------------
// Common extensions.

/// Counts the common extensions present, for the Z flag and "more" bits.
fn net_ext_count(e: &NetExts<'_>, node_id: bool) -> usize {
    usize::from(e.qos != QOS_DEFAULT)
        + usize::from(e.timestamp.is_some())
        + usize::from(node_id && e.node_id != 0)
}

/// Writes QoS then timestamp; `left` is how many extensions follow in
/// total (including these), so the Z bits chain correctly. Returns what
/// is left after them.
fn write_qos_ts(w: &mut Writer<'_>, e: &NetExts<'_>, mut left: usize) -> Result<usize, CodecError> {
    if e.qos != QOS_DEFAULT {
        left = left.saturating_sub(1);
        write_ext_z64(w, EXT_QOS, u64::from(e.qos), left != 0)?;
    }
    if let Some(ts) = &e.timestamp {
        left = left.saturating_sub(1);
        write_tstamp_ext(w, ts, left != 0)?;
    }
    Ok(left)
}

fn write_node_id(
    w: &mut Writer<'_>,
    e: &NetExts<'_>,
    mut left: usize,
) -> Result<usize, CodecError> {
    if e.node_id != 0 {
        left = left.saturating_sub(1);
        write_ext_z64(w, EXT_NODE_ID, u64::from(e.node_id), left != 0)?;
    }
    Ok(left)
}

/// Reads the extensions of a network message: the common ones into
/// `exts`, the others through `other` (which returns `false` for an id it
/// does not know, so the extension is skipped or refused as mandatory).
fn read_net_exts<'a, F>(
    r: &mut Reader<'a>,
    present: bool,
    exts: &mut NetExts<'a>,
    mut other: F,
) -> Result<(), CodecError>
where
    F: FnMut(&super::Ext<'a>) -> Result<bool, CodecError>,
{
    let mut more = present;
    while more {
        let (e, m) = read_ext(r)?;
        match e.id {
            EXT_QOS => exts.qos = e.z64()? as u8,
            EXT_TSTAMP => exts.timestamp = Some(read_tstamp(e.zbuf()?)?),
            EXT_NODE_ID => exts.node_id = e.z64()? as u16,
            _ => {
                if !other(&e)? {
                    e.skip()?;
                }
            }
        }
        more = m;
    }
    Ok(())
}

fn expr_flags(we: &WireExpr<'_>) -> u8 {
    let mut h = 0;
    if we.has_suffix() {
        h |= FLAG_N;
    }
    if we.sender_mapping {
        h |= FLAG_M;
    }
    h
}

// ---------------------------------------------------------------------
// Zenoh bodies.

const PUT: u8 = 0x01;
const DEL: u8 = 0x02;
const QUERY: u8 = 0x03;
const REPLY: u8 = 0x04;
const ERR: u8 = 0x05;
const FLAG_T: u8 = 1 << 5;
const FLAG_E: u8 = 1 << 6;
const FLAG_C: u8 = 1 << 5;
const FLAG_P: u8 = 1 << 6;
const MAX_PAYLOAD: u64 = u32::MAX as u64;

impl<'a> Put<'a> {
    /// Writes the body.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        let mut h = PUT;
        if self.timestamp.is_some() {
            h |= FLAG_T;
        }
        if !self.encoding.is_empty() {
            h |= FLAG_E;
        }
        let mut left = usize::from(self.source.is_some()) + usize::from(self.attachment.is_some());
        if left != 0 {
            h |= FLAG_Z;
        }
        w.u8(h)?;
        if let Some(ts) = &self.timestamp {
            ts.write(w)?;
        }
        if !self.encoding.is_empty() {
            self.encoding.write(w)?;
        }
        if let Some(s) = &self.source {
            left = left.saturating_sub(1);
            s.write_ext(w, left != 0)?;
        }
        if let Some(a) = self.attachment {
            write_ext_zbuf(w, 0x43, a, false)?;
        }
        w.bytes_z(self.payload)
    }

    fn read(r: &mut Reader<'a>, h: u8) -> Result<Self, CodecError> {
        let timestamp = if has(h, FLAG_T) {
            Some(Timestamp::read(r)?)
        } else {
            None
        };
        let encoding = if has(h, FLAG_E) {
            Encoding::read(r)?
        } else {
            Encoding::EMPTY
        };
        let (mut source, mut attachment) = (None, None);
        let mut more = has(h, FLAG_Z);
        while more {
            let (e, m) = read_ext(r)?;
            match e.id {
                0x41 => source = Some(SourceInfo::read(e.zbuf()?)?),
                0x43 => attachment = Some(e.zbuf()?),
                _ => e.skip()?,
            }
            more = m;
        }
        let payload = r.bytes_z(MAX_PAYLOAD)?;
        Ok(Put {
            timestamp,
            encoding,
            source,
            attachment,
            payload,
        })
    }
}

impl<'a> Del<'a> {
    /// Writes the body.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        let mut h = DEL;
        if self.timestamp.is_some() {
            h |= FLAG_T;
        }
        let mut left = usize::from(self.source.is_some()) + usize::from(self.attachment.is_some());
        if left != 0 {
            h |= FLAG_Z;
        }
        w.u8(h)?;
        if let Some(ts) = &self.timestamp {
            ts.write(w)?;
        }
        if let Some(s) = &self.source {
            left = left.saturating_sub(1);
            s.write_ext(w, left != 0)?;
        }
        if let Some(a) = self.attachment {
            write_ext_zbuf(w, 0x42, a, false)?;
        }
        Ok(())
    }

    fn read(r: &mut Reader<'a>, h: u8) -> Result<Self, CodecError> {
        let timestamp = if has(h, FLAG_T) {
            Some(Timestamp::read(r)?)
        } else {
            None
        };
        let (mut source, mut attachment) = (None, None);
        let mut more = has(h, FLAG_Z);
        while more {
            let (e, m) = read_ext(r)?;
            match e.id {
                0x41 => source = Some(SourceInfo::read(e.zbuf()?)?),
                0x42 => attachment = Some(e.zbuf()?),
                _ => e.skip()?,
            }
            more = m;
        }
        Ok(Del {
            timestamp,
            source,
            attachment,
        })
    }
}

impl<'a> PushBody<'a> {
    /// Writes the body.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        match self {
            PushBody::Put(p) => p.write(w),
            PushBody::Del(d) => d.write(w),
        }
    }

    fn read(r: &mut Reader<'a>) -> Result<Self, CodecError> {
        let h = r.u8()?;
        match mid(h) {
            PUT => Ok(PushBody::Put(Put::read(r, h)?)),
            DEL => Ok(PushBody::Del(Del::read(r, h)?)),
            _ => Err(CodecError::Invalid),
        }
    }
}

fn read_consolidation(r: &mut Reader<'_>) -> Result<u8, CodecError> {
    // Unknown values fall back to auto, as zenoh-codec does.
    Ok(match r.zint()? {
        v @ 1..=3 => v as u8,
        _ => 0,
    })
}

impl<'a> Query<'a> {
    /// Writes the body.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        let mut h = QUERY;
        if self.consolidation != 0 {
            h |= FLAG_C;
        }
        if !self.parameters.is_empty() {
            h |= FLAG_P;
        }
        let mut left = usize::from(self.source.is_some())
            + usize::from(self.body.is_some())
            + usize::from(self.attachment.is_some());
        if left != 0 {
            h |= FLAG_Z;
        }
        w.u8(h)?;
        if self.consolidation != 0 {
            w.zint(u64::from(self.consolidation))?;
        }
        if !self.parameters.is_empty() {
            w.bytes_z(self.parameters.as_bytes())?;
        }
        if let Some(s) = &self.source {
            left = left.saturating_sub(1);
            s.write_ext(w, left != 0)?;
        }
        if let Some(v) = &self.body {
            left = left.saturating_sub(1);
            w.u8(if left != 0 { 0x43 | FLAG_Z } else { 0x43 })?;
            w.zint((v.encoding.len() + v.payload.len()) as u64)?;
            v.encoding.write(w)?;
            w.bytes(v.payload)?;
        }
        if let Some(a) = self.attachment {
            write_ext_zbuf(w, 0x45, a, false)?;
        }
        Ok(())
    }

    fn read(r: &mut Reader<'a>, h: u8) -> Result<Self, CodecError> {
        let consolidation = if has(h, FLAG_C) {
            read_consolidation(r)?
        } else {
            0
        };
        let parameters = if has(h, FLAG_P) {
            r.str_z(u64::from(u32::MAX))?
        } else {
            ""
        };
        let (mut source, mut body, mut attachment) = (None, None, None);
        let mut more = has(h, FLAG_Z);
        while more {
            let (e, m) = read_ext(r)?;
            match e.id {
                0x41 => source = Some(SourceInfo::read(e.zbuf()?)?),
                0x43 => {
                    let mut vr = Reader::new(e.zbuf()?);
                    let encoding = Encoding::read(&mut vr)?;
                    body = Some(Value {
                        encoding,
                        payload: vr.rest(),
                    });
                }
                0x45 => attachment = Some(e.zbuf()?),
                _ => e.skip()?,
            }
            more = m;
        }
        Ok(Query {
            consolidation,
            parameters,
            source,
            body,
            attachment,
        })
    }
}

impl<'a> Reply<'a> {
    /// Writes the body.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        let mut h = REPLY;
        if self.consolidation != 0 {
            h |= FLAG_C;
        }
        w.u8(h)?;
        if self.consolidation != 0 {
            w.zint(u64::from(self.consolidation))?;
        }
        self.body.write(w)
    }

    fn read(r: &mut Reader<'a>, h: u8) -> Result<Self, CodecError> {
        let consolidation = if has(h, FLAG_C) {
            read_consolidation(r)?
        } else {
            0
        };
        skip_exts(r, has(h, FLAG_Z))?;
        let body = PushBody::read(r)?;
        Ok(Reply {
            consolidation,
            body,
        })
    }
}

impl<'a> ReplyErr<'a> {
    /// Writes the body.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        let mut h = ERR;
        if !self.encoding.is_empty() {
            h |= FLAG_E;
        }
        if self.source.is_some() {
            h |= FLAG_Z;
        }
        w.u8(h)?;
        if !self.encoding.is_empty() {
            self.encoding.write(w)?;
        }
        if let Some(s) = &self.source {
            s.write_ext(w, false)?;
        }
        w.bytes_z(self.payload)
    }

    fn read(r: &mut Reader<'a>, h: u8) -> Result<Self, CodecError> {
        let encoding = if has(h, FLAG_E) {
            Encoding::read(r)?
        } else {
            Encoding::EMPTY
        };
        let mut source = None;
        let mut more = has(h, FLAG_Z);
        while more {
            let (e, m) = read_ext(r)?;
            match e.id {
                0x41 => source = Some(SourceInfo::read(e.zbuf()?)?),
                _ => e.skip()?,
            }
            more = m;
        }
        let payload = r.bytes_z(MAX_PAYLOAD)?;
        Ok(ReplyErr {
            encoding,
            source,
            payload,
        })
    }
}

impl<'a> ResponseBody<'a> {
    /// Writes the body.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        match self {
            ResponseBody::Reply(b) => b.write(w),
            ResponseBody::Err(b) => b.write(w),
        }
    }

    fn read(r: &mut Reader<'a>) -> Result<Self, CodecError> {
        let h = r.u8()?;
        match mid(h) {
            REPLY => Ok(ResponseBody::Reply(Reply::read(r, h)?)),
            ERR => Ok(ResponseBody::Err(ReplyErr::read(r, h)?)),
            _ => Err(CodecError::Invalid),
        }
    }
}

// ---------------------------------------------------------------------
// Network messages.

impl<'a> Push<'a> {
    /// Writes the message.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        let left = net_ext_count(&self.exts, true);
        let mut h = id::PUSH | expr_flags(&self.wire_expr);
        if left != 0 {
            h |= FLAG_Z;
        }
        w.u8(h)?;
        self.wire_expr.write(w)?;
        let left = write_qos_ts(w, &self.exts, left)?;
        write_node_id(w, &self.exts, left)?;
        self.body.write(w)
    }

    fn read(r: &mut Reader<'a>, h: u8) -> Result<Self, CodecError> {
        let wire_expr = WireExpr::read(r, has(h, FLAG_N), has(h, FLAG_M))?;
        let mut exts = NetExts::DEFAULT;
        read_net_exts(r, has(h, FLAG_Z), &mut exts, |_| Ok(false))?;
        let body = PushBody::read(r)?;
        Ok(Push {
            wire_expr,
            exts,
            body,
        })
    }
}

impl<'a> Request<'a> {
    /// Writes the message.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        let target = self.target != QueryTarget::BestMatching;
        let mut left = net_ext_count(&self.exts, true)
            + usize::from(target)
            + usize::from(self.budget.is_some())
            + usize::from(self.timeout_ms.is_some());
        let mut h = id::REQUEST | expr_flags(&self.wire_expr);
        if left != 0 {
            h |= FLAG_Z;
        }
        w.u8(h)?;
        w.zint(u64::from(self.id))?;
        self.wire_expr.write(w)?;
        left = write_qos_ts(w, &self.exts, left)?;
        if target {
            left = left.saturating_sub(1);
            let v = match self.target {
                QueryTarget::BestMatching => 0,
                QueryTarget::All => 1,
                QueryTarget::AllComplete => 2,
            };
            write_ext_z64(w, EXT_TARGET, v, left != 0)?;
        }
        if let Some(b) = self.budget {
            left = left.saturating_sub(1);
            write_ext_z64(w, EXT_BUDGET, u64::from(b), left != 0)?;
        }
        if let Some(t) = self.timeout_ms {
            left = left.saturating_sub(1);
            write_ext_z64(w, EXT_TIMEOUT, t, left != 0)?;
        }
        write_node_id(w, &self.exts, left)?;
        self.body.write(w)
    }

    fn read(r: &mut Reader<'a>, h: u8) -> Result<Self, CodecError> {
        let id = r.zu32()?;
        let wire_expr = WireExpr::read(r, has(h, FLAG_N), has(h, FLAG_M))?;
        let mut exts = NetExts::DEFAULT;
        let (mut target, mut budget, mut timeout_ms) = (QueryTarget::BestMatching, None, None);
        read_net_exts(r, has(h, FLAG_Z), &mut exts, |e| {
            match e.id {
                EXT_TARGET => {
                    target = match e.z64()? {
                        0 => QueryTarget::BestMatching,
                        1 => QueryTarget::All,
                        2 => QueryTarget::AllComplete,
                        _ => return Err(CodecError::Invalid),
                    }
                }
                EXT_BUDGET => {
                    // A zero budget is refused, as zenoh-codec does.
                    let b = e.z64()? as u32;
                    if b == 0 {
                        return Err(CodecError::Invalid);
                    }
                    budget = Some(b);
                }
                EXT_TIMEOUT => timeout_ms = Some(e.z64()?),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        let h2 = r.u8()?;
        if mid(h2) != QUERY {
            return Err(CodecError::Invalid);
        }
        let body = Query::read(r, h2)?;
        Ok(Request {
            id,
            wire_expr,
            exts,
            target,
            budget,
            timeout_ms,
            body,
        })
    }
}

impl<'a> Response<'a> {
    /// Writes the message.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        let mut left = net_ext_count(&self.exts, false) + usize::from(self.responder.is_some());
        let mut h = id::RESPONSE | expr_flags(&self.wire_expr);
        if left != 0 {
            h |= FLAG_Z;
        }
        w.u8(h)?;
        w.zint(u64::from(self.rid))?;
        self.wire_expr.write(w)?;
        left = write_qos_ts(w, &self.exts, left)?;
        if let Some(rid) = &self.responder {
            left = left.saturating_sub(1);
            w.u8(if left != 0 {
                EXT_RESPONDER | FLAG_Z
            } else {
                EXT_RESPONDER
            })?;
            w.zint(rid.len() as u64)?;
            rid.write(w)?;
        }
        self.body.write(w)
    }

    fn read(r: &mut Reader<'a>, h: u8) -> Result<Self, CodecError> {
        let rid = r.zu32()?;
        let wire_expr = WireExpr::read(r, has(h, FLAG_N), has(h, FLAG_M))?;
        let mut exts = NetExts::DEFAULT;
        let mut responder = None;
        read_net_exts(r, has(h, FLAG_Z), &mut exts, |e| {
            if e.id == EXT_RESPONDER {
                let mut er = Reader::new(e.zbuf()?);
                responder = Some(EntityGlobalId::read(&mut er)?);
                return Ok(true);
            }
            Ok(false)
        })?;
        let body = ResponseBody::read(r)?;
        Ok(Response {
            rid,
            wire_expr,
            exts,
            responder,
            body,
        })
    }
}

impl<'a> ResponseFinal<'a> {
    /// Writes the message.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        let left = net_ext_count(&self.exts, false);
        w.u8(if left != 0 {
            id::RESPONSE_FINAL | FLAG_Z
        } else {
            id::RESPONSE_FINAL
        })?;
        w.zint(u64::from(self.rid))?;
        write_qos_ts(w, &self.exts, left).map(|_| ())
    }

    fn read(r: &mut Reader<'a>, h: u8) -> Result<Self, CodecError> {
        let rid = r.zu32()?;
        let mut exts = NetExts::DEFAULT;
        read_net_exts(r, has(h, FLAG_Z), &mut exts, |_| Ok(false))?;
        Ok(ResponseFinal { rid, exts })
    }
}

const D_KEYEXPR: u8 = 0x00;
const U_KEYEXPR: u8 = 0x01;
const D_SUBSCRIBER: u8 = 0x02;
const U_SUBSCRIBER: u8 = 0x03;
const D_QUERYABLE: u8 = 0x04;
const U_QUERYABLE: u8 = 0x05;
const D_TOKEN: u8 = 0x06;
const U_TOKEN: u8 = 0x07;
const D_FINAL: u8 = 0x1a;
const EXT_WIRE_EXPR: u8 = 0x5f;
const EXT_QUERYABLE_INFO: u8 = 0x21;

/// Writes the wire-expression extension of an undeclaration.
fn write_wire_expr_ext(w: &mut Writer<'_>, we: &WireExpr<'_>) -> Result<(), CodecError> {
    let mut flags = 0u8;
    if we.has_suffix() {
        flags |= 1;
    }
    if we.sender_mapping {
        flags |= 2;
    }
    let len = 1 + zint_len(u64::from(we.scope)) + we.suffix.len();
    w.u8(EXT_WIRE_EXPR)?;
    w.zint(len as u64)?;
    w.u8(flags)?;
    w.zint(u64::from(we.scope))?;
    w.bytes(we.suffix.as_bytes())
}

fn read_wire_expr_ext(body: &[u8]) -> Result<WireExpr<'_>, CodecError> {
    let mut r = Reader::new(body);
    let flags = r.u8()?;
    let scope = r.zint()? as u16;
    let suffix = if has(flags, 1) {
        core::str::from_utf8(r.rest()).map_err(|_| CodecError::Utf8)?
    } else {
        ""
    };
    Ok(WireExpr {
        scope,
        suffix,
        sender_mapping: has(flags, 2),
    })
}

const NULL_EXPR: WireExpr<'static> = WireExpr::plain("");

fn read_undeclare<'a>(
    r: &mut Reader<'a>,
    h: u8,
) -> Result<(u32, Option<WireExpr<'a>>), CodecError> {
    let id = r.zu32()?;
    let mut we = None;
    let mut more = has(h, FLAG_Z);
    while more {
        let (e, m) = read_ext(r)?;
        match e.id {
            EXT_WIRE_EXPR => {
                let x = read_wire_expr_ext(e.zbuf()?)?;
                we = (x != NULL_EXPR).then_some(x);
            }
            _ => e.skip()?,
        }
        more = m;
    }
    Ok((id, we))
}

impl<'a> DeclareBody<'a> {
    /// Writes the body.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        match self {
            DeclareBody::KeyExpr { id, wire_expr } => {
                let n = if wire_expr.has_suffix() { FLAG_N } else { 0 };
                w.u8(D_KEYEXPR | n)?;
                w.zint(u64::from(*id))?;
                wire_expr.write(w)
            }
            DeclareBody::UndeclareKeyExpr { id } => {
                w.u8(U_KEYEXPR)?;
                w.zint(u64::from(*id))
            }
            DeclareBody::Subscriber { id, wire_expr } => {
                w.u8(D_SUBSCRIBER | expr_flags(wire_expr))?;
                w.zint(u64::from(*id))?;
                wire_expr.write(w)
            }
            DeclareBody::UndeclareSubscriber { id, wire_expr } => {
                w.u8(if wire_expr.is_some() {
                    U_SUBSCRIBER | FLAG_Z
                } else {
                    U_SUBSCRIBER
                })?;
                w.zint(u64::from(*id))?;
                match wire_expr {
                    Some(we) => write_wire_expr_ext(w, we),
                    None => Ok(()),
                }
            }
            DeclareBody::Queryable {
                id,
                wire_expr,
                complete,
                distance,
            } => {
                let info = *complete || *distance != 0;
                let z = if info { FLAG_Z } else { 0 };
                w.u8(D_QUERYABLE | expr_flags(wire_expr) | z)?;
                w.zint(u64::from(*id))?;
                wire_expr.write(w)?;
                if info {
                    let v = u64::from(*complete) | (u64::from(*distance) << 8);
                    write_ext_z64(w, EXT_QUERYABLE_INFO, v, false)?;
                }
                Ok(())
            }
            DeclareBody::UndeclareQueryable { id, wire_expr }
            | DeclareBody::UndeclareToken { id, wire_expr } => {
                let base = if matches!(self, DeclareBody::UndeclareQueryable { .. }) {
                    U_QUERYABLE
                } else {
                    U_TOKEN
                };
                w.u8(base | FLAG_Z)?;
                w.zint(u64::from(*id))?;
                write_wire_expr_ext(w, wire_expr.as_ref().unwrap_or(&NULL_EXPR))
            }
            DeclareBody::Token { id, wire_expr } => {
                w.u8(D_TOKEN | expr_flags(wire_expr))?;
                w.zint(u64::from(*id))?;
                wire_expr.write(w)
            }
            DeclareBody::Final => w.u8(D_FINAL),
        }
    }

    fn read(r: &mut Reader<'a>) -> Result<Self, CodecError> {
        let h = r.u8()?;
        let named = has(h, FLAG_N);
        let mapping = has(h, FLAG_M);
        let z = has(h, FLAG_Z);
        Ok(match mid(h) {
            D_KEYEXPR => {
                let id = r.zu16()?;
                let wire_expr = WireExpr::read(r, named, false)?;
                skip_exts(r, z)?;
                DeclareBody::KeyExpr { id, wire_expr }
            }
            U_KEYEXPR => {
                let id = r.zu16()?;
                skip_exts(r, z)?;
                DeclareBody::UndeclareKeyExpr { id }
            }
            D_SUBSCRIBER => {
                let id = r.zu32()?;
                let wire_expr = WireExpr::read(r, named, mapping)?;
                skip_exts(r, z)?;
                DeclareBody::Subscriber { id, wire_expr }
            }
            U_SUBSCRIBER => {
                let (id, wire_expr) = read_undeclare(r, h)?;
                DeclareBody::UndeclareSubscriber { id, wire_expr }
            }
            D_QUERYABLE => {
                let id = r.zu32()?;
                let wire_expr = WireExpr::read(r, named, mapping)?;
                let (mut complete, mut distance) = (false, 0u16);
                let mut more = z;
                while more {
                    let (e, m) = read_ext(r)?;
                    match e.id {
                        EXT_QUERYABLE_INFO => {
                            let v = e.z64()?;
                            complete = v & 1 != 0;
                            distance = (v >> 8) as u16;
                        }
                        _ => e.skip()?,
                    }
                    more = m;
                }
                DeclareBody::Queryable {
                    id,
                    wire_expr,
                    complete,
                    distance,
                }
            }
            U_QUERYABLE => {
                let (id, wire_expr) = read_undeclare(r, h)?;
                DeclareBody::UndeclareQueryable { id, wire_expr }
            }
            D_TOKEN => {
                let id = r.zu32()?;
                let wire_expr = WireExpr::read(r, named, mapping)?;
                skip_exts(r, z)?;
                DeclareBody::Token { id, wire_expr }
            }
            U_TOKEN => {
                let (id, wire_expr) = read_undeclare(r, h)?;
                DeclareBody::UndeclareToken { id, wire_expr }
            }
            D_FINAL => {
                skip_exts(r, z)?;
                DeclareBody::Final
            }
            _ => return Err(CodecError::Invalid),
        })
    }
}

impl<'a> Declare<'a> {
    /// Writes the message.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        let left = net_ext_count(&self.exts, true);
        let mut h = id::DECLARE;
        if self.interest_id.is_some() {
            h |= 1 << 5;
        }
        if left != 0 {
            h |= FLAG_Z;
        }
        w.u8(h)?;
        if let Some(i) = self.interest_id {
            w.zint(u64::from(i))?;
        }
        let left = write_qos_ts(w, &self.exts, left)?;
        write_node_id(w, &self.exts, left)?;
        self.body.write(w)
    }

    fn read(r: &mut Reader<'a>, h: u8) -> Result<Self, CodecError> {
        let interest_id = if has(h, 1 << 5) {
            Some(r.zu32()?)
        } else {
            None
        };
        let mut exts = NetExts::DEFAULT;
        read_net_exts(r, has(h, FLAG_Z), &mut exts, |_| Ok(false))?;
        let body = DeclareBody::read(r)?;
        Ok(Declare {
            interest_id,
            exts,
            body,
        })
    }
}

impl<'a> Interest<'a> {
    /// The option byte as written: `options` with restricted, named and
    /// mapping taken from the wire expression when there is one (without
    /// one only the restricted bit is cleared, as `zenoh-codec` does).
    pub fn wire_options(&self) -> u8 {
        match &self.wire_expr {
            Some(we) => {
                let mut o =
                    (self.options | interest::RESTRICTED) & !(interest::NAMED | interest::MAPPING);
                if we.has_suffix() {
                    o |= interest::NAMED;
                }
                if we.sender_mapping {
                    o |= interest::MAPPING;
                }
                o
            }
            // As zenoh-codec: only the restricted bit is cleared.
            None => self.options & !interest::RESTRICTED,
        }
    }

    /// Writes the message.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        let left = net_ext_count(&self.exts, true);
        let mode = match self.mode {
            InterestMode::Final => 0b00,
            InterestMode::Current => 0b01,
            InterestMode::Future => 0b10,
            InterestMode::CurrentFuture => 0b11,
        };
        let mut h = id::INTEREST | (mode << 5);
        if left != 0 {
            h |= FLAG_Z;
        }
        w.u8(h)?;
        w.zint(u64::from(self.id))?;
        if self.mode != InterestMode::Final {
            w.u8(self.wire_options())?;
            if let Some(we) = &self.wire_expr {
                we.write(w)?;
            }
        }
        let left = write_qos_ts(w, &self.exts, left)?;
        write_node_id(w, &self.exts, left).map(|_| ())
    }

    fn read(r: &mut Reader<'a>, h: u8) -> Result<Self, CodecError> {
        // zenoh-codec keeps the low 32 bits of the id.
        let id = r.zint()? as u32;
        let mode = match (h >> 5) & 0b11 {
            0b00 => InterestMode::Final,
            0b01 => InterestMode::Current,
            0b10 => InterestMode::Future,
            _ => InterestMode::CurrentFuture,
        };
        let (mut options, mut wire_expr) = (0u8, None);
        if mode != InterestMode::Final {
            options = r.u8()?;
            if has(options, interest::RESTRICTED) {
                wire_expr = Some(WireExpr::read(
                    r,
                    has(options, interest::NAMED),
                    has(options, interest::MAPPING),
                )?);
            }
        }
        let mut exts = NetExts::DEFAULT;
        read_net_exts(r, has(h, 1 << 7), &mut exts, |_| Ok(false))?;
        Ok(Interest {
            id,
            mode,
            options,
            wire_expr,
            exts,
        })
    }
}

impl<'a> NetworkMessage<'a> {
    /// Writes the message (an OAM writes nothing and is refused).
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        match self {
            NetworkMessage::Push(m) => m.write(w),
            NetworkMessage::Request(m) => m.write(w),
            NetworkMessage::Response(m) => m.write(w),
            NetworkMessage::ResponseFinal(m) => m.write(w),
            NetworkMessage::Declare(m) => m.write(w),
            NetworkMessage::Interest(m) => m.write(w),
            NetworkMessage::Oam => Err(CodecError::Invalid),
        }
    }

    /// Reads a network message whose header byte `h` was already read.
    pub fn read(r: &mut Reader<'a>, h: u8) -> Result<Self, CodecError> {
        Ok(match mid(h) {
            id::PUSH => NetworkMessage::Push(Push::read(r, h)?),
            id::REQUEST => NetworkMessage::Request(Request::read(r, h)?),
            id::RESPONSE => NetworkMessage::Response(Response::read(r, h)?),
            id::RESPONSE_FINAL => NetworkMessage::ResponseFinal(ResponseFinal::read(r, h)?),
            id::DECLARE => NetworkMessage::Declare(Declare::read(r, h)?),
            id::INTEREST => NetworkMessage::Interest(Interest::read(r, h)?),
            id::OAM => {
                let _id = r.zint_max(u64::from(u16::MAX))?;
                let mut exts = NetExts::DEFAULT;
                read_net_exts(r, has(h, FLAG_Z), &mut exts, |_| Ok(false))?;
                super::transport::skip_oam_body(r, h)?;
                NetworkMessage::Oam
            }
            _ => return Err(CodecError::Invalid),
        })
    }

    /// Reads a whole network message, header included.
    pub fn decode(bytes: &'a [u8]) -> Result<(Self, usize), CodecError> {
        let mut r = Reader::new(bytes);
        let h = r.u8()?;
        let m = Self::read(&mut r, h)?;
        Ok((m, bytes.len() - r.remaining()))
    }
}

#[cfg(test)]
mod tests;
