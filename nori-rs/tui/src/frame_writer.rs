//! A terminal writer that sends one frame with one `write(2)`.
//!
//! The standard `Stdout` is a line-buffered writer with a small buffer, so a frame that contains
//! many newlines (history lines are joined with `\r\n`) or more than a kilobyte becomes many
//! syscalls, and the terminal can paint a half-written frame between them. [`FrameWriter`]
//! collects everything a frame writes into one buffer and hands it to the terminal in a single
//! write when the frame is complete.

use std::fs::File;
use std::io;
use std::io::Write;

/// A buffer that has grown past this is shrunk back after it is sent.
const RETAINED_CAPACITY: usize = 256 * 1024;

pub struct FrameWriter {
    out: CountingWriter,
    buffer: Vec<u8>,
    /// Inside a frame, `flush` keeps collecting; the frame ends with [`Self::end_frame`].
    in_frame: bool,
    /// With `NORI_FRAME_TRACE=<path>`, one line per commit: epoch microseconds, bytes, and the
    /// number of `write` calls it took. Lets a probe verify one write per frame.
    trace: Option<File>,
}

/// Counts the `write` calls made through it.
struct CountingWriter {
    inner: Box<dyn Write + Send>,
    calls: usize,
}

impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.calls += 1;
        self.inner.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl FrameWriter {
    /// A writer for this process' standard output.
    ///
    /// Standard output is duplicated into a plain file handle so that a frame is written with a
    /// single `write` and no extra buffering in between.
    pub fn stdout() -> io::Result<Self> {
        let duplicate = stdout_file()?;
        Ok(Self::new(duplicate))
    }

    pub fn new(out: impl Write + Send + 'static) -> Self {
        let trace = std::env::var_os("NORI_FRAME_TRACE").and_then(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
        });
        Self {
            out: CountingWriter {
                inner: Box::new(out),
                calls: 0,
            },
            buffer: Vec::with_capacity(16 * 1024),
            in_frame: false,
            trace,
        }
    }

    /// Start collecting a frame: flushing does not send anything until [`Self::end_frame`].
    pub fn begin_frame(&mut self) {
        self.in_frame = true;
    }

    /// Send the frame, in one write, and go back to sending on every flush.
    pub fn end_frame(&mut self) -> io::Result<()> {
        self.in_frame = false;
        self.commit()
    }

    /// Bytes collected and not yet sent.
    pub fn pending(&self) -> &[u8] {
        &self.buffer
    }

    fn commit(&mut self) -> io::Result<()> {
        // Terminal status reports ride in front of the frame, in the same write, so that they
        // can never land inside one.
        let status = crate::terminal_status::take_pending();
        if !status.is_empty() {
            self.buffer.splice(0..0, status);
        }
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.out.calls = 0;
        let result = self
            .out
            .write_all(&self.buffer)
            .and_then(|()| self.out.flush());
        if let Some(trace) = self.trace.as_mut() {
            let micros = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_micros());
            let _ = writeln!(trace, "{micros} {} {}", self.buffer.len(), self.out.calls);
        }
        self.buffer.clear();
        if self.buffer.capacity() > RETAINED_CAPACITY {
            self.buffer.shrink_to(RETAINED_CAPACITY);
        }
        result
    }
}

impl Write for FrameWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.in_frame { Ok(()) } else { self.commit() }
    }
}

#[cfg(unix)]
fn stdout_file() -> io::Result<File> {
    use std::os::fd::AsFd;
    Ok(File::from(std::io::stdout().as_fd().try_clone_to_owned()?))
}

#[cfg(windows)]
fn stdout_file() -> io::Result<File> {
    use std::os::windows::io::AsHandle;
    Ok(File::from(
        std::io::stdout().as_handle().try_clone_to_owned()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;
    use std::sync::Mutex;

    /// Records each `write` call separately.
    #[derive(Clone, Default)]
    struct CountingSink(Arc<Mutex<Vec<Vec<u8>>>>);

    impl Write for CountingSink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().push(bytes.to_vec());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_frame_with_many_newlines_is_one_write() {
        let sink = CountingSink::default();
        let mut writer = FrameWriter::new(sink.clone());
        writer.begin_frame();
        for line in 0..500 {
            write!(writer, "history line {line}\r\n").unwrap();
            writer.flush().unwrap();
        }
        writer.end_frame().unwrap();

        let writes = sink.0.lock().unwrap();
        assert_eq!(writes.len(), 1);
        assert!(writes[0].len() > 8000, "{}", writes[0].len());
    }

    #[test]
    fn flush_outside_a_frame_sends_immediately() {
        let sink = CountingSink::default();
        let mut writer = FrameWriter::new(sink.clone());
        write!(writer, "enter alt screen").unwrap();
        writer.flush().unwrap();

        assert_eq!(sink.0.lock().unwrap().len(), 1);
        assert_eq!(writer.pending(), b"");
    }

    #[test]
    fn nothing_is_sent_for_an_empty_frame() {
        let sink = CountingSink::default();
        let mut writer = FrameWriter::new(sink.clone());
        writer.begin_frame();
        writer.end_frame().unwrap();

        assert_eq!(sink.0.lock().unwrap().len(), 0);
    }
}
