//! Other nodes as seen through a board (PR1–PR3): the peers it has heard
//! and the read-only requests it forwards to them.
//!
//! A browser on a board's serial port reaches the rest of the network
//! through that board, not through a transport of its own. Two pieces make
//! that possible:
//!
//! * **Forwarding** ([`forwardable`]): a request on a serial link on
//!   `sys/peer/<id16>/info`, `.../manifest` or `.../board` (the node id as
//!   16 lowercase hex digits, [`peer_key`]) is sent by the board as a query
//!   for `sys/info`, `sys/manifest` or `sys/board` to that node (over Zenoh
//!   on the RT1050, at its per-node key `@mw/node/<id16>/sys/...`) and the
//!   reply relayed back. Only those three read-only keys are forwarded;
//!   nothing that changes a node, and nothing that arrived from the
//!   network itself.
//! * **`sys/peers`** ([`PeerSet`]): the node ids the board has heard,
//!   from the senders of the samples it received and the nodes that
//!   answered forwarded requests, newest last, at most [`MAX_PEERS`]
//!   (the oldest is forgotten). The reply is `[n u8]` then `n` ids of 8
//!   bytes ([`PeerSet::encode`], [`decode`]).

use super::keys;
use crate::key::Key;
use crate::transport::SenderId;

/// Most peers a board remembers.
pub const MAX_PEERS: usize = 16;

/// Longest `sys/peers` reply.
pub const PEERS_MAX: usize = 1 + 8 * MAX_PEERS;

/// The per-node system keys a board forwards (read only).
pub const FORWARDED: [Key<'static>; 3] = [keys::INFO, keys::MANIFEST, keys::BOARD];

/// Prefix of the forwarded keys.
pub const PEER_PREFIX: &str = "sys/peer/";

/// Longest [`peer_key`].
pub const PEER_KEY_MAX: usize = PEER_PREFIX.len() + 16 + 1 + 8;

/// The key a host sends to reach `node`'s `sys/<what>` (`info`,
/// `manifest`, `board`) through a board: `sys/peer/<id16>/<what>`.
pub fn peer_key(node: SenderId, what: &str) -> Option<heapless::String<PEER_KEY_MAX>> {
    let mut s = heapless::String::new();
    s.push_str(PEER_PREFIX).ok()?;
    s.push_str(core::str::from_utf8(&crate::zenoh_wire::zid16_hex(node)).ok()?).ok()?;
    s.push('/').ok()?;
    s.push_str(what).ok()?;
    Some(s)
}

/// The node and system key of a request to forward: `key` is
/// `sys/peer/<id16>/<what>` for one of [`FORWARDED`] and another node than
/// `own` (the board answers its own keys itself); `None` for anything
/// else.
pub fn forwardable(key: Key<'_>, own: SenderId) -> Option<(SenderId, Key<'static>)> {
    let rest = key.as_str().strip_prefix(PEER_PREFIX)?;
    let (hex, what) = rest.split_at_checked(16)?;
    let what = what.strip_prefix('/')?;
    let mut id = [0u8; 8];
    for (byte, pair) in id.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        let digit = |c: u8| match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        };
        *byte = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    let path = FORWARDED.into_iter().find(|k| k.as_str().strip_prefix("sys/") == Some(what))?;
    let node = SenderId(id);
    (node != own).then_some((node, path))
}

/// The node ids heard, oldest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerSet {
    ids: [SenderId; MAX_PEERS],
    n: usize,
}

impl Default for PeerSet {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerSet {
    /// No peers.
    pub const fn new() -> Self {
        PeerSet {
            ids: [SenderId([0; 8]); MAX_PEERS],
            n: 0,
        }
    }

    /// Records `id` as heard now: it moves to the newest place; when full
    /// the oldest is forgotten. The all-zero id (no sender) is ignored.
    pub fn record(&mut self, id: SenderId) {
        if id == SenderId([0; 8]) {
            return;
        }
        let at = self.ids[..self.n].iter().position(|p| *p == id);
        match at {
            Some(i) => self.ids[i..self.n].rotate_left(1),
            None if self.n < MAX_PEERS => {
                self.ids[self.n] = id;
                self.n += 1;
            }
            None => {
                self.ids.rotate_left(1);
                self.ids[MAX_PEERS - 1] = id;
            }
        }
    }

    /// The peers, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = SenderId> + '_ {
        self.ids[..self.n].iter().copied()
    }

    /// The `sys/peers` reply into `out`; its length, `None` if too short.
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        let len = 1 + 8 * self.n;
        let dst = out.get_mut(..len)?;
        dst[0] = self.n as u8;
        for (chunk, id) in dst[1..].chunks_exact_mut(8).zip(self.iter()) {
            chunk.copy_from_slice(&id.0);
        }
        Some(len)
    }
}

/// The node ids of a `sys/peers` reply; `None` when malformed.
pub fn decode(bytes: &[u8]) -> Option<impl Iterator<Item = SenderId> + '_> {
    let (&n, rest) = bytes.split_first()?;
    if usize::from(n) > MAX_PEERS || rest.len() != 8 * usize::from(n) {
        return None;
    }
    Some(rest.chunks_exact(8).map(|c| {
        let mut id = [0u8; 8];
        id.copy_from_slice(c);
        SenderId(id)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN: SenderId = SenderId([1, 2, 3, 4, 5, 6, 7, 8]);
    const PEER: SenderId = SenderId([0xab; 8]);

    #[test]
    fn pr1_only_read_only_keys_of_other_nodes_are_forwarded() {
        let k = |s: &'static str| Key::new(s).unwrap();
        for what in ["info", "manifest", "board"] {
            let s = peer_key(PEER, what).unwrap();
            assert_eq!(s.as_str(), format!("sys/peer/abababababababab/{what}"));
            let got = forwardable(Key::new(&s).unwrap(), OWN).map(|(n, p)| (n, p.as_str()));
            assert_eq!(got, Some((PEER, format!("sys/{what}").as_str())));
        }
        for refused in [
            "sys/peer/abababababababab/payload/stop",
            "sys/peer/abababababababab/loader/begin",
            "sys/peer/abababababababab/trust/claim",
            "sys/peer/0102030405060708/info",
            "sys/info",
            "sys/peer/ABABABABABABABAB/info",
            "sys/peer/abab/info",
        ] {
            assert_eq!(forwardable(k(refused), OWN), None, "{refused}");
        }
    }

    #[test]
    fn pr2_the_peer_set_keeps_the_newest_and_round_trips() {
        let mut p = PeerSet::new();
        p.record(SenderId([0; 8]));
        assert_eq!(p.iter().count(), 0);
        for i in 0..MAX_PEERS as u8 + 3 {
            p.record(SenderId([i; 8]));
        }
        // The three oldest are gone; a repeat moves to the newest place.
        assert_eq!(p.iter().next(), Some(SenderId([3; 8])));
        p.record(SenderId([5; 8]));
        assert_eq!(p.iter().last(), Some(SenderId([5; 8])));
        assert_eq!(p.iter().count(), MAX_PEERS);
        let mut out = [0u8; PEERS_MAX];
        let n = p.encode(&mut out).unwrap();
        assert_eq!(n, PEERS_MAX);
        assert!(decode(&out[..n]).unwrap().eq(p.iter()));
        assert!(decode(&out[..n - 1]).is_none());
        assert!(decode(&[17]).is_none());
        assert_eq!(decode(&[0]).map(|i| i.count()), Some(0));
        assert_eq!(p.encode(&mut [0u8; 4]), None);
    }
}
