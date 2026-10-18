//! Input and output endpoints: files, standard streams and memory buffers.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Cursor, Read, Seek, SeekFrom, Write};

use crate::error::{Error, Result};

/// True for the names that mean a standard stream rather than a file.
pub fn is_pipe(path: &str) -> bool {
    path == "-" || path.starts_with("pipe:")
}

/// Opens a file, or standard input for "-" and "pipe:".
pub fn open_input(path: &str) -> Result<Box<dyn Read>> {
    if is_pipe(path) {
        return Ok(Box::new(BufReader::new(io::stdin())));
    }
    let file = File::open(path)
        .map_err(|e| Error::Io(io::Error::new(e.kind(), format!("{path}: {e}"))))?;
    Ok(Box::new(BufReader::with_capacity(1 << 16, file)))
}

/// Reads up to `len` bytes for format probing and returns them together
/// with a reader that still yields the whole stream from the start, so
/// probing works on pipes that cannot seek back.
pub fn peek_prefix(mut reader: Box<dyn Read>, len: usize) -> Result<(Vec<u8>, Box<dyn Read>)> {
    let mut prefix = Vec::with_capacity(len);
    (&mut reader).take(len as u64).read_to_end(&mut prefix)?;
    let replay = Cursor::new(prefix.clone()).chain(reader);
    Ok((prefix, Box::new(replay)))
}

/// Where a muxer writes. Seeking is only available on files and memory,
/// so muxers that patch their header must check [`Output::is_seekable`].
pub struct Output {
    inner: OutputInner,
    written: u64,
}

enum OutputInner {
    File(BufWriter<File>),
    Stdout(BufWriter<io::Stdout>),
    Memory(Cursor<Vec<u8>>),
}

impl Output {
    /// Creates or truncates a file, or uses standard output for "-" and "pipe:".
    pub fn create(path: &str) -> Result<Self> {
        let inner = if is_pipe(path) {
            OutputInner::Stdout(BufWriter::new(io::stdout()))
        } else {
            let file = File::create(path)
                .map_err(|e| Error::Io(io::Error::new(e.kind(), format!("{path}: {e}"))))?;
            OutputInner::File(BufWriter::with_capacity(1 << 16, file))
        };
        Ok(Output { inner, written: 0 })
    }

    /// An in-memory output, mostly for tests.
    pub fn memory() -> Self {
        Output {
            inner: OutputInner::Memory(Cursor::new(Vec::new())),
            written: 0,
        }
    }

    pub fn is_seekable(&self) -> bool {
        !matches!(self.inner, OutputInner::Stdout(_))
    }

    /// Bytes written so far, counting rewrites after a seek. Used for the
    /// progress line, where an approximation is fine.
    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    /// Returns the buffer of a memory output, `None` for other kinds.
    pub fn into_bytes(self) -> Option<Vec<u8>> {
        match self.inner {
            OutputInner::Memory(c) => Some(c.into_inner()),
            _ => None,
        }
    }
}

impl Write for Output {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = match &mut self.inner {
            OutputInner::File(w) => w.write(buf)?,
            OutputInner::Stdout(w) => w.write(buf)?,
            OutputInner::Memory(w) => w.write(buf)?,
        };
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.inner {
            OutputInner::File(w) => w.flush(),
            OutputInner::Stdout(w) => w.flush(),
            OutputInner::Memory(w) => w.flush(),
        }
    }
}

impl Seek for Output {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match &mut self.inner {
            OutputInner::File(w) => w.seek(pos),
            OutputInner::Memory(w) => w.seek(pos),
            OutputInner::Stdout(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "cannot seek on standard output",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peek_keeps_the_whole_stream() {
        let data: Vec<u8> = (0..100).collect();
        let reader: Box<dyn Read> = Box::new(Cursor::new(data.clone()));
        let (prefix, mut rest) = peek_prefix(reader, 10).unwrap();
        assert_eq!(prefix, &data[..10]);
        let mut all = Vec::new();
        rest.read_to_end(&mut all).unwrap();
        assert_eq!(all, data);
    }

    #[test]
    fn memory_output_seeks_and_counts() {
        let mut out = Output::memory();
        out.write_all(b"abcd").unwrap();
        out.seek(SeekFrom::Start(1)).unwrap();
        out.write_all(b"X").unwrap();
        assert_eq!(out.bytes_written(), 5);
        assert_eq!(out.into_bytes().unwrap(), b"aXcd");
    }
}
