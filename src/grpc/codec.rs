//! [`RawCodec`]: a tonic [`Codec`] that passes message bodies through as
//! [`Bytes`].
//!
//! gRPC frames each message with a 5-byte length prefix; tonic handles that
//! framing and hands the codec only the message bytes. This codec copies
//! them unchanged in both directions, so no `.proto` compilation (and no
//! `build.rs`/`protoc`) is needed. Serialization is the job of the crate's
//! own [`Codec`](crate::Codec) layer above the transport.

use bytes::{Buf, BufMut, Bytes};
use tonic::Status;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};

/// Passthrough codec: encodes and decodes [`Bytes`] verbatim.
#[derive(Debug, Clone, Copy, Default)]
pub struct RawCodec;

/// Encoding half of [`RawCodec`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RawEncoder;

/// Decoding half of [`RawCodec`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RawDecoder;

impl Codec for RawCodec {
    type Encode = Bytes;
    type Decode = Bytes;
    type Encoder = RawEncoder;
    type Decoder = RawDecoder;

    fn encoder(&mut self) -> RawEncoder {
        RawEncoder
    }

    fn decoder(&mut self) -> RawDecoder {
        RawDecoder
    }
}

impl Encoder for RawEncoder {
    type Item = Bytes;
    type Error = Status;

    fn encode(&mut self, item: Bytes, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        dst.put(item);
        Ok(())
    }
}

impl Decoder for RawDecoder {
    type Item = Bytes;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Bytes>, Status> {
        let n = src.remaining();
        Ok(Some(src.copy_to_bytes(n)))
    }
}
