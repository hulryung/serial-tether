//! NDJSON framing — one message per line (`\n` terminated).

use bytes::{Buf, BufMut, BytesMut};
use thiserror::Error;
use tokio_util::codec::{Decoder, Encoder};

use crate::message::Message;

/// Maximum line length in bytes. Connections that exceed this are dropped.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid utf-8")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("line exceeded {MAX_LINE_BYTES} bytes")]
    LineTooLong,
}

#[derive(Debug, Default)]
pub struct NdjsonCodec {
    /// Next search position — the length of the prefix already known to contain no LF.
    next_index: usize,
}

impl NdjsonCodec {
    pub fn new() -> Self { Self::default() }
}

impl Decoder for NdjsonCodec {
    type Item = Message;
    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let read_to = src.len();
        if let Some(rel) = src[self.next_index..read_to].iter().position(|b| *b == b'\n') {
            let newline_at = self.next_index + rel;
            let line_len = newline_at + 1;
            if line_len > MAX_LINE_BYTES {
                return Err(CodecError::LineTooLong);
            }
            let mut line = src.split_to(line_len);
            // strip trailing \n (and optional \r)
            line.truncate(line.len() - 1);
            if line.last() == Some(&b'\r') {
                line.truncate(line.len() - 1);
            }
            self.next_index = 0;
            if line.is_empty() {
                // skip blank line
                return self.decode(src);
            }
            let msg: Message = serde_json::from_slice(&line)?;
            Ok(Some(msg))
        } else {
            if read_to > MAX_LINE_BYTES {
                return Err(CodecError::LineTooLong);
            }
            self.next_index = read_to;
            Ok(None)
        }
    }
}

impl Encoder<Message> for NdjsonCodec {
    type Error = CodecError;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let bytes = serde_json::to_vec(&item)?;
        dst.reserve(bytes.len() + 1);
        dst.put_slice(&bytes);
        dst.put_u8(b'\n');
        Ok(())
    }
}

// Convenience — encode each message variant directly.
impl Encoder<crate::message::Request> for NdjsonCodec {
    type Error = CodecError;
    fn encode(&mut self, item: crate::message::Request, dst: &mut BytesMut) -> Result<(), Self::Error> {
        Encoder::<Message>::encode(self, Message::Request(item), dst)
    }
}
impl Encoder<crate::message::Response> for NdjsonCodec {
    type Error = CodecError;
    fn encode(&mut self, item: crate::message::Response, dst: &mut BytesMut) -> Result<(), Self::Error> {
        Encoder::<Message>::encode(self, Message::Response(item), dst)
    }
}
impl Encoder<crate::message::Notification> for NdjsonCodec {
    type Error = CodecError;
    fn encode(&mut self, item: crate::message::Notification, dst: &mut BytesMut) -> Result<(), Self::Error> {
        Encoder::<Message>::encode(self, Message::Notification(item), dst)
    }
}

// "advance past consumed prefix" helper — unused after migrating to split_to.
#[allow(dead_code)]
fn advance(buf: &mut BytesMut, n: usize) { buf.advance(n); }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Message, Notification, Request, RpcId};

    #[test]
    fn encode_decode_round() {
        let mut codec = NdjsonCodec::new();
        let mut buf = BytesMut::new();

        let req = Request::new(RpcId::Number(7), "ping", serde_json::json!({}));
        Encoder::<Message>::encode(&mut codec, Message::Request(req), &mut buf).unwrap();

        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        match decoded {
            Message::Request(r) => assert_eq!(r.method, "ping"),
            _ => panic!(),
        }
    }

    #[test]
    fn split_across_chunks() {
        let mut codec = NdjsonCodec::new();
        let mut buf = BytesMut::new();
        let line = br#"{"jsonrpc":"2.0","method":"data","params":{"session_id":"s","seq":1,"data":"YQ=="}}"#;
        // first half
        buf.extend_from_slice(&line[..20]);
        assert!(codec.decode(&mut buf).unwrap().is_none());
        // rest + newline
        buf.extend_from_slice(&line[20..]);
        buf.extend_from_slice(b"\n");
        let m = codec.decode(&mut buf).unwrap().unwrap();
        match m {
            Message::Notification(Notification { method, .. }) => assert_eq!(method, "data"),
            _ => panic!(),
        }
    }

    #[test]
    fn skips_blank_lines() {
        let mut codec = NdjsonCodec::new();
        let mut buf = BytesMut::new();
        buf.extend_from_slice(b"\n\n");
        buf.extend_from_slice(br#"{"jsonrpc":"2.0","id":1,"method":"x"}"#);
        buf.extend_from_slice(b"\n");
        let m = codec.decode(&mut buf).unwrap().unwrap();
        match m {
            Message::Request(r) => assert_eq!(r.method, "x"),
            _ => panic!(),
        }
    }
}
