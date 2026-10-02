//! [`StreamingBody`]: a request body that is read while it is sent (a multipart form with file
//! parts read from disk), and its reader.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::response::BackendError;

/// One piece of a streamed body.
#[derive(Clone)]
#[cfg_attr(not(feature = "http"), allow(dead_code))]
pub(crate) enum Segment {
    /// Bytes already in memory (boundaries, part headers, in-memory parts).
    Bytes(Vec<u8>),
    /// A local file, read when the body is sent. `field` names the form field in errors.
    File { path: PathBuf, field: String },
}

/// A request body that is read while it is sent: a `multipart/form-data` form with file parts
/// read from disk (`Multipart::file_from_path`, feature `http`). The files are opened, measured
/// and read on the transport's thread when the request goes out, never on the thread that made
/// the request, and never loaded into memory as a whole.
///
/// A transport sends it with its exact length (`Content-Length`) from
/// [`open`](Self::open). `Debug` shows the number of pieces and files, never paths or bytes.
#[derive(Clone)]
pub struct StreamingBody {
    segments: Arc<[Segment]>,
    max_bytes: u64,
}

impl fmt::Debug for StreamingBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let files = self.segments.iter().filter(|s| matches!(s, Segment::File { .. })).count();
        f.debug_struct("StreamingBody").field("pieces", &self.segments.len()).field("files", &files).field("max_bytes", &self.max_bytes).finish()
    }
}

/// The error a [`StreamingReader`] reports through `std::io` when a local file changed while it
/// was sent; the HTTP transport turns it back into its text.
#[derive(Debug)]
pub(crate) struct LocalFileChanged(pub(crate) String);

impl fmt::Display for LocalFileChanged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LocalFileChanged {}

/// The file name of `path` only (errors never show the rest of a local path).
fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

impl StreamingBody {
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    pub(crate) fn new(segments: Vec<Segment>, max_bytes: u64) -> Self {
        Self { segments: segments.into(), max_bytes }
    }

    /// Open every file and measure the whole body: its exact length and a reader. Blocking file
    /// I/O: call it on a transport's own thread. A file that cannot be opened is
    /// `InvalidRequest`, a body over the form's limit `RequestTooLarge`; both before anything is
    /// sent.
    pub fn open(&self) -> Result<(u64, StreamingReader), BackendError> {
        let mut pieces = Vec::with_capacity(self.segments.len());
        let mut total: u64 = 0;
        for segment in self.segments.iter() {
            match segment {
                Segment::Bytes(bytes) => {
                    total = total.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
                    pieces.push(Piece::Bytes { bytes: bytes.clone(), at: 0 });
                }
                Segment::File { path, field } => {
                    let open = |what: &str, e: io::Error| {
                        BackendError::InvalidRequest(format!("{what} the local file `{}` of multipart field `{field}`: {e}", file_name(path)))
                    };
                    let file = File::open(path).map_err(|e| open("could not open", e))?;
                    let metadata = file.metadata().map_err(|e| open("could not read", e))?;
                    if !metadata.is_file() {
                        return Err(BackendError::InvalidRequest(format!("`{}` of multipart field `{field}` is not a file", file_name(path))));
                    }
                    let size = metadata.len();
                    total = total.saturating_add(size);
                    pieces.push(Piece::File { file, left: size, size, name: file_name(path), field: field.clone() });
                }
            }
        }
        if total > self.max_bytes {
            return Err(BackendError::RequestTooLarge { limit: self.max_bytes, size: total });
        }
        Ok((total, StreamingReader { pieces, index: 0 }))
    }

    /// The whole body in memory (opens and reads every file): for tests and for transports that
    /// cannot stream. Same errors as [`open`](Self::open), plus a file that changed size.
    pub fn read_all(&self) -> Result<Vec<u8>, BackendError> {
        let (len, mut reader) = self.open()?;
        let mut body = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
        reader.read_to_end(&mut body).map_err(|e| BackendError::InvalidRequest(e.to_string()))?;
        Ok(body)
    }

    /// The form's size limit.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }
}

enum Piece {
    Bytes { bytes: Vec<u8>, at: usize },
    File { file: File, left: u64, size: u64, name: String, field: String },
}

/// Reads a [`StreamingBody`] from its start: exactly the length [`StreamingBody::open`]
/// measured. A file that is shorter or longer by the time it is read ends the reading with an
/// error (a body with the measured length cannot hold it), never with a silently different body.
pub struct StreamingReader {
    pieces: Vec<Piece>,
    index: usize,
}

impl fmt::Debug for StreamingReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamingReader").field("pieces", &self.pieces.len()).field("at_piece", &self.index).finish()
    }
}

impl Read for StreamingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let Some(piece) = self.pieces.get_mut(self.index) else { return Ok(0) };
            match piece {
                Piece::Bytes { bytes, at } => {
                    let rest = bytes.get(*at..).unwrap_or_default();
                    if rest.is_empty() {
                        self.index += 1;
                        continue;
                    }
                    let n = rest.len().min(buf.len());
                    if let (Some(to), Some(from)) = (buf.get_mut(..n), rest.get(..n)) {
                        to.copy_from_slice(from);
                    }
                    *at += n;
                    return Ok(n);
                }
                Piece::File { file, left, size, name, field } => {
                    if *left == 0 {
                        // The measured size is sent: a file that grew since does not fit.
                        let mut probe = [0u8; 1];
                        match file.read(&mut probe) {
                            Ok(0) => {}
                            Ok(_) => return Err(changed(name, field, *size)),
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(e) => return Err(e),
                        }
                        self.index += 1;
                        continue;
                    }
                    let want = usize::try_from(*left).unwrap_or(usize::MAX).min(buf.len());
                    let Some(to) = buf.get_mut(..want) else { return Ok(0) };
                    match file.read(to) {
                        Ok(0) => return Err(changed(name, field, *size)),
                        Ok(n) => {
                            *left = left.saturating_sub(u64::try_from(n).unwrap_or(u64::MAX));
                            return Ok(n);
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e),
                    }
                }
            }
        }
    }
}

fn changed(name: &str, field: &str, size: u64) -> io::Error {
    io::Error::other(LocalFileChanged(format!(
        "the local file `{name}` of multipart field `{field}` changed size while it was sent (it had {size} bytes); the request was cut off"
    )))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::{LocalFileChanged, Segment, StreamingBody};
    use crate::response::BackendError;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("tmp").join(format!("body-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        dir.join(name)
    }

    fn body(path: &std::path::Path, max: u64) -> StreamingBody {
        StreamingBody::new(
            vec![Segment::Bytes(b"head-".to_vec()), Segment::File { path: path.to_path_buf(), field: "f".into() }, Segment::Bytes(b"-tail".to_vec())],
            max,
        )
    }

    #[test]
    fn the_reader_gives_exactly_the_measured_bytes_in_small_reads() {
        let path = scratch("exact.bin");
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap_or_else(|e| panic!("{e}"));
        let (len, mut reader) = body(&path, u64::MAX).open().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(len, 200_010);
        let mut out = Vec::new();
        let mut buf = [0u8; 7]; // odd, small reads across every piece edge
        loop {
            let n = reader.read(&mut buf).unwrap_or_else(|e| panic!("{e}"));
            if n == 0 {
                break;
            }
            assert!(n <= buf.len());
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(&out[..5], b"head-");
        assert_eq!(&out[5..200_005], &data[..]);
        assert_eq!(&out[200_005..], b"-tail");
        assert_eq!(body(&path, u64::MAX).read_all().ok(), Some(out));
    }

    #[test]
    fn a_missing_file_or_an_over_limit_body_is_refused_before_sending() {
        let missing = scratch("missing.bin");
        let _ = std::fs::remove_file(&missing);
        let error = body(&missing, u64::MAX).open().err();
        assert!(matches!(&error, Some(BackendError::InvalidRequest(why)) if why.contains("missing.bin") && why.contains("`f`")), "{error:?}");
        assert_eq!(error.and_then(|e| e.was_sent()), Some(false));
        // Only the file name, never the folders around it.
        let why = body(&missing, u64::MAX).open().err().map(|e| e.to_string()).unwrap_or_default();
        assert!(!why.contains("target"), "{why}");
        let path = scratch("limit.bin");
        std::fs::write(&path, vec![0u8; 1000]).unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(body(&path, 1009).open().err(), Some(BackendError::RequestTooLarge { limit: 1009, size: 1010 })));
        assert!(body(&path, 1010).open().is_ok());
        let dir = scratch("a-dir");
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(body(&dir, u64::MAX).open().err(), Some(BackendError::InvalidRequest(_))));
    }

    #[test]
    fn a_file_that_shrinks_or_grows_while_it_is_sent_is_an_error() {
        for grow in [false, true] {
            let path = scratch(if grow { "grow.bin" } else { "shrink.bin" });
            std::fs::write(&path, vec![1u8; 10_000]).unwrap_or_else(|e| panic!("{e}"));
            let (_, mut reader) = body(&path, u64::MAX).open().unwrap_or_else(|e| panic!("{e}"));
            if grow {
                std::fs::OpenOptions::new().append(true).open(&path).and_then(|mut f| f.write_all(&[2u8; 10])).unwrap_or_else(|e| panic!("{e}"));
            } else {
                std::fs::OpenOptions::new().write(true).open(&path).and_then(|f| f.set_len(4_000)).unwrap_or_else(|e| panic!("{e}"));
            }
            let mut out = Vec::new();
            let error = reader.read_to_end(&mut out).err().unwrap_or_else(|| panic!("grow {grow}: no error"));
            let inner = error.get_ref().and_then(|e| e.downcast_ref::<LocalFileChanged>()).map(ToString::to_string);
            assert!(inner.as_deref().is_some_and(|why| why.contains("changed size") && why.contains("`f`")), "{error}");
        }
    }

    #[test]
    fn debug_shows_no_paths() {
        let debug = format!("{:?}", body(std::path::Path::new("secret-folder/private.bin"), 5));
        assert!(!debug.contains("private") && !debug.contains("secret"), "{debug}");
    }
}
