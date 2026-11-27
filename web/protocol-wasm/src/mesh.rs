//! Browser mesh (design.md BM…): what two pages say to each other over a
//! WebRTC data channel, and what they say to a PeerJS-compatible server
//! to find each other. Pure: the page's TypeScript owns the `WebSocket`
//! and the `RTCPeerConnection` and calls in here for every byte format
//! and every decision.
//!
//! * **Rooms and names** ([`valid_room`], [`room_key`], [`auto_binding`]):
//!   every channel a payload declares and the user has not bound is bound
//!   to the key `room/<room>/<name>`, so an output publishes there and an
//!   input subscribes there, whoever the other peers are.
//! * **The link** ([`publish_out`], [`inbound`], [`hello_request`],
//!   [`hello_reply`]): one data-channel message is one raw frame, as a UDP
//!   datagram carries one ([`middleware::udp`]). Publish frames carry
//!   channel data. Requests are answered for three read-only keys
//!   ([`HELLO_KEYS`]) and `NotFound` for anything else.
//! * **The gate** ([`outbound`], [`inbound`]): `sys/…` and the
//!   node-internal `ch/…` and `drv/…` keys never cross the room boundary in
//!   either direction, and an inbound publication is at most
//!   [`MAX_MESSAGE`] bytes. Peers are not authenticated: the room carries
//!   data only.
//! * **Ids and dialling** ([`peer_id`], [`hub_id`], [`parse_peer_id`],
//!   [`room_peers`], [`dials`], [`dial_plan`]): a node is
//!   `mw-<room>-<node16>` on the server, the lower id dials, and a node
//!   holds at most [`MAX_PEERS`] links.
//! * **Signalling** ([`Server`], [`Signal`]): the PeerJS server protocol's
//!   URLs and JSON messages, in the shapes a stock `peerjs-server` relays.
//! * **Rendezvous** ([`HubMsg`]): when the server does not list its
//!   peers, the page that holds the id `mw-<room>-hub` tells the others
//!   who is there.
//! * **Channels of a peer** ([`compare`]): a peer's declarations next to
//!   the local ones, by name; shown, not enforced.

use middleware::loader::binding::{BINDING_MAX, Binding, Target, is_internal_key};
use middleware::loader::trust::Version;
use middleware::loader::{is_system_key, keys};
use middleware::serial::frame::Frame;
use middleware::udp::{self, MAX_DATAGRAM};
use middleware::wasm_abi::channels::{Channel, Channels, DataKind, Direction};
use middleware::{HandlerError, Key, SenderId};

/// Most links a node holds (the peers a board remembers, `sys/peers`).
pub const MAX_PEERS: usize = middleware::loader::peers::MAX_PEERS;

/// Largest publication that crosses the room boundary: the ABI's largest
/// message.
pub const MAX_MESSAGE: usize = middleware::wasm_abi::MAX_MESSAGE;

/// Longest room name.
pub const MAX_ROOM: usize = 24;

/// Most members a rendezvous hub lists.
pub const MAX_MEMBERS: usize = 64;

/// The key a peer answers with its payload's `mw.channels` section bytes
/// (empty without one).
pub const CHANNELS_KEY: &str = "sys/payload/channels";

/// The keys a peer answers over the room: its `sys/info`, its
/// `sys/manifest` and [`CHANNELS_KEY`].
pub const HELLO_KEYS: [&str; 3] = ["sys/info", "sys/manifest", CHANNELS_KEY];

/// Whether `room` names a room: 1 to [`MAX_ROOM`] of `a-z`, `0-9`, `_`
/// (key bytes, valid in a PeerJS id, and without the `-` the ids are
/// split on).
pub fn valid_room(room: &str) -> bool {
    !room.is_empty()
        && room.len() <= MAX_ROOM
        && room
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// The key channel `name` is carried on in `room`: `room/<room>/<name>`;
/// `None` for an invalid room or a name that is not a key segment.
pub fn room_key(room: &str, name: &str) -> Option<String> {
    if !valid_room(room) || name.contains('/') {
        return None;
    }
    let k = format!("room/{room}/{name}");
    Key::new(&k).ok()?;
    Some(k)
}

/// A binding record for `module` in which every declared channel that
/// `existing` (an encoded record, if any) does not bind is bound to its
/// [`room_key`]. The user's entries are kept as they are; without
/// `existing` the record is for this image at version 0.
pub fn auto_binding(module: &[u8], existing: Option<&[u8]>, room: &str) -> Result<Vec<u8>, String> {
    if !valid_room(room) {
        return Err(format!("a room name is 1 to {MAX_ROOM} of a-z, 0-9 and _"));
    }
    let channels = Channels::from_module(module).map_err(|e| format!("mw.channels: {e:?}"))?;
    let keys: Vec<(&str, String)> = channels
        .iter()
        .filter_map(|c| Some((c.name, room_key(room, c.name)?)))
        .collect();
    let mut binding = match existing {
        Some(bytes) => Binding::decode(bytes).map_err(|e| format!("binding: {e:?}"))?,
        None => Binding::new(crate::loader::sha256(module), Version(0)),
    };
    for (name, key) in &keys {
        if binding.target(name).is_some() {
            continue;
        }
        let key = Key::new(key).map_err(|_| format!("bad key {key:?}"))?;
        binding
            .bind(name, Target::Key(key))
            .map_err(|e| format!("{name}: {e:?}"))?;
    }
    let mut out = vec![0u8; BINDING_MAX];
    let n = binding
        .encode(&mut out)
        .ok_or("the binding does not fit a record")?;
    out.truncate(n);
    Ok(out)
}

/// Whether a publication on `key` may leave the node for the room, or
/// enter from it: a valid key that is neither a system key (`sys/…`) nor
/// node-internal (`ch/…`, `drv/…`).
pub fn outbound(key: &str) -> bool {
    Key::new(key).is_ok_and(|k| !is_system_key(k) && !is_internal_key(k))
}

fn datagram(frame: &Frame<'_>) -> Option<Vec<u8>> {
    let mut out = vec![0u8; MAX_DATAGRAM];
    let n = udp::encode(frame, &mut out).ok()?;
    out.truncate(n);
    Some(out)
}

/// The data-channel message for a local publication, `None` when the gate
/// refuses it ([`outbound`], [`MAX_MESSAGE`]).
pub fn publish_out(key: &str, data: &[u8]) -> Option<Vec<u8>> {
    if !outbound(key) || data.len() > MAX_MESSAGE {
        return None;
    }
    datagram(&Frame::Publish {
        key: Key::new(key).ok()?,
        payload: data,
    })
}

/// Why a message from a peer was not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Not a frame (length, CRC, kind, key bytes).
    Malformed,
    /// A publication on a system or node-internal key.
    Key,
    /// A publication longer than [`MAX_MESSAGE`].
    Oversize,
}

impl Refusal {
    /// A short name for the page's counters.
    pub const fn name(self) -> &'static str {
        match self {
            Refusal::Malformed => "malformed",
            Refusal::Key => "key",
            Refusal::Oversize => "oversize",
        }
    }
}

/// One message from a peer, after the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inbound<'a> {
    /// A publication to deliver to the local payloads, with the peer's
    /// node as sender.
    Publish {
        /// The key.
        key: &'a str,
        /// The data.
        data: &'a [u8],
    },
    /// A request; answer it with [`hello_reply`].
    Request {
        /// Correlation id.
        id: u8,
        /// The key asked for.
        key: &'a str,
    },
    /// The answer to one of our hello requests.
    Reply {
        /// Correlation id.
        id: u8,
        /// The body, or the peer's error.
        result: Result<&'a [u8], HandlerError>,
    },
    /// Dropped; count it.
    Refused(Refusal),
}

/// Reads one data-channel message from a peer.
pub fn inbound(message: &[u8]) -> Inbound<'_> {
    match udp::decode(message) {
        Err(_) => Inbound::Refused(Refusal::Malformed),
        Ok(Frame::Publish { key, payload }) => {
            if is_system_key(key) || is_internal_key(key) {
                Inbound::Refused(Refusal::Key)
            } else if payload.len() > MAX_MESSAGE {
                Inbound::Refused(Refusal::Oversize)
            } else {
                Inbound::Publish {
                    key: key.as_str(),
                    data: payload,
                }
            }
        }
        Ok(Frame::Request { id, key, .. }) => Inbound::Request {
            id,
            key: key.as_str(),
        },
        Ok(Frame::Response { id, result }) => Inbound::Reply { id, result },
    }
}

/// The request for one of [`HELLO_KEYS`]; `None` for any other key.
pub fn hello_request(id: u8, key: &str) -> Option<Vec<u8>> {
    HELLO_KEYS.contains(&key).then_some(())?;
    datagram(&Frame::Request {
        id,
        key: Key::new(key).ok()?,
        payload: &[],
    })
}

/// The answer to a peer's request for `key`: this node's `info`
/// (`sys/info` body), `manifest` (`sys/manifest` body) or `channels`
/// (section bytes), and `NotFound` for every other key: nothing else of a
/// node is reachable from the room.
pub fn hello_reply(id: u8, key: &str, info: &[u8], manifest: &[u8], channels: &[u8]) -> Vec<u8> {
    let body = if key == keys::INFO.as_str() {
        Ok(info)
    } else if key == keys::MANIFEST.as_str() {
        Ok(manifest)
    } else if key == CHANNELS_KEY {
        Ok(channels)
    } else {
        Err(HandlerError::NotFound)
    };
    datagram(&Frame::Response { id, result: body })
        .or_else(|| {
            datagram(&Frame::Response {
                id,
                result: Err(HandlerError::BufferTooSmall),
            })
        })
        .unwrap_or_default()
}

/// The `mw.channels` section bytes of a module, empty without one.
pub fn channels_section(module: &[u8]) -> Vec<u8> {
    middleware::wasm_abi::channels::read_custom_section(
        module,
        middleware::wasm_abi::channels::SECTION,
    )
    .ok()
    .flatten()
    .map(<[u8]>::to_vec)
    .unwrap_or_default()
}

fn hex16(node: SenderId) -> String {
    node.0.iter().map(|b| format!("{b:02x}")).collect()
}

/// A node's id on the signalling server: `mw-<room>-<node16>`.
pub fn peer_id(room: &str, node: SenderId) -> Option<String> {
    valid_room(room).then(|| format!("mw-{room}-{}", hex16(node)))
}

/// The rendezvous id of a room: `mw-<room>-hub`.
pub fn hub_id(room: &str) -> Option<String> {
    valid_room(room).then(|| format!("mw-{room}-hub"))
}

/// The node an id of `room` names; `None` for another room's id, the hub
/// or anything else on the server.
pub fn parse_peer_id(room: &str, id: &str) -> Option<SenderId> {
    if !valid_room(room) {
        return None;
    }
    let hex = id
        .strip_prefix("mw-")?
        .strip_prefix(room)?
        .strip_prefix('-')?
        .as_bytes();
    if hex.len() != 16 {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut node = [0u8; 8];
    for (byte, pair) in node.iter_mut().zip(hex.chunks_exact(2)) {
        *byte = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(SenderId(node))
}

/// The other nodes of `room` among `ids` (a server's listing or a hub's):
/// this room's node ids without `own`, sorted, each once.
pub fn room_peers(room: &str, own: &str, ids: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = ids
        .iter()
        .filter(|id| **id != own && parse_peer_id(room, id).is_some())
        .map(|id| (*id).to_owned())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Whether `own` opens the connection to `other`: the lower id dials, so
/// two offers never cross.
pub fn dials(own: &str, other: &str) -> bool {
    own < other
}

/// Whether a node already holding `linked` links takes another.
pub const fn has_room(linked: usize) -> bool {
    linked < MAX_PEERS
}

/// The peers to dial now: those of `listed` that `own` dials and is not
/// linked (or linking) to, lowest first, as many as fit under
/// [`MAX_PEERS`] links. The rest stay listed.
pub fn dial_plan(own: &str, listed: &[&str], linked: &[&str]) -> Vec<String> {
    let mut want: Vec<&str> = listed
        .iter()
        .copied()
        .filter(|id| dials(own, id) && !linked.contains(id))
        .collect();
    want.sort_unstable();
    want.dedup();
    want.truncate(MAX_PEERS.saturating_sub(linked.len()));
    want.into_iter().map(str::to_owned).collect()
}

/// Where a PeerJS-compatible server is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    /// Host name.
    pub host: String,
    /// Port.
    pub port: u16,
    /// The path the server is mounted at (`/` by default).
    pub path: String,
    /// The server's key (`peerjs` by default).
    pub key: String,
    /// TLS (`wss` and `https`).
    pub secure: bool,
}

fn url_part(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

impl Server {
    fn base(&self) -> Option<String> {
        let host_ok = !self.host.is_empty()
            && self
                .host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-.:[]".contains(&b));
        let path = self.path.trim_matches('/');
        let path_ok = path.split('/').all(|seg| seg.is_empty() || url_part(seg));
        if !host_ok || !path_ok || !url_part(&self.key) {
            return None;
        }
        let sep = if path.is_empty() { "" } else { "/" };
        Some(format!("{}:{}/{path}{sep}", self.host, self.port))
    }

    /// The signalling socket's URL for `id` with the session `token`.
    pub fn socket_url(&self, id: &str, token: &str) -> Option<String> {
        if !url_part(id) || !url_part(token) {
            return None;
        }
        let scheme = if self.secure { "wss" } else { "ws" };
        Some(format!(
            "{scheme}://{}peerjs?key={}&id={id}&token={token}",
            self.base()?,
            self.key
        ))
    }

    /// The URL that lists the server's peers (a server started with
    /// `allow_discovery`; others answer 401).
    pub fn peers_url(&self) -> Option<String> {
        let scheme = if self.secure { "https" } else { "http" };
        Some(format!("{scheme}://{}{}/peers", self.base()?, self.key))
    }
}

/// The ids in a server's peer listing (a JSON array of strings).
pub fn parse_listing(text: &str) -> Option<Vec<String>> {
    match json::parse(text)? {
        json::Value::Arr(items) => Some(
            items
                .into_iter()
                .filter_map(|v| match v {
                    json::Value::Str(s) => Some(s),
                    _ => None,
                })
                .collect(),
        ),
        _ => None,
    }
}

/// One signalling message. `peer` is the other party: the destination of
/// a message we send ([`Signal::encode`]), the source of one the server
/// relays ([`Signal::decode`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    /// The server accepted our id.
    Open,
    /// Our id is in use.
    IdTaken,
    /// The server refused us (its message).
    Error(String),
    /// Keeps the socket open; sent every [`HEARTBEAT_MS`].
    Heartbeat,
    /// `peer` closed its connection to us.
    Leave {
        /// The peer.
        peer: String,
    },
    /// A message to `peer` could not be delivered.
    Expire {
        /// The peer.
        peer: String,
    },
    /// An offer of a data connection.
    Offer {
        /// The peer.
        peer: String,
        /// The connection's id, repeated by its answer and candidates.
        connection: String,
        /// The data channel's label.
        label: String,
        /// The SDP.
        sdp: String,
    },
    /// The answer to an offer.
    Answer {
        /// The peer.
        peer: String,
        /// The connection's id.
        connection: String,
        /// The SDP.
        sdp: String,
    },
    /// An ICE candidate.
    Candidate {
        /// The peer.
        peer: String,
        /// The connection's id.
        connection: String,
        /// The candidate line.
        candidate: String,
        /// Its media id, if any.
        mid: Option<String>,
        /// Its media line index, if any.
        mline: Option<u32>,
    },
}

/// How often a signalling socket sends [`Signal::Heartbeat`].
pub const HEARTBEAT_MS: u32 = 5000;

impl Signal {
    /// The message's JSON text, addressed to `peer`.
    pub fn encode(&self) -> String {
        use json::Value::{Bool, Num, Obj, Str};
        let s = |v: &str| Str(v.to_owned());
        let field = |k: &str, v: json::Value| (k.to_owned(), v);
        let plain = |t: &str| Obj(vec![field("type", s(t))]);
        let to = |t: &str, peer: &str, payload: Vec<(String, json::Value)>| {
            Obj(vec![
                field("type", s(t)),
                field("payload", Obj(payload)),
                field("dst", s(peer)),
            ])
        };
        let sdp = |kind: &str, text: &str| Obj(vec![field("type", s(kind)), field("sdp", s(text))]);
        let v = match self {
            Signal::Open => plain("OPEN"),
            Signal::IdTaken => plain("ID-TAKEN"),
            Signal::Heartbeat => plain("HEARTBEAT"),
            Signal::Error(msg) => Obj(vec![
                field("type", s("ERROR")),
                field("payload", Obj(vec![field("msg", s(msg))])),
            ]),
            Signal::Leave { peer } => Obj(vec![field("type", s("LEAVE")), field("dst", s(peer))]),
            Signal::Expire { peer } => Obj(vec![field("type", s("EXPIRE")), field("dst", s(peer))]),
            Signal::Offer {
                peer,
                connection,
                label,
                sdp: text,
            } => to(
                "OFFER",
                peer,
                vec![
                    field("sdp", sdp("offer", text)),
                    field("type", s("data")),
                    field("connectionId", s(connection)),
                    field("label", s(label)),
                    field("reliable", Bool(true)),
                    field("serialization", s("raw")),
                ],
            ),
            Signal::Answer {
                peer,
                connection,
                sdp: text,
            } => to(
                "ANSWER",
                peer,
                vec![
                    field("sdp", sdp("answer", text)),
                    field("type", s("data")),
                    field("connectionId", s(connection)),
                ],
            ),
            Signal::Candidate {
                peer,
                connection,
                candidate,
                mid,
                mline,
            } => {
                let mut c = vec![field("candidate", s(candidate))];
                if let Some(m) = mid {
                    c.push(field("sdpMid", s(m)));
                }
                if let Some(i) = mline {
                    c.push(field("sdpMLineIndex", Num(f64::from(*i))));
                }
                to(
                    "CANDIDATE",
                    peer,
                    vec![
                        field("candidate", Obj(c)),
                        field("type", s("data")),
                        field("connectionId", s(connection)),
                    ],
                )
            }
        };
        json::write(&v)
    }

    /// Reads a message from the server; `None` for anything that is not
    /// one of these shapes (another kind of connection, a missing field).
    pub fn decode(text: &str) -> Option<Signal> {
        let v = json::parse(text)?;
        let kind = v.get("type")?.str()?;
        let peer = || Some(v.get("src")?.str()?.to_owned());
        let payload = || v.get("payload");
        let connection = || Some(payload()?.get("connectionId")?.str()?.to_owned());
        let data = || (payload()?.get("type")?.str()? == "data").then_some(());
        let sdp = |want: &str| {
            let d = payload()?.get("sdp")?;
            (d.get("type")?.str()? == want).then_some(())?;
            Some(d.get("sdp")?.str()?.to_owned())
        };
        Some(match kind {
            "OPEN" => Signal::Open,
            "ID-TAKEN" => Signal::IdTaken,
            "HEARTBEAT" => Signal::Heartbeat,
            "ERROR" => Signal::Error(
                payload()
                    .and_then(|p| p.get("msg"))
                    .and_then(json::Value::str)
                    .unwrap_or("error")
                    .to_owned(),
            ),
            "LEAVE" => Signal::Leave { peer: peer()? },
            "EXPIRE" => Signal::Expire { peer: peer()? },
            "OFFER" => {
                data()?;
                let connection = connection()?;
                Signal::Offer {
                    peer: peer()?,
                    label: payload()?
                        .get("label")
                        .and_then(json::Value::str)
                        .unwrap_or(&connection)
                        .to_owned(),
                    connection,
                    sdp: sdp("offer")?,
                }
            }
            "ANSWER" => {
                data()?;
                Signal::Answer {
                    peer: peer()?,
                    connection: connection()?,
                    sdp: sdp("answer")?,
                }
            }
            "CANDIDATE" => {
                data()?;
                let c = payload()?.get("candidate")?;
                Signal::Candidate {
                    peer: peer()?,
                    connection: connection()?,
                    candidate: c.get("candidate")?.str()?.to_owned(),
                    mid: c
                        .get("sdpMid")
                        .and_then(json::Value::str)
                        .map(str::to_owned),
                    mline: c
                        .get("sdpMLineIndex")
                        .and_then(json::Value::num)
                        .filter(|n| *n >= 0.0 && *n <= 65535.0 && n.fract() == 0.0)
                        .map(|n| n as u32),
                }
            }
            _ => return None,
        })
    }
}

/// What a rendezvous hub and its members say (text messages on the hub's
/// data channel).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubMsg {
    /// A member to the hub: its id.
    Join(String),
    /// The hub to every member, on each change: the ids in the room.
    Peers(Vec<String>),
}

const HUB_TAG: &str = "mw-hub";

fn id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && url_part(id)
}

impl HubMsg {
    /// The message's text.
    pub fn encode(&self) -> String {
        match self {
            HubMsg::Join(id) => format!("{HUB_TAG} join {id}"),
            HubMsg::Peers(ids) => {
                let mut s = format!("{HUB_TAG} peers");
                for id in ids.iter().take(MAX_MEMBERS) {
                    s.push(' ');
                    s.push_str(id);
                }
                s
            }
        }
    }

    /// Reads a message; `None` for anything else, a malformed id or more
    /// than [`MAX_MEMBERS`] ids.
    pub fn decode(text: &str) -> Option<HubMsg> {
        let mut words = text.split(' ');
        (words.next()? == HUB_TAG).then_some(())?;
        match words.next()? {
            "join" => {
                let id = words.next()?;
                (id_ok(id) && words.next().is_none()).then(|| HubMsg::Join(id.to_owned()))
            }
            "peers" => {
                let ids: Vec<String> = words.map(str::to_owned).collect();
                (ids.len() <= MAX_MEMBERS && ids.iter().all(|i| id_ok(i)))
                    .then_some(HubMsg::Peers(ids))
            }
            _ => None,
        }
    }
}

/// How a channel declared here and by a peer under the same name relate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairState {
    /// One writes what the other reads.
    Connected,
    /// Both read or both write: no data flows between these two.
    SameDirection,
    /// Opposite directions, but the declarations do not fit.
    Mismatch,
}

impl PairState {
    /// The state's name for the page.
    pub const fn name(self) -> &'static str {
        match self {
            PairState::Connected => "connected",
            PairState::SameDirection => "same-direction",
            PairState::Mismatch => "mismatch",
        }
    }
}

/// One channel name both sides declare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pairing {
    /// The channel.
    pub name: String,
    /// How the two declarations relate.
    pub state: PairState,
    /// In plain language.
    pub message: String,
}

fn misfit(out: &Channel<'_>, inp: &Channel<'_>) -> Option<String> {
    if out.kind != inp.kind {
        return Some(format!(
            "the writer sends {}, the reader takes {}",
            out.kind.name(),
            inp.kind.name()
        ));
    }
    if out.kind != DataKind::Samples {
        return None;
    }
    if let (Some(a), Some(b)) = (out.sensor, inp.sensor)
        && a != b
    {
        return Some(format!(
            "the writer sends {a:?} samples, the reader takes {b:?}"
        ));
    }
    if out.formats != 0 && inp.formats != 0 && out.formats & inp.formats == 0 {
        return Some("the two share no sample format".into());
    }
    if out.channels != 0 && inp.channels != 0 && out.channels != inp.channels {
        return Some(format!(
            "the writer sends {} channels per sample, the reader takes {}",
            out.channels, inp.channels
        ));
    }
    if out.rate_max < inp.rate_min || inp.rate_max < out.rate_min {
        return Some(format!(
            "the writer's {}..={} Hz and the reader's {}..={} Hz do not overlap",
            out.rate_min, out.rate_max, inp.rate_min, inp.rate_max
        ));
    }
    None
}

/// The channels `local` and `peer` both declare, in `local`'s order.
pub fn compare(local: &Channels<'_>, peer: &Channels<'_>) -> Vec<Pairing> {
    local
        .iter()
        .filter_map(|l| {
            let (_, p) = peer.get(l.name)?;
            let (state, message) = if l.dir == p.dir {
                let both = if l.dir == Direction::In {
                    "read"
                } else {
                    "write"
                };
                (PairState::SameDirection, format!("both {both} {}", l.name))
            } else {
                let (out, inp) = if l.dir == Direction::Out {
                    (l, p)
                } else {
                    (p, l)
                };
                match misfit(out, inp) {
                    Some(why) => (PairState::Mismatch, format!("{}: {why}", l.name)),
                    None => (
                        PairState::Connected,
                        if l.dir == Direction::Out {
                            format!("{} goes to this peer", l.name)
                        } else {
                            format!("{} comes from this peer", l.name)
                        },
                    ),
                }
            };
            Some(Pairing {
                name: l.name.to_owned(),
                state,
                message,
            })
        })
        .collect()
}

/// The little JSON the PeerJS protocol needs: a value tree, a strict
/// parser with a depth limit, and a writer.
pub mod json {
    /// A JSON value. Objects keep their members in order.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Value {
        /// `null`.
        Null,
        /// `true` or `false`.
        Bool(bool),
        /// A number.
        Num(f64),
        /// A string.
        Str(String),
        /// An array.
        Arr(Vec<Value>),
        /// An object.
        Obj(Vec<(String, Value)>),
    }

    impl Value {
        /// Member `key` of an object.
        pub fn get(&self, key: &str) -> Option<&Value> {
            match self {
                Value::Obj(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            }
        }

        /// The string, if it is one.
        pub fn str(&self) -> Option<&str> {
            match self {
                Value::Str(s) => Some(s),
                _ => None,
            }
        }

        /// The number, if it is one.
        pub fn num(&self) -> Option<f64> {
            match self {
                Value::Num(n) => Some(*n),
                _ => None,
            }
        }
    }

    /// Deepest nesting [`parse`] accepts.
    pub const MAX_DEPTH: usize = 16;

    struct Reader<'a> {
        s: &'a [u8],
        at: usize,
    }

    impl Reader<'_> {
        fn ws(&mut self) {
            while matches!(self.s.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                self.at += 1;
            }
        }

        fn eat(&mut self, lit: &[u8]) -> Option<()> {
            self.s.get(self.at..)?.starts_with(lit).then_some(())?;
            self.at += lit.len();
            Some(())
        }

        fn hex4(&mut self) -> Option<u32> {
            let h = core::str::from_utf8(self.s.get(self.at..self.at + 4)?).ok()?;
            h.bytes().all(|b| b.is_ascii_hexdigit()).then_some(())?;
            self.at += 4;
            u32::from_str_radix(h, 16).ok()
        }

        fn string(&mut self) -> Option<String> {
            self.eat(b"\"")?;
            let mut out = Vec::new();
            loop {
                let b = *self.s.get(self.at)?;
                self.at += 1;
                match b {
                    b'"' => return String::from_utf8(out).ok(),
                    b'\\' => {
                        let e = *self.s.get(self.at)?;
                        self.at += 1;
                        let c = match e {
                            b'"' => '"',
                            b'\\' => '\\',
                            b'/' => '/',
                            b'b' => '\u{8}',
                            b'f' => '\u{c}',
                            b'n' => '\n',
                            b'r' => '\r',
                            b't' => '\t',
                            b'u' => {
                                let hi = self.hex4()?;
                                let code = if (0xD800..0xDC00).contains(&hi) {
                                    self.eat(b"\\u")?;
                                    let lo = self.hex4()?;
                                    (0xDC00..0xE000).contains(&lo).then_some(())?;
                                    0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                                } else {
                                    hi
                                };
                                char::from_u32(code)?
                            }
                            _ => return None,
                        };
                        out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                    }
                    0..=0x1f => return None,
                    _ => out.push(b),
                }
            }
        }

        fn number(&mut self) -> Option<f64> {
            let start = self.at;
            while matches!(
                self.s.get(self.at),
                Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
            ) {
                self.at += 1;
            }
            let n: f64 = core::str::from_utf8(&self.s[start..self.at])
                .ok()?
                .parse()
                .ok()?;
            n.is_finite().then_some(n)
        }

        fn value(&mut self, depth: usize) -> Option<Value> {
            self.ws();
            match *self.s.get(self.at)? {
                b'n' => self.eat(b"null").map(|()| Value::Null),
                b't' => self.eat(b"true").map(|()| Value::Bool(true)),
                b'f' => self.eat(b"false").map(|()| Value::Bool(false)),
                b'"' => self.string().map(Value::Str),
                b'[' if depth < MAX_DEPTH => {
                    self.at += 1;
                    let mut items = Vec::new();
                    self.ws();
                    if self.eat(b"]").is_some() {
                        return Some(Value::Arr(items));
                    }
                    loop {
                        items.push(self.value(depth + 1)?);
                        self.ws();
                        if self.eat(b",").is_none() {
                            self.eat(b"]")?;
                            return Some(Value::Arr(items));
                        }
                    }
                }
                b'{' if depth < MAX_DEPTH => {
                    self.at += 1;
                    let mut members = Vec::new();
                    self.ws();
                    if self.eat(b"}").is_some() {
                        return Some(Value::Obj(members));
                    }
                    loop {
                        self.ws();
                        let k = self.string()?;
                        self.ws();
                        self.eat(b":")?;
                        members.push((k, self.value(depth + 1)?));
                        self.ws();
                        if self.eat(b",").is_none() {
                            self.eat(b"}")?;
                            return Some(Value::Obj(members));
                        }
                    }
                }
                b'-' | b'0'..=b'9' => self.number().map(Value::Num),
                _ => None,
            }
        }
    }

    /// Parses one JSON text; `None` when it is not one (or nests deeper
    /// than [`MAX_DEPTH`]).
    pub fn parse(text: &str) -> Option<Value> {
        let mut r = Reader {
            s: text.as_bytes(),
            at: 0,
        };
        let v = r.value(0)?;
        r.ws();
        (r.at == r.s.len()).then_some(v)
    }

    fn write_str(s: &str, out: &mut String) {
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
    }

    fn write_into(v: &Value, out: &mut String) {
        match v {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Num(n) => out.push_str(&format!("{n}")),
            Value::Str(s) => write_str(s, out),
            Value::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_into(item, out);
                }
                out.push(']');
            }
            Value::Obj(members) => {
                out.push('{');
                for (i, (k, item)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_str(k, out);
                    out.push(':');
                    write_into(item, out);
                }
                out.push('}');
            }
        }
    }

    /// The JSON text of `v`.
    pub fn write(v: &Value) -> String {
        let mut out = String::new();
        write_into(v, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODE: SenderId = SenderId([0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77]);

    /// A module with a `mw.channels` section holding `decls`.
    fn module(decls: &[Channel<'_>]) -> Vec<u8> {
        use middleware::wasm_abi::channels::{ANY, SECTION, VERSION};
        let mut section = vec![VERSION, decls.len() as u8];
        for c in decls {
            section.push(c.name.len() as u8);
            section.extend_from_slice(c.name.as_bytes());
            section.extend_from_slice(&[
                c.dir as u8,
                c.kind as u8,
                c.sensor.map_or(ANY, |s| s as u8),
                c.formats,
                c.channels,
            ]);
            section.extend_from_slice(&c.rate_min.to_le_bytes());
            section.extend_from_slice(&c.rate_max.to_le_bytes());
            section.push(u8::from(c.required));
        }
        assert_eq!(Channels::parse(&section).map(|c| c.len()), Ok(decls.len()));
        let mut m = b"\0asm\x01\0\0\0".to_vec();
        m.push(0);
        m.push((1 + SECTION.len() + section.len()) as u8);
        m.push(SECTION.len() as u8);
        m.extend_from_slice(SECTION.as_bytes());
        m.extend_from_slice(&section);
        m
    }

    fn decls() -> [Channel<'static>; 3] {
        [
            Channel::bytes("audio", Direction::In, true),
            Channel::bytes("level", Direction::Out, false),
            Channel::bytes("extra", Direction::Out, false),
        ]
    }

    #[test]
    fn bm1_rooms_and_room_keys() {
        for ok in ["lab", "a", "room_2", &"r".repeat(MAX_ROOM)] {
            assert!(valid_room(ok), "{ok}");
        }
        for bad in [
            "",
            "Lab",
            "a-b",
            "a/b",
            "a b",
            "é",
            &"r".repeat(MAX_ROOM + 1),
        ] {
            assert!(!valid_room(bad), "{bad}");
            assert_eq!(room_key(bad, "level"), None);
        }
        assert_eq!(room_key("lab", "level").as_deref(), Some("room/lab/level"));
        assert_eq!(room_key("lab", "a/b"), None);
        assert_eq!(room_key("lab", ""), None);
        // The longest room and channel name still make a key.
        let longest = room_key(&"r".repeat(MAX_ROOM), &"n".repeat(24)).unwrap();
        assert!(longest.len() <= middleware::key::MAX_KEY_LEN);
    }

    #[test]
    fn bm2_unbound_channels_bind_to_their_room_key_and_user_entries_stay() {
        let m = module(&decls());
        let record = auto_binding(&m, None, "lab").unwrap();
        let b = Binding::decode(&record).unwrap();
        assert_eq!(b.image_sha256, crate::loader::sha256(&m));
        let key = |s: &'static str| Target::Key(Key::new(s).unwrap());
        assert_eq!(b.target("audio"), Some(key("room/lab/audio")));
        assert_eq!(b.target("level"), Some(key("room/lab/level")));
        assert_eq!(b.target("extra"), Some(key("room/lab/extra")));

        // A binding the user made (a remote one too) is left alone.
        let mut user = Binding::new([7; 32], Version(3));
        let remote = Target::Remote {
            node: NODE,
            key: Key::new("mic/level").unwrap(),
        };
        user.bind("audio", remote).unwrap();
        user.bind("level", key("lab/level")).unwrap();
        let mut bytes = [0u8; BINDING_MAX];
        let n = user.encode(&mut bytes).unwrap();
        let record = auto_binding(&m, Some(&bytes[..n]), "lab").unwrap();
        let b = Binding::decode(&record).unwrap();
        assert_eq!((b.image_sha256, b.version), ([7; 32], Version(3)));
        assert_eq!(b.target("audio"), Some(remote));
        assert_eq!(b.target("level"), Some(key("lab/level")));
        assert_eq!(b.target("extra"), Some(key("room/lab/extra")));

        assert!(auto_binding(&m, None, "No Room").is_err());
        assert!(auto_binding(&m, Some(&[9, 9]), "lab").is_err());
        // No section: an empty binding.
        let none = auto_binding(b"\0asm\x01\0\0\0", None, "lab").unwrap();
        assert_eq!(Binding::decode(&none).unwrap().entries().count(), 0);
    }

    #[test]
    fn bm3_the_gate_refuses_system_and_internal_keys_both_ways() {
        for ok in [
            "room/lab/level",
            "lab/level",
            "unit/0011223344556677/mix/state",
            "system/x",
        ] {
            assert!(outbound(ok), "{ok}");
            let msg = publish_out(ok, b"hi").unwrap();
            assert_eq!(
                inbound(&msg),
                Inbound::Publish {
                    key: ok,
                    data: b"hi"
                }
            );
        }
        for refused in [
            "sys/info",
            "sys/payload/start",
            "sys/loader/begin",
            "sys",
            "ch/audio",
            "drv/spindle",
            "",
            "a//b",
            "not a key",
        ] {
            assert!(!outbound(refused), "{refused}");
            assert_eq!(publish_out(refused, b"x"), None);
        }
        // A peer that sends them anyway is refused on the way in.
        for key in ["sys/payload/start", "ch/audio", "drv/spindle"] {
            let msg = datagram(&Frame::Publish {
                key: Key::new(key).unwrap(),
                payload: b"x",
            })
            .unwrap();
            assert_eq!(inbound(&msg), Inbound::Refused(Refusal::Key), "{key}");
        }
    }

    #[test]
    fn bm3_oversize_and_malformed_messages_are_refused() {
        let k = Key::new("room/lab/level").unwrap();
        assert!(publish_out(k.as_str(), &[0; MAX_MESSAGE]).is_some());
        assert_eq!(publish_out(k.as_str(), &[0; MAX_MESSAGE + 1]), None);
        let big = datagram(&Frame::Publish {
            key: k,
            payload: &[0; MAX_MESSAGE + 1],
        })
        .unwrap();
        assert_eq!(inbound(&big), Inbound::Refused(Refusal::Oversize));
        let mut bad = publish_out(k.as_str(), b"abc").unwrap();
        bad[4] ^= 1;
        assert_eq!(inbound(&bad), Inbound::Refused(Refusal::Malformed));
        assert_eq!(inbound(&[]), Inbound::Refused(Refusal::Malformed));
        assert_eq!(
            inbound(&[0; MAX_DATAGRAM + 1]),
            Inbound::Refused(Refusal::Malformed)
        );
        assert_eq!(Refusal::Oversize.name(), "oversize");
    }

    #[test]
    fn bm4_only_the_three_hello_keys_are_asked_and_answered() {
        for (i, key) in HELLO_KEYS.iter().enumerate() {
            let req = hello_request(i as u8, key).unwrap();
            assert_eq!(inbound(&req), Inbound::Request { id: i as u8, key });
            let reply = hello_reply(i as u8, key, b"info", b"manifest", b"channels");
            let want: &[u8] = [&b"info"[..], b"manifest", b"channels"][i];
            assert_eq!(
                inbound(&reply),
                Inbound::Reply {
                    id: i as u8,
                    result: Ok(want)
                }
            );
        }
        for key in [
            "sys/payload/start",
            "sys/board",
            "sys/peers",
            "room/lab/level",
            "sys/payload/bind",
        ] {
            assert_eq!(hello_request(1, key), None, "{key}");
            let reply = hello_reply(9, key, b"i", b"m", b"c");
            assert_eq!(
                inbound(&reply),
                Inbound::Reply {
                    id: 9,
                    result: Err(HandlerError::NotFound)
                },
                "{key}"
            );
        }
        // The largest section and manifest still fit a reply.
        let reply = hello_reply(
            1,
            CHANNELS_KEY,
            b"",
            b"",
            &[1; middleware::wasm_abi::channels::MAX_SECTION],
        );
        assert!(
            matches!(inbound(&reply), Inbound::Reply { result: Ok(b), .. } if b.len() == middleware::wasm_abi::channels::MAX_SECTION)
        );
        let m = module(&decls());
        assert!(Channels::parse(&channels_section(&m)).is_ok_and(|c| c.len() == 3));
        assert!(channels_section(b"\0asm\x01\0\0\0").is_empty());
    }

    #[test]
    fn bm5_peer_ids_name_the_room_and_the_node() {
        let id = peer_id("lab", NODE).unwrap();
        assert_eq!(id, "mw-lab-0011223344556677");
        assert_eq!(parse_peer_id("lab", &id), Some(NODE));
        assert_eq!(hub_id("lab").as_deref(), Some("mw-lab-hub"));
        assert_eq!(peer_id("La b", NODE), None);
        for other in [
            "mw-lab-hub",
            "mw-lab2-0011223344556677",
            "mw-la-0011223344556677",
            "mw-lab-001122334455667",
            "mw-lab-00112233445566770",
            "mw-lab-0011223344556G77",
            "mw-lab-0011223344556677 ",
            "someone-else",
            "",
        ] {
            assert_eq!(parse_peer_id("lab", other), None, "{other}");
        }
        // Upper-case hex is another id on the server: not ours.
        assert_eq!(parse_peer_id("lab", "mw-lab-00112233445566AA"), None);
        let listed = [
            "mw-lab-00000000000000bb",
            "zz",
            "mw-lab-hub",
            id.as_str(),
            "mw-lab-00000000000000aa",
            "mw-other-00000000000000cc",
            "mw-lab-00000000000000bb",
        ];
        assert_eq!(
            room_peers("lab", &id, &listed),
            ["mw-lab-00000000000000aa", "mw-lab-00000000000000bb"]
        );
    }

    #[test]
    fn bm6_the_lower_id_dials_and_a_node_holds_sixteen_links() {
        let id = |n: u8| peer_id("lab", SenderId([0, 0, 0, 0, 0, 0, 0, n])).unwrap();
        assert!(dials(&id(1), &id(2)));
        assert!(!dials(&id(2), &id(1)));
        assert!(!dials(&id(2), &id(2)));
        let all: Vec<String> = (2..=18).map(id).collect();
        let listed: Vec<&str> = all.iter().map(String::as_str).collect();
        // Seventeen others: sixteen are dialled, the last only listed.
        let plan = dial_plan(&id(1), &listed, &[]);
        assert_eq!(plan.len(), MAX_PEERS);
        assert_eq!(plan, all[..16]);
        assert!(!plan.contains(&id(18)));
        // Links already held count, and are not dialled twice.
        let linked: Vec<&str> = listed[..15].to_vec();
        assert_eq!(dial_plan(&id(1), &listed, &linked), [id(17)]);
        assert_eq!(dial_plan(&id(1), &listed, &listed[..16]), [] as [String; 0]);
        // The higher id waits to be dialled.
        assert_eq!(dial_plan(&id(18), &listed, &[]), [] as [String; 0]);
        assert_eq!(dial_plan(&id(10), &listed, &[]).first(), Some(&id(11)));
        assert!(has_room(15) && !has_room(16));
    }

    #[test]
    fn bm7_server_urls() {
        let mut s = Server {
            host: "0.peerjs.com".into(),
            port: 443,
            path: "/".into(),
            key: "peerjs".into(),
            secure: true,
        };
        assert_eq!(
            s.socket_url("mw-lab-0011223344556677", "t0k").as_deref(),
            Some("wss://0.peerjs.com:443/peerjs?key=peerjs&id=mw-lab-0011223344556677&token=t0k")
        );
        assert_eq!(
            s.peers_url().as_deref(),
            Some("https://0.peerjs.com:443/peerjs/peers")
        );
        s = Server {
            host: "localhost".into(),
            port: 9000,
            path: "myapp".into(),
            key: "k1".into(),
            secure: false,
        };
        assert_eq!(
            s.socket_url("a", "b").as_deref(),
            Some("ws://localhost:9000/myapp/peerjs?key=k1&id=a&token=b")
        );
        assert_eq!(
            s.peers_url().as_deref(),
            Some("http://localhost:9000/myapp/k1/peers")
        );
        assert_eq!(s.socket_url("a&x=1", "b"), None);
        assert_eq!(s.socket_url("a", ""), None);
        s.key = "k?x".into();
        assert_eq!(s.peers_url(), None);
        s.key = "k".into();
        s.host = "evil.example/x?".into();
        assert_eq!(s.peers_url(), None);
        assert_eq!(
            parse_listing(r#"["mw-lab-00000000000000aa", 4, "x"]"#),
            Some(vec!["mw-lab-00000000000000aa".to_owned(), "x".to_owned()])
        );
        assert_eq!(parse_listing("{}"), None);
        assert_eq!(parse_listing("Unauthorized"), None);
    }

    /// What a stock server does to a relayed message: `src` is the
    /// sender's id.
    fn relayed(text: &str, src: &str) -> String {
        let json::Value::Obj(mut members) = json::parse(text).unwrap() else {
            panic!("not an object");
        };
        members.push(("src".to_owned(), json::Value::Str(src.to_owned())));
        json::write(&json::Value::Obj(members))
    }

    #[test]
    fn bm8_peerjs_messages_have_the_servers_shapes() {
        let sdp = "v=0\r\no=- 46 2 IN IP4 127.0.0.1\r\na=ice-ufrag:\"q\\x\"\r\n";
        let offer = Signal::Offer {
            peer: "mw-lab-00000000000000bb".into(),
            connection: "dc_1".into(),
            label: "mw".into(),
            sdp: sdp.into(),
        };
        let text = offer.encode();
        let v = json::parse(&text).unwrap();
        assert_eq!(v.get("type").and_then(json::Value::str), Some("OFFER"));
        assert_eq!(
            v.get("dst").and_then(json::Value::str),
            Some("mw-lab-00000000000000bb")
        );
        let p = v.get("payload").unwrap();
        assert_eq!(p.get("type").and_then(json::Value::str), Some("data"));
        assert_eq!(
            p.get("connectionId").and_then(json::Value::str),
            Some("dc_1")
        );
        assert_eq!(
            p.get("serialization").and_then(json::Value::str),
            Some("raw")
        );
        assert_eq!(p.get("reliable"), Some(&json::Value::Bool(true)));
        assert_eq!(
            p.get("sdp")
                .and_then(|d| d.get("sdp"))
                .and_then(json::Value::str),
            Some(sdp)
        );
        // Relayed, it reads back with the sender as the peer.
        let Signal::Offer { peer, sdp: got, .. } =
            Signal::decode(&relayed(&text, "mw-lab-00000000000000aa")).unwrap()
        else {
            panic!("not an offer");
        };
        assert_eq!(
            (peer.as_str(), got.as_str()),
            ("mw-lab-00000000000000aa", sdp)
        );

        for s in [
            Signal::Answer {
                peer: "a".into(),
                connection: "dc_1".into(),
                sdp: sdp.into(),
            },
            Signal::Candidate {
                peer: "a".into(),
                connection: "dc_1".into(),
                candidate: "candidate:1 1 udp 2113937151 192.0.2.1 5000 typ host".into(),
                mid: Some("0".into()),
                mline: Some(0),
            },
            Signal::Candidate {
                peer: "a".into(),
                connection: "dc_1".into(),
                candidate: String::new(),
                mid: None,
                mline: None,
            },
        ] {
            assert_eq!(Signal::decode(&relayed(&s.encode(), "a")), Some(s));
        }
        assert_eq!(Signal::Heartbeat.encode(), r#"{"type":"HEARTBEAT"}"#);
        assert_eq!(Signal::decode(r#"{"type":"OPEN"}"#), Some(Signal::Open));
        assert_eq!(
            Signal::decode(r#"{"type":"ID-TAKEN","payload":{"msg":"ID is taken"}}"#),
            Some(Signal::IdTaken)
        );
        assert_eq!(
            Signal::decode(r#"{"type":"ERROR","payload":{"msg":"Invalid key provided"}}"#),
            Some(Signal::Error("Invalid key provided".into()))
        );
        assert_eq!(
            Signal::decode(r#"{"type":"LEAVE","src":"a","dst":"b"}"#),
            Some(Signal::Leave { peer: "a".into() })
        );
        assert_eq!(
            Signal::decode(r#"{ "type" : "EXPIRE", "src" : "a" }"#),
            Some(Signal::Expire { peer: "a".into() })
        );
        // A media call, a missing field, and things that are not messages.
        for bad in [
            r#"{"type":"OFFER","src":"a","payload":{"type":"media","connectionId":"c","sdp":{"type":"offer","sdp":"x"}}}"#,
            r#"{"type":"OFFER","src":"a","payload":{"type":"data","sdp":{"type":"offer","sdp":"x"}}}"#,
            r#"{"type":"OFFER","payload":{"type":"data","connectionId":"c","sdp":{"type":"offer","sdp":"x"}}}"#,
            r#"{"type":"ANSWER","src":"a","payload":{"type":"data","connectionId":"c","sdp":{"type":"offer","sdp":"x"}}}"#,
            r#"{"type":"NOPE"}"#,
            r#"{"type":7}"#,
            "[]",
            "",
            "{",
        ] {
            assert_eq!(Signal::decode(bad), None, "{bad}");
        }
    }

    #[test]
    fn bm8_json_reads_and_writes_what_the_messages_need() {
        use json::Value::*;
        let v = json::parse(
            r#" {"a":[1,-2.5e1,true,false,null],"s":"q\"\\\/\n\u00e9\ud83d\ude00","o":{}} "#,
        )
        .unwrap();
        assert_eq!(
            v.get("a"),
            Some(&Arr(vec![
                Num(1.0),
                Num(-25.0),
                Bool(true),
                Bool(false),
                Null
            ]))
        );
        assert_eq!(v.get("s").and_then(json::Value::str), Some("q\"\\/\né😀"));
        assert_eq!(json::parse(&json::write(&v)), Some(v));
        assert_eq!(json::write(&Str("\u{1}\t".into())), r#""\u0001\t""#);
        for bad in [
            "{\"a\":1,}",
            "[1 2]",
            "\"\\x\"",
            "\"\\ud83d\"",
            "\"a\nb\"",
            "nul",
            "1 2",
            "{\"a\"}",
            "--1",
        ] {
            assert_eq!(json::parse(bad), None, "{bad}");
        }
        let deep = "[".repeat(json::MAX_DEPTH + 1) + &"]".repeat(json::MAX_DEPTH + 1);
        assert_eq!(json::parse(&deep), None);
    }

    #[test]
    fn bm9_hub_messages() {
        let join = HubMsg::Join("mw-lab-00000000000000aa".into());
        assert_eq!(join.encode(), "mw-hub join mw-lab-00000000000000aa");
        assert_eq!(HubMsg::decode(&join.encode()), Some(join));
        let peers = HubMsg::Peers(vec![
            "mw-lab-00000000000000aa".into(),
            "mw-lab-00000000000000bb".into(),
        ]);
        assert_eq!(HubMsg::decode(&peers.encode()), Some(peers));
        assert_eq!(
            HubMsg::decode("mw-hub peers"),
            Some(HubMsg::Peers(Vec::new()))
        );
        let many: Vec<String> = (0..MAX_MEMBERS + 1).map(|i| format!("p{i}")).collect();
        let Some(HubMsg::Peers(kept)) = HubMsg::decode(&HubMsg::Peers(many.clone()).encode())
        else {
            panic!("not a list");
        };
        assert_eq!(kept.len(), MAX_MEMBERS);
        for bad in [
            "",
            "hello",
            "mw-hub",
            "mw-hub join",
            "mw-hub join a b",
            "mw-hub join a/b",
            "mw-hub peers a  b",
            "mw-hub leave a",
            &format!("mw-hub peers {}", many.join(" ")),
        ] {
            assert_eq!(HubMsg::decode(bad), None, "{bad}");
        }
    }

    #[test]
    fn bm10_a_peers_channels_are_compared_by_name() {
        use middleware::stream::samples::SensorKind;
        let local_m = module(&[
            Channel::bytes("level", Direction::Out, false),
            Channel::bytes("audio", Direction::In, true),
            Channel::samples(
                "mic",
                Direction::In,
                Some(SensorKind::Microphone),
                8000,
                16000,
                false,
            ),
            Channel::bytes("cmd", Direction::Out, false),
            Channel::bytes("only_here", Direction::Out, false),
        ]);
        let peer_m = module(&[
            Channel::bytes("level", Direction::In, false),
            Channel::bytes("audio", Direction::In, false),
            Channel::samples(
                "mic",
                Direction::Out,
                Some(SensorKind::Microphone),
                44100,
                48000,
                false,
            ),
            Channel::frames("cmd", Direction::In, false),
            Channel::bytes("only_there", Direction::In, false),
        ]);
        let local = Channels::from_module(&local_m).unwrap();
        let peer = Channels::from_module(&peer_m).unwrap();
        let got: Vec<(String, PairState)> = compare(&local, &peer)
            .into_iter()
            .map(|p| (p.name, p.state))
            .collect();
        assert_eq!(
            got,
            [
                ("level".to_owned(), PairState::Connected),
                ("audio".to_owned(), PairState::SameDirection),
                ("mic".to_owned(), PairState::Mismatch),
                ("cmd".to_owned(), PairState::Mismatch),
            ]
        );
        let all = compare(&local, &peer);
        assert_eq!(all[0].message, "level goes to this peer");
        assert!(
            all[2].message.contains("do not overlap"),
            "{}",
            all[2].message
        );
        assert!(all[3].message.contains("sends bytes"), "{}", all[3].message);
        assert_eq!(
            compare(&peer, &local)[0].message,
            "level comes from this peer"
        );
        assert!(compare(&local, &Channels::none()).is_empty());
    }
}
