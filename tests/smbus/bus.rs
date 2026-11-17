//! A simulated SMBus multi-master bus for `tests/smbus.rs` (host only).
//!
//! Several nodes share one [`Bus`]. Each attaches with [`attach`] and gets
//! a [`SimMaster`] (an `embedded-hal-async` I2C master) and a
//! [`SimTarget`] (the [`I2cTarget`] adapter): the target answers the
//! node's own address and, if given, a broadcast address, and buffers
//! completed write transactions in arrival order like an interrupt-driven
//! adapter would (the contract's rule 4), unbounded unless a capacity is
//! set.
//!
//! A write yields once to the executor before it completes, so masters
//! running concurrently in one task (`tokio::join!`) interleave their
//! blocks at a shared target. The bus can also:
//!
//! * inject faults per master ([`inject`]), one per write attempt: lost
//!   arbitration, an address or data NACK, a bus error, a corrupted byte
//!   (the PEC then fails), a block delivered twice, or none;
//! * model contention ([`Bus::contention`]): a write that starts while
//!   another master's write is in flight loses arbitration;
//! * log every write attempt ([`Bus::log`]).
//!
//! A write to an address no attached target answers is a NACK of the
//! address, as is a write to a detached target; a unicast write to a full
//! target is a NACK of the data, and a full broadcast listener drops the
//! transaction (the newest), as the adapter contract allows.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::future::poll_fn;
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

use embedded_hal::i2c::{ErrorKind, ErrorType, NoAcknowledgeSource, Operation};
use embedded_hal_async::i2c::I2c;
use middleware::smbus::{Clock, I2cTarget, Transaction};

/// A fault injected into one write attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// No fault: the write goes through.
    Pass,
    /// The write loses arbitration.
    ArbitrationLoss,
    /// Nobody acknowledges the address.
    NackAddress,
    /// The data is not acknowledged (nothing is delivered).
    NackData,
    /// A bus error.
    Bus,
    /// The write is delivered with byte `n` (after the address byte)
    /// inverted.
    Corrupt(usize),
    /// The write is delivered twice.
    Duplicate,
}

/// What became of one write attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Delivered to this many targets.
    Delivered(usize),
    /// Failed with this error.
    Failed(ErrorKind),
}

/// One logged write attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The writing master's own address.
    pub from: u8,
    /// The address written to.
    pub to: u8,
    /// Bytes after the address byte.
    pub bytes: Vec<u8>,
    /// What happened.
    pub outcome: Outcome,
}

struct Endpoint {
    broadcast: Option<u8>,
    queue: VecDeque<(u8, Vec<u8>)>,
    capacity: Option<usize>,
    attached: bool,
    dropped: usize,
    waker: Option<Waker>,
}

/// The shared bus model.
#[derive(Default)]
pub struct Bus {
    endpoints: BTreeMap<u8, Endpoint>,
    faults: BTreeMap<u8, VecDeque<Fault>>,
    in_flight: Option<u8>,
    /// Model contention: a write that starts while another is in flight
    /// loses arbitration.
    pub contention: bool,
    /// Every write attempt, in order.
    pub log: Vec<Record>,
}

/// A bus shared by the nodes of one test.
pub type Shared = Rc<RefCell<Bus>>;

/// A new, empty bus.
pub fn bus() -> Shared {
    Rc::new(RefCell::new(Bus::default()))
}

/// Attaches a node at `address`, also answering `broadcast` if given.
pub fn attach(bus: &Shared, address: u8, broadcast: Option<u8>) -> (SimMaster, SimTarget) {
    bus.borrow_mut().endpoints.insert(
        address,
        Endpoint {
            broadcast,
            queue: VecDeque::new(),
            capacity: None,
            attached: true,
            dropped: 0,
            waker: None,
        },
    );
    (
        SimMaster {
            bus: bus.clone(),
            address,
        },
        SimTarget {
            bus: bus.clone(),
            address,
        },
    )
}

/// Queues `faults` for the next write attempts of the master at `master`.
pub fn inject(bus: &Shared, master: u8, faults: &[Fault]) {
    bus.borrow_mut()
        .faults
        .entry(master)
        .or_default()
        .extend(faults);
}

/// Limits how many transactions the target at `address` buffers.
pub fn set_capacity(bus: &Shared, address: u8, capacity: usize) {
    if let Some(e) = bus.borrow_mut().endpoints.get_mut(&address) {
        e.capacity = Some(capacity);
    }
}

/// Detaches (`false`) or re-attaches the target at `address`.
pub fn set_attached(bus: &Shared, address: u8, attached: bool) {
    if let Some(e) = bus.borrow_mut().endpoints.get_mut(&address) {
        e.attached = attached;
    }
}

impl Bus {
    /// Write attempts by `from` to `to`.
    pub fn writes(&self, from: u8, to: u8) -> usize {
        self.log
            .iter()
            .filter(|r| r.from == from && r.to == to)
            .count()
    }

    /// Transactions a full broadcast listener at `address` dropped.
    pub fn dropped(&self, address: u8) -> usize {
        self.endpoints.get(&address).map_or(0, |e| e.dropped)
    }

    /// Delivers `bytes` written by `from` to `to`.
    fn deliver(&mut self, from: u8, to: u8, bytes: &[u8]) -> Result<usize, ErrorKind> {
        let targets: Vec<u8> = self
            .endpoints
            .iter()
            .filter(|(a, e)| e.attached && **a != from && (**a == to || e.broadcast == Some(to)))
            .map(|(a, _)| *a)
            .collect();
        if targets.is_empty() {
            return Err(ErrorKind::NoAcknowledge(NoAcknowledgeSource::Address));
        }
        let mut delivered = 0;
        for a in targets {
            let Some(e) = self.endpoints.get_mut(&a) else {
                continue;
            };
            if e.capacity.is_some_and(|c| e.queue.len() >= c) {
                if a == to {
                    return Err(ErrorKind::NoAcknowledge(NoAcknowledgeSource::Data));
                }
                e.dropped += 1;
                continue;
            }
            e.queue.push_back((to, bytes.to_vec()));
            if let Some(w) = e.waker.take() {
                w.wake();
            }
            delivered += 1;
        }
        Ok(delivered)
    }
}

/// Error of a [`SimMaster`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimError(pub ErrorKind);

impl embedded_hal::i2c::Error for SimError {
    fn kind(&self) -> ErrorKind {
        self.0
    }
}

/// A node's master on the simulated bus.
pub struct SimMaster {
    bus: Shared,
    address: u8,
}

impl ErrorType for SimMaster {
    type Error = SimError;
}

/// Yields to the executor once.
async fn yield_once() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}

impl I2c for SimMaster {
    async fn transaction(
        &mut self,
        address: u8,
        operations: &mut [Operation<'_>],
    ) -> Result<(), SimError> {
        let mut bytes = Vec::new();
        for op in operations.iter() {
            match op {
                Operation::Write(w) => bytes.extend_from_slice(w),
                Operation::Read(_) => return Err(SimError(ErrorKind::Other)),
            }
        }
        let me = self.address;
        {
            let mut bus = self.bus.borrow_mut();
            if bus.contention && bus.in_flight.is_some_and(|m| m != me) {
                let kind = ErrorKind::ArbitrationLoss;
                bus.log.push(Record {
                    from: me,
                    to: address,
                    bytes,
                    outcome: Outcome::Failed(kind),
                });
                return Err(SimError(kind));
            }
            bus.in_flight = Some(me);
        }
        yield_once().await;
        let mut bus = self.bus.borrow_mut();
        if bus.in_flight == Some(me) {
            bus.in_flight = None;
        }
        let fault = bus.faults.get_mut(&me).and_then(VecDeque::pop_front);
        let result = match fault {
            Some(Fault::ArbitrationLoss) => Err(ErrorKind::ArbitrationLoss),
            Some(Fault::NackAddress) => Err(ErrorKind::NoAcknowledge(NoAcknowledgeSource::Address)),
            Some(Fault::NackData) => Err(ErrorKind::NoAcknowledge(NoAcknowledgeSource::Data)),
            Some(Fault::Bus) => Err(ErrorKind::Bus),
            Some(Fault::Corrupt(n)) => {
                let mut bad = bytes.clone();
                if let Some(b) = bad.get_mut(n) {
                    *b = !*b;
                }
                bus.deliver(me, address, &bad)
            }
            Some(Fault::Duplicate) => bus
                .deliver(me, address, &bytes)
                .and_then(|n| bus.deliver(me, address, &bytes).map(|m| n + m)),
            None | Some(Fault::Pass) => bus.deliver(me, address, &bytes),
        };
        bus.log.push(Record {
            from: me,
            to: address,
            bytes,
            outcome: match result {
                Ok(n) => Outcome::Delivered(n),
                Err(k) => Outcome::Failed(k),
            },
        });
        result.map(|_| ()).map_err(SimError)
    }
}

/// A node's target adapter on the simulated bus.
pub struct SimTarget {
    bus: Shared,
    address: u8,
}

impl SimTarget {
    fn pop(&self, buf: &mut [u8]) -> Option<Transaction> {
        let mut bus = self.bus.borrow_mut();
        let (to, bytes) = bus.endpoints.get_mut(&self.address)?.queue.pop_front()?;
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Some(Transaction {
            address: to,
            len: bytes.len(),
        })
    }
}

impl I2cTarget for SimTarget {
    type Error = std::convert::Infallible;

    /// Waits for a buffered transaction; consumes it only on completion.
    async fn listen(&mut self, buf: &mut [u8]) -> Result<Transaction, Self::Error> {
        poll_fn(|cx| match self.pop(buf) {
            Some(t) => Poll::Ready(Ok(t)),
            None => {
                if let Some(e) = self.bus.borrow_mut().endpoints.get_mut(&self.address) {
                    e.waker = Some(cx.waker().clone());
                }
                Poll::Pending
            }
        })
        .await
    }

    fn try_listen(&mut self, buf: &mut [u8]) -> Result<Option<Transaction>, Self::Error> {
        Ok(self.pop(buf))
    }
}

/// A [`Clock`] over tokio's timers.
#[derive(Debug, Clone, Copy)]
pub struct TokioClock(Instant);

impl TokioClock {
    /// A clock starting now.
    pub fn new() -> Self {
        TokioClock(Instant::now())
    }
}

impl Clock for TokioClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    async fn delay_us(&self, us: u32) {
        tokio::time::sleep(Duration::from_micros(u64::from(us))).await;
    }
}
