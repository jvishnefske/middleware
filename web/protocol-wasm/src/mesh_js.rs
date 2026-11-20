//! `MeshSession`: the JS face of [`crate::mesh::Pump`]. Every method is
//! synchronous, so JS can never re-enter the session while it runs; the
//! page owns the WebSocket and the timer (`web/src/device/mesh.ts`).

use js_sys::{Array, Object, Uint8Array};
use middleware::zenoh_lite::Zid;
use wasm_bindgen::prelude::*;

use crate::bindings::{bytes, node_hex, parse_node, set};
use crate::mesh::{Cmd, Done, Failure, Outcome, Pump};

/// One Zenoh session over one WebSocket (see `crate::mesh`).
#[wasm_bindgen]
pub struct MeshSession(Pump);

fn completion(d: Done) -> JsValue {
    let o = Object::new();
    set(&o, "id", d.id);
    match d.outcome {
        Outcome::Opened(sender) => {
            set(&o, "kind", "opened");
            set(&o, "sender", node_hex(sender));
        }
        Outcome::Closed => set(&o, "kind", "closed"),
        Outcome::Reply(body) => {
            set(&o, "kind", "reply");
            set(&o, "body", bytes(&body));
        }
        Outcome::Replies(list) => {
            set(&o, "kind", "replies");
            let replies: Array = list
                .into_iter()
                .map(|(sender, result)| {
                    let r = Object::new();
                    if let Some(s) = sender {
                        set(&r, "sender", node_hex(s));
                    }
                    match result {
                        Ok(body) => set(&r, "body", bytes(&body)),
                        Err(code) => set(&r, "status", code),
                    }
                    JsValue::from(r)
                })
                .collect();
            set(&o, "replies", replies);
        }
        Outcome::Failed(f) => {
            set(&o, "kind", "error");
            let (failure, code, message) = match f {
                Failure::Handler(c) => ("handler", Some(c), None),
                Failure::NoReply => ("noReply", None, None),
                Failure::Down => ("down", None, None),
                Failure::TooLarge => ("tooLarge", None, None),
                Failure::Other(m) => ("other", None, Some(m)),
            };
            set(&o, "failure", failure);
            if let Some(c) = code {
                set(&o, "status", c);
            }
            if let Some(m) = message {
                set(&o, "message", m);
            }
        }
    }
    JsValue::from(o)
}

#[wasm_bindgen]
impl MeshSession {
    /// A session with Zenoh id `zid` (1 to 16 bytes, not all zero; the page
    /// draws 16 random ones), not yet opened. Throws for an invalid id.
    #[wasm_bindgen(constructor)]
    pub fn new(zid: &[u8]) -> Result<MeshSession, JsError> {
        let zid = Zid::from_slice(zid)
            .ok_or_else(|| JsError::new("a Zenoh id is 1 to 16 bytes, not all zero"))?;
        Ok(MeshSession(Pump::new(zid)))
    }

    /// One WebSocket message from the router.
    pub fn feed(&mut self, msg: &[u8]) {
        self.0.feed(msg);
    }

    /// The WebSocket closed.
    pub fn closed(&mut self) {
        self.0.close();
    }

    /// The messages to send on the WebSocket, in order.
    #[wasm_bindgen(js_name = takeOutput)]
    pub fn take_output(&mut self) -> Array {
        self.0
            .take_output()
            .iter()
            .map(|m| JsValue::from(Uint8Array::from(&m[..])))
            .collect()
    }

    /// Queues the handshake; completes `{id, kind: "opened", sender}`.
    pub fn open(&mut self) -> u32 {
        self.0.submit(Cmd::Open)
    }

    /// Queues a Close; completes `{id, kind: "closed"}`.
    pub fn close(&mut self) -> u32 {
        self.0.submit(Cmd::Close)
    }

    /// Queues a request for `key`: to the node with sender id `sender` (16
    /// hex digits; only its reply is accepted), or to whoever serves `key`
    /// when undefined. Completes `{id, kind: "reply", body}` or an error.
    /// Throws for a malformed sender id.
    pub fn request(
        &mut self,
        sender: Option<String>,
        key: String,
        body: Vec<u8>,
        #[wasm_bindgen(js_name = timeoutMs)] timeout_ms: u32,
    ) -> Result<u32, JsError> {
        let node = match sender {
            None => None,
            Some(s) => {
                Some(parse_node(&s).ok_or_else(|| JsError::new("a sender id is 16 hex digits"))?)
            }
        };
        Ok(self.0.submit(Cmd::Request {
            node,
            key,
            body,
            timeout_ms,
        }))
    }

    /// Queues a discovery of `key` on every node (the wildcard node key).
    /// Completes `{id, kind: "replies", replies: [{sender?, body?,
    /// status?}]}` or an error.
    pub fn discover(
        &mut self,
        key: String,
        body: Vec<u8>,
        #[wasm_bindgen(js_name = timeoutMs)] timeout_ms: u32,
        max: u32,
    ) -> u32 {
        self.0.submit(Cmd::Discover {
            key,
            body,
            timeout_ms,
            max: max as usize,
        })
    }

    /// Runs the session at `nowMs` (the page's `performance.now()`) and
    /// returns what finished: `[{id, kind: "opened"|"closed"|"reply"|
    /// "replies"|"error", ...}]`; an error has `failure: "handler"|
    /// "noReply"|"down"|"tooLarge"|"other"`, `status` (the `HandlerError`
    /// code) for "handler" and `message` for "other".
    pub fn poll(&mut self, #[wasm_bindgen(js_name = nowMs)] now_ms: f64) -> Array {
        let now = if now_ms.is_finite() && now_ms > 0.0 {
            now_ms as u64
        } else {
            0
        };
        self.0.poll(now).into_iter().map(completion).collect()
    }

    /// When to poll next if nothing arrives, in the same time as `poll`'s.
    #[wasm_bindgen(js_name = nextWakeMs)]
    pub fn next_wake_ms(&self) -> Option<f64> {
        self.0.next_wake_ms().map(|w| w as f64)
    }

    /// Our sender id, 16 hex digits.
    #[wasm_bindgen(getter, js_name = senderId)]
    pub fn sender_id(&self) -> String {
        node_hex(self.0.sender_id())
    }
}
