//! Guest stdout and stderr, line-buffered into `tracing` (sub-spec 3 §2.2).

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use tokio::io::AsyncWrite;
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};
use wasmtime_wasi::p2::{OutputStream, Pollable, StreamResult};

/// The longest line logged; the rest of a longer line is dropped.
const MAX_LINE: usize = 4096;
/// Bytes one write may carry, as reported by `check-write`.
const WRITE_CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy)]
pub(crate) enum Stream {
    Stdout,
    Stderr,
}

/// A guest's stdout or stderr: every call to `async_stream` / `p2_stream` yields a stream
/// with its own line buffer, logging under `service`.
pub(crate) struct GuestOutput {
    service: String,
    stream: Stream,
}

impl GuestOutput {
    pub(crate) fn new(service: &str, stream: Stream) -> Self {
        GuestOutput {
            service: service.to_owned(),
            stream,
        }
    }

    fn sink(&self) -> LineSink {
        LineSink {
            service: self.service.clone(),
            stream: self.stream,
            line: Vec::new(),
        }
    }
}

impl IsTerminal for GuestOutput {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for GuestOutput {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(self.sink())
    }

    fn p2_stream(&self) -> Box<dyn OutputStream> {
        Box::new(self.sink())
    }
}

/// Splits written bytes into lines and logs each. A partial last line is logged when the
/// sink is dropped, which happens when the instance's resource table is.
struct LineSink {
    service: String,
    stream: Stream,
    /// The current line, at most [`MAX_LINE`] bytes; the rest of a longer line is dropped.
    line: Vec<u8>,
}

impl LineSink {
    fn push(&mut self, bytes: &[u8]) {
        for part in bytes.split_inclusive(|&b| b == b'\n') {
            let (text, ended) = match part.split_last() {
                Some((&b'\n', rest)) => (rest, true),
                _ => (part, false),
            };
            let room = MAX_LINE - self.line.len();
            self.line.extend_from_slice(&text[..text.len().min(room)]);
            if ended {
                self.emit();
            }
        }
    }

    fn emit(&mut self) {
        let line = std::mem::take(&mut self.line);
        let line = line.strip_suffix(b"\r").unwrap_or(&line);
        if line.is_empty() {
            return;
        }
        let line = String::from_utf8_lossy(line);
        match self.stream {
            Stream::Stdout => tracing::info!(service = %self.service, "{line}"),
            Stream::Stderr => tracing::warn!(service = %self.service, "{line}"),
        }
    }
}

impl Drop for LineSink {
    fn drop(&mut self) {
        self.emit();
    }
}

impl OutputStream for LineSink {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        self.push(&bytes);
        Ok(())
    }

    fn flush(&mut self) -> StreamResult<()> {
        // A flush does not end a line: the rest of it may still be written.
        Ok(())
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        Ok(WRITE_CHUNK)
    }
}

#[async_trait::async_trait]
impl Pollable for LineSink {
    async fn ready(&mut self) {}
}

impl AsyncWrite for LineSink {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.push(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_line_keeps_a_prefix_and_resets_at_newline() {
        let mut sink = GuestOutput::new("t", Stream::Stdout).sink();
        sink.push(&vec![b'a'; MAX_LINE + 10]);
        assert_eq!(sink.line.len(), MAX_LINE);
        sink.push(b"more");
        assert_eq!(sink.line.len(), MAX_LINE);
        sink.push(b"\nnext");
        assert_eq!(sink.line, b"next");
    }
}
