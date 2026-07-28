//! Blocking `io::Write` adapter that republishes whole JSONL lines.
//!
//! The turn engine writes its protocol with a synchronous writer, so the ACP
//! translator cannot borrow it directly. This sink buffers partial writes and
//! forwards each completed line to an async consumer.

use std::io;

use tokio::sync::mpsc::UnboundedSender;

/// Forwards each complete line the engine writes to the translator task.
pub(super) struct LineSink {
    lines: UnboundedSender<String>,
    partial: Vec<u8>,
}

impl LineSink {
    pub(super) fn new(lines: UnboundedSender<String>) -> Self {
        Self {
            lines,
            partial: Vec::new(),
        }
    }

    /// Publishes one line, dropping it when the translator has gone away.
    fn publish(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        match std::str::from_utf8(bytes) {
            Ok(line) => {
                let _ = self.lines.send(line.to_string());
            }
            // The engine only ever writes serde_json output, so this means the
            // stream is corrupt rather than merely unexpected.
            Err(error) => tracing::warn!("dropping non-UTF-8 engine line: {error}"),
        }
    }
}

impl io::Write for LineSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut rest = buf;
        while let Some(position) = rest.iter().position(|byte| *byte == b'\n') {
            let (line, tail) = rest.split_at(position);
            if self.partial.is_empty() {
                self.publish(line);
            } else {
                self.partial.extend_from_slice(line);
                let complete = std::mem::take(&mut self.partial);
                self.publish(&complete);
            }
            rest = &tail[1..];
        }
        self.partial.extend_from_slice(rest);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // A trailing fragment is not a message yet; it is completed by the
        // newline of the next write.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn splits_lines_across_writes() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut sink = LineSink::new(tx);
        sink.write_all(b"{\"a\":1}\n{\"b\"").expect("write");
        sink.write_all(b":2}\n").expect("write");
        assert_eq!(rx.try_recv().expect("first line"), "{\"a\":1}");
        assert_eq!(rx.try_recv().expect("second line"), "{\"b\":2}");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn trailing_fragment_is_withheld_until_terminated() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut sink = LineSink::new(tx);
        sink.write_all(b"{\"partial\":true}").expect("write");
        sink.flush().expect("flush");
        assert!(rx.try_recv().is_err(), "fragment must not be published");
        sink.write_all(b"\n").expect("write");
        assert_eq!(rx.try_recv().expect("line"), "{\"partial\":true}");
    }
}
