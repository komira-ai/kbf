//! The codec of every call: the request is encoded by prost as tonic's own codec does, and
//! the answer's bytes are decoded only after tonic has read the whole call. tonic's codec
//! turns an answer that does not decode into an `INTERNAL` status, which a server may also
//! answer; this one keeps the decode error, so [`Error::Decode`](crate::Error::Decode) is
//! told apart from a status of the server.

use std::marker::PhantomData;

use bytes::Buf as _;
use prost::Message;
use tonic::Status;
use tonic::codec::{Codec, DecodeBuf, Decoder};
use tonic_prost::ProstEncoder;

/// The codec of a call that sends `Req` and is answered `Resp`.
pub(crate) struct Lenient<Req, Resp>(PhantomData<fn(Req) -> Resp>);

impl<Req, Resp> Lenient<Req, Resp> {
    pub(crate) fn new() -> Self {
        Self(PhantomData)
    }
}

impl<Req, Resp> Codec for Lenient<Req, Resp>
where
    Req: Message + Send + 'static,
    Resp: Message + Default + Send + 'static,
{
    type Encode = Req;
    type Decode = Result<Resp, prost::DecodeError>;
    type Encoder = ProstEncoder<Req>;
    type Decoder = LenientDecoder<Resp>;

    fn encoder(&mut self) -> Self::Encoder {
        ProstEncoder::default()
    }

    fn decoder(&mut self) -> Self::Decoder {
        LenientDecoder(PhantomData)
    }
}

/// Decodes the one message of an answer and hands tonic the result, error or not.
pub(crate) struct LenientDecoder<Resp>(PhantomData<fn() -> Resp>);

impl<Resp: Message + Default> Decoder for LenientDecoder<Resp> {
    type Item = Result<Resp, prost::DecodeError>;
    type Error = Status;

    fn decode(&mut self, buf: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Status> {
        // The whole message is taken, so a message that does not decode leaves nothing
        // behind in tonic's buffer.
        let bytes = buf.copy_to_bytes(buf.remaining());
        Ok(Some(Resp::decode(bytes)))
    }
}
