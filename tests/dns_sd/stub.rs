//! A stub DNS server on 127.0.0.1:0 serving SRV, A and AAAA fixtures for
//! headless Kubernetes Services, for the `dns_sd` tests. It never forwards
//! anything: names it has no fixture for are NXDOMAIN.
//!
//! The server task owns its fixture table; tests change it by message
//! ([`Stub`] methods await the server's acknowledgement), so a change is in
//! effect for every query after the call returns.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use middleware::dns::wire::{
    FLAG_AA, FLAG_QR, FLAG_RA, FLAG_RD, FLAG_TC, Message, MessageWriter, Rdata, RecordData,
    RecordOut, Section, TYPE_A, TYPE_AAAA, TYPE_OPT, TYPE_SRV, TextName, Txt,
};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

/// One pod of a headless Service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pod {
    /// The SRV target, `<pod>.<service>.<namespace>.svc.cluster.local`.
    pub target: String,
    pub port: u16,
    pub v4: Option<Ipv4Addr>,
    pub v6: Option<Ipv6Addr>,
    /// TXT RDATA served for the target (empty: no TXT record).
    pub txt: Vec<u8>,
    pub ttl: u32,
}

impl Pod {
    /// A pod `<name>.<suffix>` at `v4:port` with TTL `ttl`.
    pub fn v4(name: &str, suffix: &str, v4: [u8; 4], port: u16, ttl: u32) -> Self {
        Pod {
            target: format!("{name}.{suffix}"),
            port,
            v4: Some(v4.into()),
            v6: None,
            txt: Vec::new(),
            ttl,
        }
    }
}

/// How the server answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Correct replies.
    Normal,
    /// A reply with the wrong id, then the correct one.
    WrongIdFirst,
    /// A reply with a bogus pod from another port, then the correct one.
    SpoofFirst,
    /// Only replies with the wrong id.
    OnlyWrongId,
    /// `n` replies with the wrong id from the server's address and `n`
    /// replies with the right id and a bogus pod from another port, then
    /// the correct reply.
    Flood(usize),
    /// Replies with TC set and no records.
    Truncate,
    /// SERVFAIL.
    ServFail,
}

/// A query the server received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub name: String,
    pub qtype: u16,
    pub id: u16,
    pub flags: u16,
    /// The UDP payload size of an EDNS(0) OPT record, if present.
    pub edns: Option<u16>,
    /// The source port the query came from.
    pub port: u16,
}

enum Command {
    Upsert(String, Pod, oneshot::Sender<()>),
    Remove(String, String, oneshot::Sender<()>),
    Zone(String, Vec<Pod>, oneshot::Sender<()>),
    Glue(bool, oneshot::Sender<()>),
    Mode(Mode, oneshot::Sender<()>),
    Seen(oneshot::Sender<Vec<Seen>>),
}

fn rec<'a>(name: &'a str, ttl: u32, data: RecordData<'a>) -> RecordOut<'a> {
    RecordOut {
        name: TextName::new(name).expect("name"),
        ttl,
        cache_flush: false,
        data,
    }
}

/// The A and/or AAAA records of `p`.
fn addresses(p: &Pod, v4: bool, v6: bool) -> Vec<RecordOut<'_>> {
    let mut out = Vec::new();
    if let (true, Some(a)) = (v4, p.v4) {
        out.push(rec(&p.target, p.ttl, RecordData::A(a)));
    }
    if let (true, Some(a)) = (v6, p.v6) {
        out.push(rec(&p.target, p.ttl, RecordData::Aaaa(a)));
    }
    out
}

#[derive(Debug, Clone)]
struct State {
    zones: Vec<(String, Vec<Pod>)>,
    glue: bool,
    mode: Mode,
    seen: Vec<Seen>,
}

impl State {
    fn zone_mut(&mut self, srv: &str) -> &mut Vec<Pod> {
        let srv = srv.to_ascii_lowercase();
        let i = match self.zones.iter().position(|(n, _)| *n == srv) {
            Some(i) => i,
            None => {
                self.zones.push((srv, Vec::new()));
                self.zones.len() - 1
            }
        };
        &mut self.zones[i].1
    }

    fn apply(&mut self, cmd: Command) {
        match cmd {
            Command::Upsert(srv, pod, ack) => {
                let zone = self.zone_mut(&srv);
                zone.retain(|p| p.target != pod.target);
                zone.push(pod);
                let _ = ack.send(());
            }
            Command::Remove(srv, target, ack) => {
                self.zone_mut(&srv).retain(|p| p.target != target);
                let _ = ack.send(());
            }
            Command::Zone(srv, pods, ack) => {
                *self.zone_mut(&srv) = pods;
                let _ = ack.send(());
            }
            Command::Glue(glue, ack) => {
                self.glue = glue;
                let _ = ack.send(());
            }
            Command::Mode(mode, ack) => {
                self.mode = mode;
                let _ = ack.send(());
            }
            Command::Seen(reply) => {
                let _ = reply.send(self.seen.clone());
            }
        }
    }

    /// This state with the pods of `srv` replaced by one bogus pod.
    fn evil(&self, srv: &str) -> State {
        let mut evil = State {
            mode: Mode::Normal,
            ..self.clone()
        };
        let suffix = srv.split_once("._tcp.").map_or("x", |(_, s)| s);
        if let Some((_, pods)) = evil
            .zones
            .iter_mut()
            .find(|(n, _)| n.eq_ignore_ascii_case(srv))
        {
            *pods = vec![Pod::v4("evil", suffix, [6, 6, 6, 6], 666, 60)];
        }
        evil
    }

    fn pod(&self, target: &str) -> Option<&Pod> {
        self.zones
            .iter()
            .flat_map(|(_, pods)| pods)
            .find(|p| p.target.eq_ignore_ascii_case(target))
    }

    /// The reply to (id, qname, qtype) in this state.
    fn reply(&self, out: &mut [u8], id: u16, qname: &str, qtype: u16) -> usize {
        let name = TextName::new(qname).expect("query name");
        let mut flags = FLAG_QR | FLAG_AA | FLAG_RD | FLAG_RA;
        let mut answers: Vec<RecordOut<'_>> = Vec::new();
        let mut additional: Vec<RecordOut<'_>> = Vec::new();
        let zone = self
            .zones
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(qname))
            .map(|(_, pods)| pods);
        match (self.mode, qtype) {
            (Mode::Truncate, _) => flags |= FLAG_TC,
            (Mode::ServFail, _) => flags |= 2,
            (_, TYPE_SRV) => match zone {
                Some(pods) if !pods.is_empty() => {
                    for p in pods {
                        answers.push(rec(
                            qname,
                            p.ttl,
                            RecordData::Srv {
                                priority: 0,
                                weight: 100,
                                port: p.port,
                                target: TextName::new(&p.target).expect("target"),
                            },
                        ));
                        if self.glue {
                            additional.extend(addresses(p, true, true));
                            if !p.txt.is_empty() {
                                additional.push(rec(
                                    &p.target,
                                    p.ttl,
                                    RecordData::Txt(Txt::new(&p.txt).expect("txt")),
                                ));
                            }
                        }
                    }
                }
                _ => flags |= 3,
            },
            (_, TYPE_A | TYPE_AAAA) => match self.pod(qname) {
                Some(p) => answers.extend(addresses(p, qtype == TYPE_A, qtype == TYPE_AAAA)),
                None => flags |= 3,
            },
            _ => flags |= 4,
        }
        let mut w = MessageWriter::new(out, id, flags)
            .expect("header")
            .question(name, qtype, false)
            .expect("question");
        for r in &answers {
            w = w.record(Section::Answer, r).expect("answer");
        }
        for r in &additional {
            w = w.record(Section::Additional, r).expect("additional");
        }
        w.finish()
    }
}

/// Handle to a running stub server.
#[derive(Debug, Clone)]
pub struct Stub {
    pub addr: SocketAddr,
    tx: mpsc::Sender<Command>,
}

impl Stub {
    /// Starts a server on 127.0.0.1:0 with no zones, glue on, normal mode.
    /// It stops when every handle is dropped.
    pub async fn start() -> Stub {
        let socket = UdpSocket::bind("127.0.0.1:0").await.expect("bind stub");
        let spoof = UdpSocket::bind("127.0.0.1:0").await.expect("bind spoof");
        let addr = socket.local_addr().expect("addr");
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(serve(socket, spoof, rx));
        Stub { addr, tx }
    }

    async fn ack(&self, cmd: impl FnOnce(oneshot::Sender<()>) -> Command) {
        let (ack, done) = oneshot::channel();
        self.tx.send(cmd(ack)).await.expect("stub running");
        done.await.expect("stub ack");
    }

    /// Replaces the pods of SRV name `srv`.
    pub async fn set_pods(&self, srv: &str, pods: Vec<Pod>) {
        self.ack(|a| Command::Zone(srv.into(), pods, a)).await;
    }

    /// Adds or replaces one pod (by target) of `srv`.
    pub async fn upsert(&self, srv: &str, pod: Pod) {
        self.ack(|a| Command::Upsert(srv.into(), pod, a)).await;
    }

    /// Removes the pod with `target` from `srv`.
    pub async fn remove(&self, srv: &str, target: &str) {
        self.ack(|a| Command::Remove(srv.into(), target.into(), a))
            .await;
    }

    /// Whether SRV replies carry address and TXT glue.
    pub async fn set_glue(&self, glue: bool) {
        self.ack(|a| Command::Glue(glue, a)).await;
    }

    pub async fn set_mode(&self, mode: Mode) {
        self.ack(|a| Command::Mode(mode, a)).await;
    }

    /// Every query received so far, in order.
    pub async fn seen(&self) -> Vec<Seen> {
        let (reply, done) = oneshot::channel();
        self.tx
            .send(Command::Seen(reply))
            .await
            .expect("stub running");
        done.await.expect("stub reply")
    }
}

async fn serve(socket: UdpSocket, spoof: UdpSocket, mut rx: mpsc::Receiver<Command>) {
    let mut state = State {
        zones: Vec::new(),
        glue: true,
        mode: Mode::Normal,
        seen: Vec::new(),
    };
    let mut buf = [0u8; 2048];
    let mut out = [0u8; 4096];
    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(cmd) => state.apply(cmd),
                None => return,
            },
            received = socket.recv_from(&mut buf) => {
                let Ok((len, from)) = received else { return };
                let Ok(query) = Message::parse(&buf[..len]) else { continue };
                let Some(q) = query.questions().next() else { continue };
                let mut text = [0u8; 512];
                let Ok(qname) = q.name.write_text(&mut text) else { continue };
                let qname = qname.to_owned();
                let edns = query.records().find_map(|r| match r.data {
                    Rdata::Other { rtype: TYPE_OPT, .. } => Some(r.class),
                    _ => None,
                });
                state.seen.push(Seen {
                    name: qname.clone(),
                    qtype: q.qtype,
                    id: query.id(),
                    flags: query.flags(),
                    edns,
                    port: from.port(),
                });
                let id = query.id();
                match state.mode {
                    Mode::WrongIdFirst | Mode::OnlyWrongId => {
                        let n = state.reply(&mut out, id.wrapping_add(1), &qname, q.qtype);
                        let _ = socket.send_to(&out[..n], from).await;
                    }
                    Mode::SpoofFirst => {
                        let n = state.evil(&qname).reply(&mut out, id, &qname, q.qtype);
                        let _ = spoof.send_to(&out[..n], from).await;
                    }
                    Mode::Flood(count) => {
                        let n = state.reply(&mut out, id.wrapping_add(1), &qname, q.qtype);
                        for _ in 0..count {
                            let _ = socket.send_to(&out[..n], from).await;
                        }
                        let n = state.evil(&qname).reply(&mut out, id, &qname, q.qtype);
                        for _ in 0..count {
                            let _ = spoof.send_to(&out[..n], from).await;
                        }
                    }
                    _ => {}
                }
                if state.mode != Mode::OnlyWrongId {
                    let n = state.reply(&mut out, id, &qname, q.qtype);
                    let _ = socket.send_to(&out[..n], from).await;
                }
            }
        }
    }
}
