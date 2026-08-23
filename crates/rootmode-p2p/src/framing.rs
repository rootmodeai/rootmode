//! Newline-delimited JSON over a libp2p stream.
//!
//! Deliberately the same messages the WebSocket transport carries — one
//! RootmodeProtocol v1 value per line. Keeping the framing this dull means the
//! p2p path and the direct path cannot drift in meaning, only in how the bytes
//! arrive.

use libp2p::futures::io::{AsyncRead, AsyncWrite, BufReader, ReadHalf, WriteHalf};
use libp2p::futures::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use serde::Serialize;

use crate::{P2pError, Result};

/// Matches the WebSocket transport's frame cap.
pub const MAX_LINE_BYTES: usize = rootmode_core::protocol::MAX_MESSAGE_BYTES;

pub struct JsonStream<S: AsyncRead + AsyncWrite + Unpin> {
    reader: BufReader<ReadHalf<S>>,
    writer: WriteHalf<S>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> JsonStream<S> {
    pub fn new(stream: S) -> Self {
        let (read, write) = stream.split();
        Self {
            reader: BufReader::new(read),
            writer: write,
        }
    }

    pub async fn send<T: Serialize>(&mut self, message: &T) -> Result<()> {
        let mut line = rootmode_core::canonical::wire_json(message)
            .map_err(|e| P2pError::Stream(e.to_string()))?
            .into_bytes();
        line.push(b'\n');
        self.writer
            .write_all(&line)
            .await
            .map_err(|e| P2pError::Stream(e.to_string()))?;
        self.writer
            .flush()
            .await
            .map_err(|e| P2pError::Stream(e.to_string()))
    }

    /// Next line, or `None` at end of stream.
    ///
    /// Bounded: a peer that never sends a newline cannot make us buffer
    /// forever.
    pub async fn recv(&mut self) -> Result<Option<String>> {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let (chunk, newline_at) = {
                let available = self
                    .reader
                    .fill_buf()
                    .await
                    .map_err(|e| P2pError::Stream(e.to_string()))?;
                if available.is_empty() {
                    return if buf.is_empty() {
                        Ok(None)
                    } else {
                        finish(buf).map(Some)
                    };
                }
                match available.iter().position(|b| *b == b'\n') {
                    Some(i) => (available[..i].to_vec(), Some(i)),
                    None => (available.to_vec(), None),
                }
            };

            let consumed = chunk.len() + usize::from(newline_at.is_some());
            buf.extend_from_slice(&chunk);
            self.reader.consume_unpin(consumed);

            if newline_at.is_some() {
                return finish(buf).map(Some);
            }
            if buf.len() > MAX_LINE_BYTES {
                return Err(P2pError::Stream(format!(
                    "peer sent more than {MAX_LINE_BYTES} bytes without a newline"
                )));
            }
        }
    }

    pub async fn close(mut self) {
        let _ = self.writer.close().await;
    }
}

fn finish(buf: Vec<u8>) -> Result<String> {
    String::from_utf8(buf).map_err(|e| P2pError::Stream(format!("not utf-8: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::futures::io::Cursor;

    /// A duplex pair: a cursor to read from, a shared buffer to write into, so
    /// framing can be tested without a network.
    struct Pair {
        read: Cursor<Vec<u8>>,
        write: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl AsyncRead for Pair {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::pin::Pin::new(&mut self.read).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for Pair {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.write.lock().unwrap().extend_from_slice(buf);
            std::task::Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn reading(input: &str) -> JsonStream<Pair> {
        JsonStream::new(Pair {
            read: Cursor::new(input.as_bytes().to_vec()),
            write: Default::default(),
        })
    }

    #[tokio::test]
    async fn splits_on_newlines_and_ends_cleanly() {
        let mut s = reading("{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(s.recv().await.unwrap().as_deref(), Some("{\"a\":1}"));
        assert_eq!(s.recv().await.unwrap().as_deref(), Some("{\"b\":2}"));
        assert_eq!(s.recv().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_trailing_line_without_a_newline_still_arrives() {
        let mut s = reading("{\"a\":1}");
        assert_eq!(s.recv().await.unwrap().as_deref(), Some("{\"a\":1}"));
        assert_eq!(s.recv().await.unwrap(), None);
    }

    #[tokio::test]
    async fn writes_one_line_per_message() {
        let out: std::sync::Arc<std::sync::Mutex<Vec<u8>>> = Default::default();
        let mut s = JsonStream::new(Pair {
            read: Cursor::new(Vec::new()),
            write: out.clone(),
        });
        s.send(&serde_json::json!({ "type": "peer.hello" }))
            .await
            .unwrap();
        s.send(&serde_json::json!({ "type": "job.submit" }))
            .await
            .unwrap();

        let written = String::from_utf8(out.lock().unwrap().clone()).unwrap();
        assert_eq!(written.lines().count(), 2);
        assert!(written.ends_with('\n'));
    }
}
