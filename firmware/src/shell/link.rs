//! Serial link tasks: one per port, each owning its `SerialLink`.
//!
//! [`serve`] waits for whichever comes first: a frame from the host or an
//! outbound publication. Requests and publications are copied into an
//! [`Inbound`] for the controller; a request's reply comes back on this
//! port's reply signal and is sent as the matching response. Outbound
//! publications go out as publish frames. Responses from the host are
//! ignored (the device sends no requests).
//!
//! Before the link starts, [`prime`] writes a single `0x00` to the port.
//!
//! Sends are bounded by [`SEND_TIMEOUT`]: USB-Serial-JTAG blocks until a host
//! reads, and a port with nobody listening must not stall the task. A frame
//! cut short by a timeout fails its CRC on the host and is skipped.
//!
//! Waiting on `next_frame` is cancelled when a publication wins the race;
//! the link keeps its partial-frame state in itself, and the esp-hal async
//! reads only await before data is consumed, so no input is lost.

use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Timer, with_timeout};
use embedded_io_async::{Read, Write};
use middleware::serial::frame::{Frame, MAX_RAW_FRAME};
use middleware::serial::link::{LinkError, SerialLink};
use middleware::transport::HandlerError;

use super::bus::{Bus, Inbound, Kind, Port};

/// Longest a single frame write may take before it is abandoned.
const SEND_TIMEOUT: Duration = Duration::from_millis(200);

/// Pause after a stream error, so a failing port cannot spin.
const ERROR_BACKOFF: Duration = Duration::from_millis(10);

/// Writes one `0x00` delimiter so the first frame is never glued to the
/// tail of a ROM boot banner without a trailing newline (the host decoder
/// treats everything before a delimiter as one run). An extra delimiter is
/// an empty run, which COBS framing ignores.
pub async fn prime<IO: Write>(io: &mut IO) {
    let _ = with_timeout(SEND_TIMEOUT, async {
        let _ = io.write_all(&[0]).await;
        let _ = io.flush().await;
    })
    .await;
}

async fn send<IO: Read + Write>(link: &mut SerialLink<IO>, frame: &Frame<'_>) {
    // Failures and timeouts drop the frame; the host retries requests.
    let _ = with_timeout(SEND_TIMEOUT, link.send(frame)).await;
}

/// Runs the protocol on one port forever.
pub async fn serve<IO: Read + Write>(mut link: SerialLink<IO>, port: Port, bus: &'static Bus) {
    let Some(mut outbound) = bus.subscribe() else {
        bus.log(format_args!("link {port:?}: no outbound subscriber slot"));
        return;
    };
    let mut buf = [0u8; MAX_RAW_FRAME];
    loop {
        match select(link.next_frame(&mut buf), outbound.next_message_pure()).await {
            Either::First(Ok(Frame::Request { id, key, payload })) => {
                let result = match Inbound::new(port, Kind::Request { id }, key, payload) {
                    Some(msg) => {
                        bus.send_inbound(msg).await;
                        bus.wait_reply(port, id).await.result
                    }
                    None => Err(HandlerError::BufferTooSmall),
                };
                let body = result.as_ref().map(|v| v.as_slice()).map_err(|e| *e);
                send(&mut link, &Frame::Response { id, result: body }).await;
            }
            Either::First(Ok(Frame::Publish { key, payload })) => {
                if let Some(msg) = Inbound::new(port, Kind::Publish, key, payload) {
                    bus.send_inbound(msg).await;
                }
            }
            Either::First(Ok(Frame::Response { .. })) => {}
            Either::First(Err(LinkError::BufferTooSmall | LinkError::Frame(_))) => {}
            Either::First(Err(_)) => Timer::after(ERROR_BACKOFF).await,
            Either::Second(msg) => {
                if let Ok(key) = middleware::Key::new(&msg.key) {
                    send(&mut link, &Frame::Publish { key, payload: &msg.payload }).await;
                }
            }
        }
    }
}
