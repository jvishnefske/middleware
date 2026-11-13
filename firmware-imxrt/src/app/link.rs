//! The serial link tasks: LPUART1 and (feature `usb`) USB CDC.
//!
//! [`serve`] runs middleware's `SerialLink` (COBS frames, the protocol
//! `mwctl` speaks) over any byte stream, as firmware-rp's link does over USB
//! CDC: it waits for whichever comes first, a frame from the host or a
//! publication queued for this link; requests and publications are copied
//! into an [`Inbound`] tagged with the link for the controller, a request's
//! reply comes back on the bus for this link only and goes out as the
//! matching response (then [`Bus::reply_sent`], which a reset waits for),
//! and publications go out as publish frames. Responses from the host are
//! ignored (the device sends no requests).
//!
//! Sends are bounded by [`SEND_TIMEOUT`]; after one times out the link
//! drops publications until the host sends a frame again
//! (`imxrt_base::usb::Host`, the RP base firmware's rule; LPUART1's sends
//! always complete). Waiting on `next_frame` is
//! cancelled when a publication wins the race; the link keeps its
//! partial-frame state in itself and the receive side keeps the bytes
//! (LPUART1's DMA ring, the CDC class's buffer), so no input is lost.
//!
//! The bootloader prints text on LPUART1 before the firmware starts; the
//! host's frame decoder skips it.

use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Timer, with_timeout};
use embedded_io_async::{Read, Write};
use imxrt_base::links::LinkId;
use imxrt_base::usb::Host;
use middleware::serial::frame::{Frame, MAX_RAW_FRAME};
use middleware::serial::link::{Duplex, LinkError, SerialLink};
use middleware::transport::HandlerError;

use super::bus::{Bus, Inbound, Kind};
use super::uart::{UartRx, UartTx};

/// Longest a single frame write may take before it is abandoned: a
/// largest frame (about 1.1 KB) takes 95 ms at 115200 baud.
const SEND_TIMEOUT: Duration = Duration::from_millis(400);

/// Pause after a stream error (USB not configured), so the task cannot
/// spin.
const ERROR_BACKOFF: Duration = Duration::from_millis(10);

/// Inbox of a link: none. The task reads with `next_frame` and never
/// waits in `request_with` or `serve_one`, as on esp-base.
pub const INBOX: usize = 0;

/// The link over LPUART1.
pub type UartLink = SerialLink<Duplex<UartRx, UartTx>, INBOX>;

/// The link over the UART halves.
pub fn new(rx: UartRx, tx: UartTx) -> UartLink {
    SerialLink::with_inbox(Duplex::new(rx, tx))
}

/// Sends one frame; `false` when it timed out or failed.
async fn send<IO: Read + Write>(link: &mut SerialLink<IO, INBOX>, frame: &Frame<'_>) -> bool {
    matches!(with_timeout(SEND_TIMEOUT, link.send(frame)).await, Ok(Ok(())))
}

/// Runs the protocol on one serial link forever.
pub async fn serve<IO: Read + Write>(mut link: SerialLink<IO, INBOX>, id_of_link: LinkId, bus: &'static Bus) -> ! {
    let mut buf = [0u8; MAX_RAW_FRAME];
    let mut host = Host::Present;
    loop {
        match select(link.next_frame(&mut buf), bus.next_outbound(id_of_link)).await {
            Either::First(Ok(Frame::Request { id, key, payload })) => {
                host = host.after_receive();
                let result = match Inbound::new(id_of_link, Kind::Request { id }, key, payload, link.peer()) {
                    Some(msg) => {
                        bus.send_inbound(msg).await;
                        bus.wait_reply(id_of_link, id).await.result
                    }
                    None => Err(HandlerError::BufferTooSmall),
                };
                let body = result.as_ref().map(|v| v.as_slice()).map_err(|e| *e);
                host = host.after_send(send(&mut link, &Frame::Response { id, result: body }).await);
                bus.reply_sent(id_of_link, id);
            }
            Either::First(Ok(Frame::Publish { key, payload })) => {
                host = host.after_receive();
                if let Some(msg) = Inbound::new(id_of_link, Kind::Publish, key, payload, link.peer()) {
                    bus.send_inbound(msg).await;
                }
            }
            Either::First(Ok(Frame::Response { .. })) => host = host.after_receive(),
            // A bad frame is skipped.
            Either::First(Err(LinkError::BufferTooSmall | LinkError::Frame(_))) => {}
            // The stream failed (USB not configured): back off.
            Either::First(Err(_)) => Timer::after(ERROR_BACKOFF).await,
            // While the host is away (a CDC port nobody reads), dropped.
            Either::Second(msg) => {
                if host.sends_publications() {
                    host = host.after_send(send(&mut link, &Frame::Publish { key: msg.key(), payload: &msg.payload }).await);
                }
            }
        }
    }
}
