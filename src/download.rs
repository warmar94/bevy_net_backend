//! Downloads streamed to a file: [`HttpDownload`] (what to write and what to check),
//! [`DownloadedFile`] (what was written), and the messages [`HttpDownloadResponse`] and
//! [`HttpDownloadProgress`].
//!
//! The answer body is written to a part file next to the target as it arrives (never held in
//! memory as a whole), checked (size limit, expected size, expected SHA-256, the server's
//! `Content-Length`), synced to disk and renamed over the target. Any failure, a cancel or a
//! timeout removes the part file. On the app's exit the built-in transport gives running downloads
//! up to 1 s to stop and remove their part files; a transfer still waiting for the server after
//! that (a stalled connection) can leave its part file when the process ends. Such leftovers of an
//! earlier run are removed by the next download to the same target.

use std::fmt;
#[cfg(feature = "http")]
use std::fs::{self, File};
#[cfg(feature = "http")]
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
#[cfg(feature = "http")]
use std::time::{Duration, Instant};

use bevy_ecs::message::Message;
use http::header::{HeaderMap, HeaderName};
use http::StatusCode;

use crate::request::RequestId;
use crate::response::BackendError;

/// The default largest file a download writes: 256 MiB ([`HttpDownload::with_max_bytes`]).
pub const DEFAULT_DOWNLOAD_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Download progress is reported at most this often per request (plus once at the end).
#[cfg(feature = "http")]
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// The piece size the body is read and written in.
#[cfg(feature = "http")]
const CHUNK: usize = 64 * 1024;

/// The file name of `path` only (errors and `Debug` never show the rest of a local path).
fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The part file next to `local` that this crate's downloads (HTTP and SFTP) write first:
/// `<name>.<process>-<request>-<n>.part`, a name no other download uses.
#[cfg(any(feature = "http", feature = "sftp"))]
pub(crate) fn part_path(local: &Path, id: RequestId) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut name = local.file_name().map(std::ffi::OsStr::to_os_string).unwrap_or_default();
    name.push(format!(".{}-{}-{}.part", std::process::id(), id.to_string().trim_start_matches('#'), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    local.with_file_name(name)
}

/// Whether `entry` (a file name in the target's folder) is a part file of `target` (a file name)
/// written by ANOTHER process: exactly `<target>.<process>-<request>-<n>.part` with three decimal
/// numbers and a process number other than `own`.
#[cfg(any(feature = "http", feature = "sftp"))]
fn is_stale_part(entry: &str, target: &str, own: u32) -> bool {
    let Some(numbers) = entry.strip_prefix(target).and_then(|rest| rest.strip_prefix('.')).and_then(|rest| rest.strip_suffix(".part")) else {
        return false;
    };
    let parts: Vec<&str> = numbers.split('-').collect();
    let all_numbers = parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.len() <= 20 && p.bytes().all(|b| b.is_ascii_digit()));
    all_numbers && parts.first().and_then(|pid| pid.parse::<u32>().ok()).is_some_and(|pid| pid != own)
}

/// Remove the part files of `local` that earlier runs left (a process that ended while one of its
/// downloads was still running). Only names of this crate's exact pattern with another process
/// number; part files of this process (downloads running now) and every other file stay. Best
/// effort: a file that cannot be removed or listed is left.
#[cfg(any(feature = "http", feature = "sftp"))]
pub(crate) fn remove_stale_parts(local: &Path) {
    let Some(target) = local.file_name().and_then(std::ffi::OsStr::to_str) else { return };
    let folder = match local.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let Ok(entries) = std::fs::read_dir(folder) else { return };
    let own = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_str().is_some_and(|name| is_stale_part(name, target, own)) && entry.file_type().is_ok_and(|t| t.is_file()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Where a download goes and what it is checked against
/// ([`HttpClient::download`](crate::HttpClient::download)).
///
/// The answer body (status 200–299) is written to a part file next to `path`
/// (`<name>.<process>-<request>-<n>.part`), then renamed over `path` (an existing file is
/// replaced) once every check passed. The part file is created before the request is sent: a
/// folder that does not exist or cannot be written, or a `path` that is a folder, is answered
/// `InvalidRequest`, never sent. Before that, part files of `path` that another process left
/// (same pattern, another process number: a run that ended during a download) are removed.
/// An answer outside 200–299 writes no file: it arrives as [`BackendError::Status`] with its body
/// in memory (capped by [`HttpConfig::with_max_body_bytes`](crate::HttpConfig::with_max_body_bytes)).
///
/// ```
/// use bevy_net_backend::HttpDownload;
///
/// let download = HttpDownload::to("downloads/level-3.pak")
///     .with_size(1_048_576)
///     .with_sha256("9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")
///     .with_max_bytes(64 * 1024 * 1024);
/// assert_eq!(download.size(), Some(1_048_576));
/// ```
///
/// Its `Debug` output shows the file name, not the whole path.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpDownload {
    path: PathBuf,
    sha256: Option<String>,
    size: Option<u64>,
    max_bytes: u64,
    progress: bool,
}

impl fmt::Debug for HttpDownload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpDownload")
            .field("file_name", &file_name(&self.path))
            .field("sha256", &self.sha256)
            .field("size", &self.size)
            .field("max_bytes", &self.max_bytes)
            .field("progress", &self.progress)
            .finish()
    }
}

impl HttpDownload {
    /// A download to the local file `path` (its folder must exist). Limit
    /// [`DEFAULT_DOWNLOAD_MAX_BYTES`], progress on.
    pub fn to(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), sha256: None, size: None, max_bytes: DEFAULT_DOWNLOAD_MAX_BYTES, progress: true }
    }

    /// The SHA-256 the file must have, as 64 hex digits (any case). A file with another hash is
    /// not put in place: the answer is a [`BackendError::Network`] naming the mismatch. Anything
    /// but 64 hex digits is answered `InvalidRequest`, never sent.
    pub fn with_sha256(mut self, hex: impl Into<String>) -> Self {
        self.sha256 = Some(hex.into().to_ascii_lowercase());
        self
    }

    /// The size in bytes the file must have. A server `Content-Length` that differs is refused
    /// before anything is written; a body that ends with another size is not put in place.
    pub fn with_size(mut self, bytes: u64) -> Self {
        self.size = Some(bytes);
        self
    }

    /// The largest file accepted (default [`DEFAULT_DOWNLOAD_MAX_BYTES`]). A larger
    /// `Content-Length` is refused before anything is written, a body that grows past it is cut
    /// there; both are [`BackendError::BodyTooLarge`].
    pub fn with_max_bytes(mut self, bytes: u64) -> Self {
        self.max_bytes = bytes;
        self
    }

    /// Report the download as [`HttpDownloadProgress`] messages (default on). The built-in
    /// transport reports at most about 10 per second per request, plus one at the end.
    pub fn with_progress(mut self, report: bool) -> Self {
        self.progress = report;
        self
    }

    /// The local file the answer is written to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The expected SHA-256 (lowercase hex), if set.
    pub fn sha256(&self) -> Option<&str> {
        self.sha256.as_deref()
    }

    /// The expected size, if set.
    pub fn size(&self) -> Option<u64> {
        self.size
    }

    /// The largest file accepted.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Whether the download is reported as [`HttpDownloadProgress`].
    pub fn progress(&self) -> bool {
        self.progress
    }

    /// The settings' own problems (before anything is sent).
    pub(crate) fn check(&self) -> Result<(), BackendError> {
        let invalid = |why: &str| Err(BackendError::InvalidRequest(format!("download: {why}")));
        if self.path.file_name().is_none() {
            return invalid("the local path has no file name");
        }
        if let Some(sha) = &self.sha256 {
            if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                return invalid("the expected SHA-256 must be 64 hex digits");
            }
        }
        if self.size.is_some_and(|size| size > self.max_bytes) {
            return invalid("the expected size is larger than the size limit");
        }
        Ok(())
    }

    /// Write `body` (the answer body of request `id`) to the file with every check of these
    /// settings: the part file, the limits, the expected size and SHA-256, the rename. For a
    /// custom [`HttpTransport`](crate::HttpTransport) that answers downloads
    /// ([`downloads_to_files`](crate::HttpTransport::downloads_to_files)): put the result into
    /// [`RawResponse::file`](crate::RawResponse::file). Blocking file I/O: call it on the
    /// transport's own thread. `content_length` is the server's `Content-Length` of the body as
    /// `body` gives it (`None` when unknown or when the body is decoded, e.g. gzip). `progress` is
    /// called with `(bytes written, total)` (throttled) when [`progress`](Self::progress) is on.
    /// A read error of `body` is a [`BackendError::Network`].
    #[cfg(feature = "http")]
    #[cfg_attr(docsrs, doc(cfg(feature = "http")))]
    pub fn receive(
        &self,
        id: RequestId,
        body: &mut dyn Read,
        content_length: Option<u64>,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<DownloadedFile, BackendError> {
        let part = PartFile::create(self, id)?;
        part.write_from(body, content_length, progress, &|| false, &|e| BackendError::Network(format!("reading the download failed: {e}")))?.put_in_place()
    }
}

/// A file a download wrote ([`HttpDownloadResponse`]): where, how big, its SHA-256, and the
/// server's status and headers.
///
/// `Debug` shows the file name and the header names, not the whole path or header values.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DownloadedFile {
    /// The local file (the download's target; the part file was renamed to it).
    pub path: PathBuf,
    /// Its size in bytes.
    pub bytes: u64,
    /// Its SHA-256, 64 lowercase hex digits (compare it with a hash the server sent, e.g. in an
    /// `ETag`).
    pub sha256: String,
    /// The HTTP status (200–299).
    pub status: StatusCode,
    /// The response headers.
    pub headers: HeaderMap,
}

impl fmt::Debug for DownloadedFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<&str> = self.headers.keys().map(HeaderName::as_str).collect();
        f.debug_struct("DownloadedFile")
            .field("file_name", &file_name(&self.path))
            .field("bytes", &self.bytes)
            .field("sha256", &self.sha256)
            .field("status", &self.status)
            .field("header_names", &headers)
            .finish()
    }
}

/// The answer to a download ([`HttpClient::download`](crate::HttpClient::download),
/// [`download_to`](crate::HttpClient::download_to)): the file written, or why not. On an error
/// no file is left (neither the part file nor a partial target); an existing target file is
/// only replaced on success.
///
/// Written in `First` ([`BackendSystems::Receive`](crate::BackendSystems::Receive)) of the frame
/// the answer arrived, so `PreUpdate` and `Update` read it that frame.
#[derive(Message, Clone, Debug)]
#[non_exhaustive]
pub struct HttpDownloadResponse {
    /// The request this answers.
    pub id: RequestId,
    /// The file, or the error.
    pub result: Result<DownloadedFile, BackendError>,
}

/// Download progress: the bytes of the answer body written to the part file so far. Only for
/// downloads with [`HttpDownload::with_progress`] on (the default). The built-in transport writes
/// at most about 10 per second per request, plus one when the body is complete; all of them before
/// the [`HttpDownloadResponse`].
#[derive(Message, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct HttpDownloadProgress {
    /// The request.
    pub id: RequestId,
    /// Bytes written so far.
    pub received: u64,
    /// The body's size, when the server sent a `Content-Length` (and the body is not decoded).
    pub total: Option<u64>,
}

/// A part file being written: removed on drop unless it was put in place.
#[cfg(feature = "http")]
pub(crate) struct PartFile {
    file: Option<File>,
    part: PathBuf,
    settings: HttpDownload,
    done: bool,
}

#[cfg(feature = "http")]
impl Drop for PartFile {
    fn drop(&mut self) {
        self.file = None;
        if !self.done {
            let _ = fs::remove_file(&self.part);
        }
    }
}

#[cfg(feature = "http")]
fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX.get(usize::from(b >> 4)).copied().map_or('0', char::from));
        out.push(HEX.get(usize::from(b & 0x0f)).copied().map_or('0', char::from));
    }
    out
}

#[cfg(feature = "http")]
impl PartFile {
    /// Check the settings and create the part file next to the target, before anything is sent
    /// (stale part files of the target that other processes left are removed first).
    pub(crate) fn create(settings: &HttpDownload, id: RequestId) -> Result<Self, BackendError> {
        settings.check()?;
        if settings.path.is_dir() {
            return Err(BackendError::InvalidRequest(format!("download: `{}` is a folder, not a file", file_name(&settings.path))));
        }
        remove_stale_parts(&settings.path);
        let part = part_path(&settings.path, id);
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(&part)
            .map_err(|e| BackendError::InvalidRequest(format!("download: the part file for `{}` cannot be created: {e}", file_name(&settings.path))))?;
        Ok(Self { file: Some(file), part, settings: settings.clone(), done: false })
    }

    /// Copy `body` into the part file with every check and sync it to disk; the result is put in
    /// place with [`ReadyFile::put_in_place`] (dropping it removes the part file). `stop` is asked
    /// before each piece (cancel, shutdown); `read_error` maps a read error of the body.
    pub(crate) fn write_from(
        mut self,
        body: &mut dyn Read,
        content_length: Option<u64>,
        progress: &mut dyn FnMut(u64, Option<u64>),
        stop: &dyn Fn() -> bool,
        read_error: &dyn Fn(io::Error) -> BackendError,
    ) -> Result<ReadyFile, BackendError> {
        let limit = self.settings.max_bytes;
        if let Some(length) = content_length {
            if length > limit {
                return Err(BackendError::BodyTooLarge { limit });
            }
            if let Some(size) = self.settings.size.filter(|size| *size != length) {
                return Err(BackendError::Network(format!("download: the server announced {length} bytes, the expected size is {size}")));
            }
        }
        let report = self.settings.progress;
        let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
        let mut buffer = vec![0u8; CHUNK];
        let mut written: u64 = 0;
        let mut last: Option<Instant> = None;
        let Some(file) = self.file.as_mut() else {
            return Err(BackendError::Network("download: the part file is closed".into()));
        };
        let write_failed = |e: io::Error| BackendError::Network(format!("download: writing the file failed: {e}"));
        loop {
            if stop() {
                return Err(BackendError::Cancelled);
            }
            let n = match body.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(read_error(e)),
            };
            let piece = buffer.get(..n).unwrap_or_default();
            written = written.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
            if written > limit {
                return Err(BackendError::BodyTooLarge { limit });
            }
            hasher.update(piece);
            file.write_all(piece).map_err(write_failed)?;
            if report {
                let now = Instant::now();
                if last.is_none_or(|at| now.saturating_duration_since(at) >= PROGRESS_EVERY) {
                    last = Some(now);
                    progress(written, content_length);
                }
            }
        }
        if let Some(length) = content_length.filter(|length| *length != written) {
            return Err(BackendError::Network(format!("download: the body ended after {written} of {length} bytes")));
        }
        if let Some(size) = self.settings.size.filter(|size| *size != written) {
            return Err(BackendError::Network(format!("download: the file has {written} bytes, the expected size is {size}")));
        }
        let sha256 = hex(hasher.finish().as_ref());
        if self.settings.sha256.as_deref().is_some_and(|expected| expected != sha256) {
            return Err(BackendError::Network("download: the file does not match the expected SHA-256".into()));
        }
        file.flush().map_err(write_failed)?;
        file.sync_all().map_err(write_failed)?;
        if stop() {
            return Err(BackendError::Cancelled);
        }
        if report {
            progress(written, content_length);
        }
        self.file = None;
        Ok(ReadyFile { part: self, bytes: written, sha256 })
    }
}

/// A complete, checked part file synced to disk, not yet renamed over the target. Dropping it
/// removes the part file. The built-in transport renames it only while it holds the lock its
/// cancel uses, so a cancel, timeout or shutdown answer and the rename never both happen.
#[cfg(feature = "http")]
pub(crate) struct ReadyFile {
    part: PartFile,
    bytes: u64,
    sha256: String,
}

#[cfg(feature = "http")]
impl ReadyFile {
    /// Rename the part file over the target. The folder is synced afterwards with
    /// [`ReadyFile::sync_folder`] (outside any lock).
    pub(crate) fn rename(mut self) -> Result<DownloadedFile, BackendError> {
        let target = self.part.settings.path.clone();
        fs::rename(&self.part.part, &target)
            .map_err(|e| BackendError::Network(format!("download: the file could not be put in place as `{}`: {e}", file_name(&target))))?;
        self.part.done = true;
        Ok(DownloadedFile { path: target, bytes: self.bytes, sha256: std::mem::take(&mut self.sha256), status: StatusCode::OK, headers: HeaderMap::new() })
    }

    /// Make the rename of `file` durable (Unix: sync its folder; best effort).
    pub(crate) fn sync_folder(file: &DownloadedFile) {
        let folder = match file.path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        crate::secret_file::sync_dir(folder);
    }

    /// Rename and sync the folder (no other party to agree with).
    pub(crate) fn put_in_place(self) -> Result<DownloadedFile, BackendError> {
        let file = self.rename()?;
        Self::sync_folder(&file);
        Ok(file)
    }
}

#[cfg(all(test, feature = "http"))]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bnb-download-unit-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        dir
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir).map(|it| it.filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default()
    }

    #[test]
    fn checks_and_part_files() {
        let dir = dir("checks");
        let target = dir.join("file.bin");
        let data = vec![7u8; 200_000];
        let sha = hex(ring::digest::digest(&ring::digest::SHA256, &data).as_ref());
        let id = RequestId::next();
        let mut reports = Vec::new();
        let ok = HttpDownload::to(&target).with_sha256(sha.to_uppercase()).with_size(200_000).receive(id, &mut &data[..], Some(200_000), &mut |r, t| {
            reports.push((r, t));
        });
        let file = ok.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((file.bytes, file.sha256.as_str()), (200_000, sha.as_str()));
        assert_eq!(reports.last(), Some(&(200_000, Some(200_000))));
        assert_eq!(fs::read(&target).map(|b| b.len()).unwrap_or(0), 200_000);
        assert_eq!(leftovers(&dir), vec!["file.bin".to_string()]);

        let wrong = "0".repeat(64);
        let cases: Vec<(HttpDownload, Option<u64>)> = vec![
            (HttpDownload::to(&target).with_sha256(&wrong), None),
            (HttpDownload::to(&target).with_size(5), None),
            (HttpDownload::to(&target).with_size(5), Some(200_000)),
            (HttpDownload::to(&target).with_max_bytes(1000), None),
            (HttpDownload::to(&target).with_max_bytes(1000), Some(200_000)),
            (HttpDownload::to(&target), Some(300_000)),
            (HttpDownload::to(&target).with_sha256("abc"), None),
            (HttpDownload::to(&target).with_size(2000).with_max_bytes(1000), None),
        ];
        for (settings, length) in cases {
            let other = vec![1u8; 200_000];
            let result = settings.receive(RequestId::next(), &mut &other[..], length, &mut |_, _| {});
            assert!(result.is_err(), "{settings:?} {length:?}");
            // The target keeps the first download; no part file is left.
            assert_eq!(fs::read(&target).ok(), Some(data.clone()), "{settings:?}");
            assert_eq!(leftovers(&dir), vec!["file.bin".to_string()], "{settings:?}");
        }
        assert!(matches!(
            HttpDownload::to(&target).with_max_bytes(1000).receive(RequestId::next(), &mut &data[..], None, &mut |_, _| {}),
            Err(BackendError::BodyTooLarge { limit: 1000 })
        ));
        assert!(matches!(
            HttpDownload::to(dir.join("missing").join("x.bin")).receive(RequestId::next(), &mut &data[..], None, &mut |_, _| {}),
            Err(BackendError::InvalidRequest(_))
        ));
        assert!(matches!(HttpDownload::to("").check(), Err(BackendError::InvalidRequest(_))));
        let debug = format!("{:?}", HttpDownload::to(dir.join("secret-folder").join("x.bin")));
        assert!(debug.contains("x.bin") && !debug.contains("secret-folder"), "{debug}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stop_or_a_read_error_leaves_nothing() {
        let dir = dir("stop");
        let target = dir.join("file.bin");
        let data = vec![3u8; 500_000];
        let part = PartFile::create(&HttpDownload::to(&target), RequestId::next()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(leftovers(&dir).len(), 1, "the part file exists before anything is read");
        let result = part.write_from(&mut &data[..], None, &mut |_, _| {}, &|| true, &|e| BackendError::Network(e.to_string()));
        assert!(matches!(result, Err(BackendError::Cancelled)));
        assert!(leftovers(&dir).is_empty());

        struct Broken(usize);
        impl Read for Broken {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0 == 0 {
                    return Err(io::Error::new(io::ErrorKind::ConnectionReset, "reset"));
                }
                self.0 -= 1;
                let n = buf.len().min(1000);
                Ok(n)
            }
        }
        let result = HttpDownload::to(&target).receive(RequestId::next(), &mut Broken(3), None, &mut |_, _| {});
        assert!(matches!(result, Err(BackendError::Network(ref why)) if why.contains("reset")), "{result:?}");
        let result = HttpDownload::to(&target).receive(RequestId::next(), &mut Broken(3), Some(10_000), &mut |_, _| {});
        assert!(result.is_err());
        assert!(leftovers(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_part_files_of_other_runs_are_removed_and_nothing_else() {
        let own = std::process::id();
        let other = own.wrapping_add(1);
        assert!(is_stale_part(&format!("a.bin.{other}-12-0.part"), "a.bin", own));
        for name in [
            format!("a.bin.{own}-12-0.part"),
            format!("a.bin.{other}-12.part"),
            format!("a.bin.{other}-12-0-1.part"),
            format!("a.bin.{other}-x-0.part"),
            format!("a.bin.{other}--0.part"),
            format!("a.bin.{other}-12-0.part.old"),
            format!("b.bin.{other}-12-0.part"),
            format!("a.bin{other}-12-0.part"),
            "a.bin".to_string(),
            "a.bin.part".to_string(),
            format!("xa.bin.{other}-1-0.part"),
        ] {
            assert!(!is_stale_part(&name, "a.bin", own), "{name}");
        }

        let dir = dir("stale");
        let target = dir.join("a.bin");
        let stale = format!("a.bin.{other}-3-7.part");
        let keep = [format!("a.bin.{own}-3-7.part"), "a.bin.notes.part".to_string(), "b.bin.1-2-3.part".to_string()];
        for name in keep.iter().chain([&stale]) {
            fs::write(dir.join(name), b"x").unwrap_or_else(|e| panic!("{e}"));
        }
        fs::create_dir(dir.join(format!("a.bin.{other}-9-9.part"))).unwrap_or_else(|e| panic!("{e}"));
        let data = [1u8; 10];
        HttpDownload::to(&target).receive(RequestId::next(), &mut &data[..], None, &mut |_, _| {}).unwrap_or_else(|e| panic!("{e}"));
        let mut left = leftovers(&dir);
        left.sort();
        let mut expected: Vec<String> = keep.to_vec();
        expected.push("a.bin".into());
        expected.push(format!("a.bin.{other}-9-9.part"));
        expected.sort();
        assert_eq!(left, expected, "only the other run's part file is gone (a folder of that name stays)");

        // A target that is a folder: refused before anything is read or created.
        let folder = dir.join("folder");
        fs::create_dir(&folder).unwrap_or_else(|e| panic!("{e}"));
        let result = HttpDownload::to(&folder).receive(RequestId::next(), &mut &data[..], None, &mut |_, _| {});
        assert!(matches!(&result, Err(BackendError::InvalidRequest(why)) if why.contains("folder")), "{result:?}");
        assert_eq!(leftovers(&folder), Vec::<String>::new());
        let _ = fs::remove_dir_all(&dir);
    }
}
