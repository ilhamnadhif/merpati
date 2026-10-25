//! A tonic codec for [`DynamicMessage`]s whose type is only known at runtime.

use std::sync::{Arc, Mutex};

use prost::Message as _;
use prost_reflect::{DynamicMessage, MessageDescriptor};
use tonic::Status;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};

/// Encodes any dynamic message; decodes responses as `output`. The last
/// decode failure is also stored in `decode_error` so the caller can tell a
/// client-side decode problem from a server status.
pub(crate) struct DynamicCodec {
    pub output: MessageDescriptor,
    pub decode_error: Arc<Mutex<Option<String>>>,
}

impl Codec for DynamicCodec {
    type Encode = DynamicMessage;
    type Decode = DynamicMessage;
    type Encoder = DynamicEncoder;
    type Decoder = DynamicDecoder;

    fn encoder(&mut self) -> DynamicEncoder {
        DynamicEncoder
    }

    fn decoder(&mut self) -> DynamicDecoder {
        DynamicDecoder { output: self.output.clone(), decode_error: self.decode_error.clone() }
    }
}

pub(crate) struct DynamicEncoder;

impl Encoder for DynamicEncoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn encode(&mut self, item: DynamicMessage, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        item.encode(dst).map_err(|e| Status::internal(format!("failed to encode request: {e}")))
    }
}

pub(crate) struct DynamicDecoder {
    output: MessageDescriptor,
    decode_error: Arc<Mutex<Option<String>>>,
}

impl Decoder for DynamicDecoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<DynamicMessage>, Status> {
        match DynamicMessage::decode(self.output.clone(), src) {
            Ok(msg) => Ok(Some(msg)),
            Err(e) => {
                let msg = format!("failed to decode response as {}: {e}", self.output.full_name());
                if let Ok(mut slot) = self.decode_error.lock() {
                    *slot = Some(msg.clone());
                }
                Err(Status::internal(msg))
            }
        }
    }
}
