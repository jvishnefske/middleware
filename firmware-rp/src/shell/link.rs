//! The USB CDC-ACM link task and its writer.
//!
//! [`serve`] runs middleware's `SerialLink` (COBS frames, the protocol
//! `mwctl` speaks) over the CDC class's two halves joined as a `Duplex`:
//! the buffered packet receiver and [`CdcWriter`]. It waits for whichever
//! comes first, a frame from the host or an outbound publication, as
//! esp-base's link does: requests and publications are copied into an
//! [`Inbound`] for the controller, a request's reply comes back on the bus
//! and goes out as the matching response, and publications go out as
//! publish frames while the host is reading ([`rp_base::usb::Host`]).
//! Responses from the host are ignored (the device sends no requests).
//!
//! Sends are bounded by [`SEND_TIMEOUT`]: a CDC write blocks while no host
//! program reads the port. Waiting on `next_frame` is cancelled when a
//! publication wins the race; the link keeps its partial-frame state in
//! itself and the packet reads complete only with data, so no input is
//! lost.

use embassy_futures::select::{Either, select};
use embassy_rp::peripherals::USB;
use embassy_rp::usb::Driver;
use embassy_time::{Duration, Timer, with_timeout};
use embassy_usb::class::cdc_acm::{BufferedReceiver, Sender};
use embassy_usb::driver::EndpointError;
use embedded_io_async::{ErrorKind, ErrorType, Write};
use middleware::serial::frame::{Frame, MAX_RAW_FRAME};
use middleware::serial::link::{Duplex, LinkError, SerialLink};
use middleware::transport::HandlerError;
use rp_base::usb::{Host, MAX_PACKET, needs_zlp};

use super::bus::{Bus, Inbound, Kind};

/// Longest a single frame write may take before it is abandoned.
const SEND_TIMEOUT: Duration = Duration::from_millis(200);

/// Pause after a stream error (USB not configured), so the task cannot
/// spin.
const ERROR_BACKOFF: Duration = Duration::from_millis(10);

/// Inbox of the link: none. The task reads with `next_frame` and never
/// waits in `request_with` or `serve_one`, as on esp-base.
const INBOX: usize = 0;

/// The USB driver type.
pub type UsbDriver = Driver<'static, USB>;

/// A CDC write failed: the host has not configured the interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotConnected;

impl core::fmt::Display for NotConnected {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("USB CDC not connected")
    }
}

impl core::error::Error for NotConnected {}

impl embedded_io_async::Error for NotConnected {
    fn kind(&self) -> ErrorKind {
        ErrorKind::NotConnected
    }
}

/// The CDC sender as a byte writer: one packet per `write` and a
/// zero-length packet on `flush` after a full one ([`needs_zlp`]), so a
/// frame ending on a packet boundary reaches the host at once.
pub struct CdcWriter {
    tx: Sender<'static, UsbDriver>,
    /// Bytes in the last packet written since the previous flush.
    last: usize,
}

impl CdcWriter {
    /// Wraps the sending half.
    pub fn new(tx: Sender<'static, UsbDriver>) -> Self {
        CdcWriter { tx, last: 0 }
    }

    async fn packet(&mut self, data: &[u8]) -> Result<(), NotConnected> {
        match self.tx.write_packet(data).await {
            Ok(()) => Ok(()),
            Err(EndpointError::BufferOverflow | EndpointError::Disabled) => Err(NotConnected),
        }
    }
}

impl ErrorType for CdcWriter {
    type Error = NotConnected;
}

impl Write for CdcWriter {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        let n = buf.len().min(usize::from(MAX_PACKET));
        let packet = buf.get(..n).unwrap_or_default();
        self.packet(packet).await?;
        self.last = n;
        Ok(n)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        if needs_zlp(self.last, MAX_PACKET) {
            self.packet(&[]).await?;
        }
        self.last = 0;
        Ok(())
    }
}

/// The link over the CDC class.
pub type UsbLink = SerialLink<Duplex<BufferedReceiver<'static, UsbDriver>, CdcWriter>, INBOX>;

/// Sends one frame; `false` when it timed out or failed.
async fn send(link: &mut UsbLink, frame: &Frame<'_>) -> bool {
    matches!(with_timeout(SEND_TIMEOUT, link.send(frame)).await, Ok(Ok(())))
}

/// Runs the protocol on the CDC port forever.
pub async fn serve(mut link: UsbLink, bus: &'static Bus) {
    let Some(mut outbound) = bus.subscribe() else {
        bus.log(format_args!("link: no outbound subscriber slot"));
        return;
    };
    let mut buf = [0u8; MAX_RAW_FRAME];
    let mut host = Host::Present;
    loop {
        match select(link.next_frame(&mut buf), outbound.next_message_pure()).await {
            Either::First(Ok(Frame::Request { id, key, payload })) => {
                host = host.after_receive();
                let result = match Inbound::new(Kind::Request { id }, key, payload) {
                    Some(msg) => {
                        bus.send_inbound(msg).await;
                        bus.wait_reply(id).await.result
                    }
                    None => Err(HandlerError::BufferTooSmall),
                };
                let body = result.as_ref().map(|v| v.as_slice()).map_err(|e| *e);
                let sent = send(&mut link, &Frame::Response { id, result: body }).await;
                host = host.after_send(sent);
            }
            Either::First(Ok(Frame::Publish { key, payload })) => {
                host = host.after_receive();
                if let Some(msg) = Inbound::new(Kind::Publish, key, payload) {
                    bus.send_inbound(msg).await;
                }
            }
            Either::First(Ok(Frame::Response { .. })) => host = host.after_receive(),
            Either::First(Err(LinkError::BufferTooSmall | LinkError::Frame(_))) => {}
            Either::First(Err(_)) => Timer::after(ERROR_BACKOFF).await,
            Either::Second(msg) => {
                if host.sends_publications()
                    && let Ok(key) = middleware::Key::new(&msg.key)
                {
                    let sent = send(&mut link, &Frame::Publish { key, payload: &msg.payload }).await;
                    host = host.after_send(sent);
                }
            }
        }
    }
}
