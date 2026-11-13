//! The LPUART1 link task.
//!
//! [`serve`] runs middleware's `SerialLink` (COBS frames, the protocol
//! `mwctl` speaks) over the UART's two halves ([`super::uart`]) joined as a
//! `Duplex`, as firmware-rp's link does over USB CDC: it waits for
//! whichever comes first, a frame from the host or an outbound
//! publication; requests and publications are copied into an [`Inbound`]
//! for the controller, a request's reply comes back on the bus and goes
//! out as the matching response (then [`Bus::reply_sent`], which a reset
//! waits for), and publications go out as publish frames. Responses from
//! the host are ignored (the device sends no requests).
//!
//! A UART has no "host absent": every write completes at the line rate,
//! so publications are always sent (the RP's host-presence rule is not
//! needed). Sends are still bounded by [`SEND_TIMEOUT`]. Waiting on
//! `next_frame` is cancelled when a publication wins the race; the link
//! keeps its partial-frame state in itself and the receive ring keeps the
//! bytes, so no input is lost.
//!
//! The bootloader prints text on the same port before the firmware starts;
//! the host's frame decoder skips it (no frame delimiter, or a frame that
//! fails to decode).

use embassy_futures::select::{Either, select};
use embassy_time::{Duration, with_timeout};
use middleware::serial::frame::{Frame, MAX_RAW_FRAME};
use middleware::serial::link::{Duplex, SerialLink};
use middleware::transport::HandlerError;

use super::bus::{Bus, Inbound, Kind};
use super::uart::{UartRx, UartTx};

/// Longest a single frame write may take before it is abandoned: a
/// largest frame (about 1.1 KB) takes 95 ms at 115200 baud.
const SEND_TIMEOUT: Duration = Duration::from_millis(400);

/// Inbox of the link: none. The task reads with `next_frame` and never
/// waits in `request_with` or `serve_one`, as on esp-base.
const INBOX: usize = 0;

/// The link over LPUART1.
pub type UartLink = SerialLink<Duplex<UartRx, UartTx>, INBOX>;

/// The link over the UART halves.
pub fn new(rx: UartRx, tx: UartTx) -> UartLink {
    SerialLink::with_inbox(Duplex::new(rx, tx))
}

/// Sends one frame; `false` when it timed out or failed.
async fn send(link: &mut UartLink, frame: &Frame<'_>) -> bool {
    matches!(with_timeout(SEND_TIMEOUT, link.send(frame)).await, Ok(Ok(())))
}

/// Runs the protocol on LPUART1 forever.
pub async fn serve(mut link: UartLink, bus: &'static Bus) {
    let Some(mut outbound) = bus.subscribe() else {
        bus.log(format_args!("link: no outbound subscriber slot"));
        return;
    };
    let mut buf = [0u8; MAX_RAW_FRAME];
    loop {
        match select(link.next_frame(&mut buf), outbound.next_message_pure()).await {
            Either::First(Ok(Frame::Request { id, key, payload })) => {
                let result = match Inbound::new(Kind::Request { id }, key, payload, link.peer()) {
                    Some(msg) => {
                        bus.send_inbound(msg).await;
                        bus.wait_reply(id).await.result
                    }
                    None => Err(HandlerError::BufferTooSmall),
                };
                let body = result.as_ref().map(|v| v.as_slice()).map_err(|e| *e);
                send(&mut link, &Frame::Response { id, result: body }).await;
                bus.reply_sent(id);
            }
            Either::First(Ok(Frame::Publish { key, payload })) => {
                if let Some(msg) = Inbound::new(Kind::Publish, key, payload, link.peer()) {
                    bus.send_inbound(msg).await;
                }
            }
            Either::First(Ok(Frame::Response { .. })) => {}
            // The UART never fails; a bad frame is skipped.
            Either::First(Err(_)) => {}
            Either::Second(msg) => {
                if let Ok(key) = middleware::Key::new(&msg.key) {
                    send(&mut link, &Frame::Publish { key, payload: &msg.payload }).await;
                }
            }
        }
    }
}
