//! SFTP on SSH connections (feature `sftp`): the operations, their answers, the `SshClient`
//! methods, and the russh-sftp side that runs them on the SSH thread.

use std::fmt;
use std::path::PathBuf;

use bevy_ecs::message::Message;

use super::{file_name, SshClient, SshName};
use crate::request::RequestId;
use crate::response::BackendError;

/// The longest remote path accepted (bytes).
const MAX_REMOTE_PATH: usize = 4096;
/// The most entries a directory listing returns; a longer one is an error.
pub(crate) const MAX_LIST_ENTRIES: usize = 10_000;
/// The longest entry name accepted in a listing (bytes); a longer one is an error.
pub(crate) const MAX_ENTRY_NAME: usize = 4096;
/// The most name bytes one listing may hold in total; more is an error.
pub(crate) const MAX_LIST_NAME_BYTES: usize = 4 * 1024 * 1024;

/// One SFTP operation (what a transport receives; games use the [`SshClient`] methods). Remote
/// paths are the server's (relative paths start in the login directory). `Debug` shows local
/// paths by file name only and uploads by length only.
#[derive(Clone)]
#[non_exhaustive]
pub enum SftpOp {
    /// Write `data` to `remote` (created or truncated).
    UploadBytes {
        /// The remote file.
        remote: String,
        /// The bytes.
        data: Vec<u8>,
    },
    /// Copy the local file `local` to `remote` (created or truncated).
    UploadFile {
        /// The local file.
        local: PathBuf,
        /// The remote file.
        remote: String,
    },
    /// Read `remote` into memory ([`SftpOutcome::Data`]).
    Download {
        /// The remote file.
        remote: String,
    },
    /// Copy `remote` to the local file `local` (written as `<local>.part`, then renamed over
    /// `local`; the part file is removed on failure).
    DownloadFile {
        /// The remote file.
        remote: String,
        /// The local file.
        local: PathBuf,
    },
    /// List a directory ([`SftpOutcome::Listing`], without `.` and `..`).
    List {
        /// The remote directory.
        path: String,
    },
    /// Create a directory (its parent must exist).
    CreateDir {
        /// The remote directory.
        path: String,
    },
    /// Remove a file.
    RemoveFile {
        /// The remote file.
        path: String,
    },
    /// Remove an empty directory.
    RemoveDir {
        /// The remote directory.
        path: String,
    },
    /// Rename or move (the server decides whether an existing `to` is replaced; OpenSSH refuses).
    Rename {
        /// The old path.
        from: String,
        /// The new path.
        to: String,
    },
}

impl fmt::Debug for SftpOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SftpOp::UploadBytes { remote, data } => f.debug_struct("UploadBytes").field("remote", remote).field("bytes", &data.len()).finish(),
            SftpOp::UploadFile { local, remote } => f.debug_struct("UploadFile").field("local", &file_name(local)).field("remote", remote).finish(),
            SftpOp::Download { remote } => f.debug_struct("Download").field("remote", remote).finish(),
            SftpOp::DownloadFile { remote, local } => f.debug_struct("DownloadFile").field("remote", remote).field("local", &file_name(local)).finish(),
            SftpOp::List { path } => f.debug_struct("List").field("path", path).finish(),
            SftpOp::CreateDir { path } => f.debug_struct("CreateDir").field("path", path).finish(),
            SftpOp::RemoveFile { path } => f.debug_struct("RemoveFile").field("path", path).finish(),
            SftpOp::RemoveDir { path } => f.debug_struct("RemoveDir").field("path", path).finish(),
            SftpOp::Rename { from, to } => f.debug_struct("Rename").field("from", from).field("to", to).finish(),
        }
    }
}

fn check_remote(path: &str) -> Result<(), BackendError> {
    if path.is_empty() || path.len() > MAX_REMOTE_PATH || path.contains('\0') {
        return Err(BackendError::InvalidRequest(format!("a remote SFTP path must be 1..={MAX_REMOTE_PATH} bytes without NUL")));
    }
    Ok(())
}

impl SftpOp {
    /// Check the paths (non-empty, no NUL, at most 4096 bytes) and an in-memory upload against
    /// `max_bytes`.
    pub(crate) fn validate(&self, max_bytes: u64) -> Result<(), BackendError> {
        match self {
            SftpOp::UploadBytes { remote, data } => {
                check_remote(remote)?;
                // The same answer as an over-limit `upload_file`: refused before anything is sent.
                let size = u64::try_from(data.len()).unwrap_or(u64::MAX);
                if size > max_bytes {
                    return Err(BackendError::RequestTooLarge { limit: max_bytes, size });
                }
                Ok(())
            }
            SftpOp::UploadFile { local, remote } | SftpOp::DownloadFile { remote, local } => {
                if local.as_os_str().is_empty() {
                    return Err(BackendError::InvalidRequest("the local path is empty".into()));
                }
                check_remote(remote)
            }
            SftpOp::Download { remote } => check_remote(remote),
            SftpOp::List { path } | SftpOp::CreateDir { path } | SftpOp::RemoveFile { path } | SftpOp::RemoveDir { path } => check_remote(path),
            SftpOp::Rename { from, to } => check_remote(from).and_then(|()| check_remote(to)),
        }
    }
}

/// What a finished SFTP operation produced. `#[non_exhaustive]`; `Debug` shows downloaded data by
/// length only.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SftpOutcome {
    /// An upload wrote this many bytes.
    Uploaded {
        /// Bytes written.
        bytes: u64,
    },
    /// A download to a local file wrote this many bytes.
    Downloaded {
        /// Bytes written.
        bytes: u64,
    },
    /// A download into memory.
    Data(Vec<u8>),
    /// A directory listing, sorted by name.
    Listing(Vec<SftpEntry>),
    /// Done (create / remove / rename).
    Done,
}

impl fmt::Debug for SftpOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SftpOutcome::Uploaded { bytes } => f.debug_struct("Uploaded").field("bytes", bytes).finish(),
            SftpOutcome::Downloaded { bytes } => f.debug_struct("Downloaded").field("bytes", bytes).finish(),
            SftpOutcome::Data(data) => write!(f, "Data({} bytes)", data.len()),
            SftpOutcome::Listing(entries) => f.debug_tuple("Listing").field(entries).finish(),
            SftpOutcome::Done => f.write_str("Done"),
        }
    }
}

/// The kind of a directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SftpEntryKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link.
    Symlink,
    /// Something else, or unknown.
    Other,
}

/// One entry of a directory listing. `#[non_exhaustive]`: build one with [`new`](Self::new).
///
/// **`name` is untrusted input from the server.** A hostile or broken server can list
/// `../../.bashrc`, `C:\Windows\evil.dll`, `a/b` or names with control characters. Never join
/// `name` into a local path yourself: use [`safe_file_name`](Self::safe_file_name), which returns
/// `None` for anything that is not one plain file name. The crate itself never turns a listed name
/// into a local path.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SftpEntry {
    /// The file name as the server sent it (not the full path; UNTRUSTED, see above).
    pub name: String,
    /// File, directory, link, …
    pub kind: SftpEntryKind,
    /// The size in bytes, when the server sent it.
    pub size: Option<u64>,
    /// The modification time (Unix seconds), when the server sent it.
    pub modified: Option<u32>,
    /// The Unix permission bits, when the server sent them.
    pub permissions: Option<u32>,
}

impl SftpEntry {
    /// An entry with a name and kind and nothing else (for custom transports and tests).
    pub fn new(name: impl Into<String>, kind: SftpEntryKind) -> Self {
        Self { name: name.into(), kind, size: None, modified: None, permissions: None }
    }

    /// Set the size (builder style).
    pub fn with_size(mut self, size: u64) -> Self {
        self.size = Some(size);
        self
    }

    /// The name if it is safe to use as ONE local file name, else `None`: not empty, not `.` /
    /// `..`, no `/`, `\`, `:` (drive letters, NTFS streams), NUL or other control characters, no
    /// leading or trailing space and no trailing dot (Windows strips them), not a Windows device
    /// name (`CON`, `NUL`, `COM1`, … with any extension), at most 255 bytes.
    pub fn safe_file_name(&self) -> Option<&str> {
        safe_file_name(&self.name)
    }
}

/// See [`SftpEntry::safe_file_name`].
pub(crate) fn safe_file_name(name: &str) -> Option<&str> {
    const DEVICES: [&str; 22] = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6",
        "lpt7", "lpt8", "lpt9",
    ];
    let stem = name.split('.').next().unwrap_or(name).trim_end().to_ascii_lowercase();
    let bad = name.is_empty()
        || name.len() > 255
        || name == "."
        || name == ".."
        || name.chars().any(|c| matches!(c, '/' | '\\' | ':') || c.is_control())
        || name.starts_with(' ')
        || name.ends_with(' ')
        || name.ends_with('.')
        || DEVICES.contains(&stem.as_str());
    (!bad).then_some(name)
}

/// Progress of an SFTP transfer: bytes moved so far (at most about 10 per second per transfer).
/// Written in `First`.
#[derive(Message, Clone, Debug)]
#[non_exhaustive]
pub struct SftpProgress {
    /// The operation.
    pub id: RequestId,
    /// The connection.
    pub name: SshName,
    /// Bytes moved so far.
    pub done: u64,
    /// The size, when known.
    pub total: Option<u64>,
}

/// The one answer to an SFTP operation. Written in `First`.
#[derive(Message, Clone, Debug)]
#[non_exhaustive]
pub struct SftpFinished {
    /// The operation.
    pub id: RequestId,
    /// The connection.
    pub name: SshName,
    /// Whether the operation reached the server: `Some(true)` it did (an interrupted upload may
    /// leave a partial file), `Some(false)` it was never sent, `None` unknown.
    pub started: Option<bool>,
    /// The outcome, or the error (`Ssh` carries the server's SFTP status, e.g. `No such file`).
    pub result: Result<SftpOutcome, BackendError>,
}

/// SFTP operations (feature `sftp`). Each returns the [`RequestId`] of its one [`SftpFinished`];
/// transfers also report [`SftpProgress`]. They wait while the connection is still connecting.
/// Limits: [`SshTarget::with_max_transfer_bytes`](crate::SshTarget::with_max_transfer_bytes)
/// and [`with_sftp_timeout`](crate::SshTarget::with_sftp_timeout).
impl SshClient {
    /// Write `data` to the remote file `remote` (created or truncated).
    pub fn upload(&self, name: impl Into<SshName>, remote: impl Into<String>, data: impl Into<Vec<u8>>) -> RequestId {
        self.push_sftp(name.into(), SftpOp::UploadBytes { remote: remote.into(), data: data.into() })
    }

    /// Copy the local file `local` to `remote` (created or truncated).
    pub fn upload_file(&self, name: impl Into<SshName>, local: impl Into<PathBuf>, remote: impl Into<String>) -> RequestId {
        self.push_sftp(name.into(), SftpOp::UploadFile { local: local.into(), remote: remote.into() })
    }

    /// Read the remote file `remote` into memory ([`SftpOutcome::Data`]).
    pub fn download(&self, name: impl Into<SshName>, remote: impl Into<String>) -> RequestId {
        self.push_sftp(name.into(), SftpOp::Download { remote: remote.into() })
    }

    /// Copy the remote file `remote` to the local file `local` (replaced only when the whole file
    /// arrived).
    pub fn download_file(&self, name: impl Into<SshName>, remote: impl Into<String>, local: impl Into<PathBuf>) -> RequestId {
        self.push_sftp(name.into(), SftpOp::DownloadFile { remote: remote.into(), local: local.into() })
    }

    /// List the remote directory `path` ([`SftpOutcome::Listing`], at most 10 000 entries).
    pub fn list_dir(&self, name: impl Into<SshName>, path: impl Into<String>) -> RequestId {
        self.push_sftp(name.into(), SftpOp::List { path: path.into() })
    }

    /// Create the remote directory `path`.
    pub fn create_dir(&self, name: impl Into<SshName>, path: impl Into<String>) -> RequestId {
        self.push_sftp(name.into(), SftpOp::CreateDir { path: path.into() })
    }

    /// Remove the remote file `path`.
    pub fn remove_file(&self, name: impl Into<SshName>, path: impl Into<String>) -> RequestId {
        self.push_sftp(name.into(), SftpOp::RemoveFile { path: path.into() })
    }

    /// Remove the empty remote directory `path`.
    pub fn remove_dir(&self, name: impl Into<SshName>, path: impl Into<String>) -> RequestId {
        self.push_sftp(name.into(), SftpOp::RemoveDir { path: path.into() })
    }

    /// Rename or move the remote `from` to `to`.
    pub fn rename(&self, name: impl Into<SshName>, from: impl Into<String>, to: impl Into<String>) -> RequestId {
        self.push_sftp(name.into(), SftpOp::Rename { from: from.into(), to: to.into() })
    }

    /// Any [`SftpOp`].
    pub fn sftp(&self, name: impl Into<SshName>, op: SftpOp) -> RequestId {
        self.push_sftp(name.into(), op)
    }
}

/// The russh-sftp side, run on the SSH thread. Local file I/O runs on tokio's blocking pool
/// (capped at 2 threads), never on the SSH thread itself.
pub(super) mod ops {
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use russh_sftp::client::error::Error as SftpError;
    use russh_sftp::client::RawSftpSession;
    use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags, StatusCode};
    use tokio::task::JoinSet;

    use super::{SftpEntry, SftpEntryKind, SftpOp, SftpOutcome, MAX_ENTRY_NAME, MAX_LIST_ENTRIES, MAX_LIST_NAME_BYTES};
    use crate::request::RequestId;
    use crate::response::BackendError;

    /// Bytes per SFTP read / write request (OpenSSH serves up to 255 KiB per read; 32 KiB writes
    /// fit every server's packet limit).
    const READ_CHUNK: u32 = 64 * 1024;
    const WRITE_CHUNK: usize = 32 * 1024;
    /// Writes in flight at once for an upload.
    const WRITE_WINDOW: usize = 16;
    /// A download keeps at most this many bytes requested or received but not yet written: 16
    /// reads of 64 KiB in flight (the memory a download uses, whatever the file size).
    const READ_WINDOW_BYTES: u64 = 16 * READ_CHUNK as u64;
    const PROGRESS_EVERY: Duration = Duration::from_millis(100);

    /// What an operation reports while it runs.
    pub(in crate::ssh) trait Reporter {
        /// The first request is about to go out.
        fn started(&mut self);
        /// Bytes moved so far.
        fn progress(&mut self, done: u64, total: Option<u64>);
    }

    /// The server's words for an SFTP error.
    pub(in crate::ssh) fn map_error(error: SftpError) -> BackendError {
        match error {
            SftpError::Status(status) => {
                let mut message: String = status.error_message.chars().filter(|c| !c.is_control()).take(200).collect();
                if message.is_empty() {
                    message = status.status_code.to_string();
                }
                BackendError::Ssh(format!("SFTP: {message}"))
            }
            SftpError::Timeout => BackendError::Timeout("the SFTP server did not answer a request in time".into()),
            // russh-sftp's words for "the channel under the session is gone" (its request loop
            // ended, so pending answers are dropped) and an I/O error on that channel: the
            // connection was lost, not a server error. `sent` is decided by the caller.
            SftpError::UnexpectedBehavior(why) if channel_gone(&why) => BackendError::disconnected(format!("the SFTP channel ended ({why})"), None),
            SftpError::IO(why) => BackendError::disconnected(format!("the SFTP channel failed ({why})"), None),
            other => BackendError::Ssh(format!("SFTP: {other}")),
        }
    }

    /// russh-sftp 3.0.1 (`client/rawsession.rs`, `client/error.rs`): the texts it uses when the
    /// channel's request loop is gone.
    fn channel_gone(why: &str) -> bool {
        why == "sender dropped" || why == "session closed" || why.starts_with("SendError") || why.starts_with("RecvError")
    }

    /// A download must reach the size the server reported when the file was opened. A file that
    /// ends earlier was cut short on the server meanwhile: an error, never a short success. A size
    /// of 0 or no size (some servers and special files report that) is no promise: then the end
    /// of the file is wherever the server says so.
    pub(super) fn check_downloaded(written: u64, total: Option<u64>) -> Result<u64, BackendError> {
        match total {
            Some(total) if total > 0 && written < total => Err(BackendError::Ssh(format!(
                "SFTP: the remote file was cut short during the download (expected {total} bytes, its size when it was opened; received {written})"
            ))),
            _ => Ok(written),
        }
    }

    fn too_large(limit: u64) -> BackendError {
        BackendError::BodyTooLarge { limit }
    }

    /// A local file problem before the transfer started (opening or creating the local file):
    /// nothing was sent, `InvalidRequest`.
    fn local_error(what: &str, path: &Path, error: &std::io::Error) -> BackendError {
        BackendError::InvalidRequest(format!("{what} `{}`: {error}", super::file_name(path)))
    }

    /// A local file problem after the transfer started (reading the file mid-upload, writing,
    /// syncing or renaming the download): bytes already went over the connection, so `Ssh`, never
    /// `InvalidRequest` (that would say "never sent").
    fn local_error_during(what: &str, path: &Path, error: &std::io::Error) -> BackendError {
        BackendError::Ssh(format!("{what} `{}`: {error}", super::file_name(path)))
    }

    /// Run blocking local file work on the blocking pool.
    async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, BackendError> + Send + 'static) -> Result<T, BackendError> {
        tokio::task::spawn_blocking(work).await.map_err(|e| BackendError::Network(format!("local file work failed ({e})")))?
    }

    /// A part file next to `local` with a name no other download uses:
    /// `<name>.<process>-<request>-<n>.part`.
    pub(in crate::ssh) fn part_path(local: &Path, id: RequestId) -> PathBuf {
        crate::download::part_path(local, id)
    }

    struct Throttle {
        last: Option<Instant>,
    }

    impl Throttle {
        fn due(&mut self) -> bool {
            let now = Instant::now();
            if self.last.is_none_or(|last| now.saturating_duration_since(last) >= PROGRESS_EVERY) {
                self.last = Some(now);
                true
            } else {
                false
            }
        }
    }

    /// Run one operation. The caller puts it under the operation's deadline; `part` is the part
    /// file of a `DownloadFile` (see [`part_path`]).
    pub(in crate::ssh) async fn run(
        session: &std::sync::Arc<RawSftpSession>,
        op: SftpOp,
        max_bytes: u64,
        part: Option<PathBuf>,
        report: &mut impl Reporter,
    ) -> Result<SftpOutcome, BackendError> {
        match op {
            SftpOp::UploadBytes { remote, data } => {
                report.started();
                let total = u64::try_from(data.len()).unwrap_or(u64::MAX);
                let bytes = upload(session, &remote, total, report, ChunkSource::Memory { data, at: 0 }).await?;
                Ok(SftpOutcome::Uploaded { bytes })
            }
            SftpOp::UploadFile { local, remote } => {
                let path = local.clone();
                let (file, total) = blocking(move || {
                    let file = std::fs::File::open(&path).map_err(|e| local_error("could not open the local file", &path, &e))?;
                    let total = file.metadata().map_err(|e| local_error("could not read the local file", &path, &e))?.len();
                    Ok((file, total))
                })
                .await?;
                if total > max_bytes {
                    return Err(BackendError::RequestTooLarge { limit: max_bytes, size: total });
                }
                report.started();
                let bytes = upload(session, &remote, total, report, ChunkSource::File { file: Some(file), path: local }).await?;
                Ok(SftpOutcome::Uploaded { bytes })
            }
            SftpOp::Download { remote } => {
                report.started();
                let mut sink = Sink::Memory(Vec::new());
                download(session, &remote, max_bytes, report, &mut sink).await?;
                match sink {
                    Sink::Memory(data) => Ok(SftpOutcome::Data(data)),
                    Sink::File { .. } => Err(BackendError::Ssh("internal: wrong download sink".into())),
                }
            }
            SftpOp::DownloadFile { remote, local } => {
                let part = part.unwrap_or_else(|| part_path(&local, crate::request::RequestId::next()));
                let (path, target) = (part.clone(), local.clone());
                let file = blocking(move || {
                    if target.is_dir() {
                        return Err(BackendError::InvalidRequest(format!("`{}` is a folder, not a file", super::file_name(&target))));
                    }
                    // Part files of this target that an earlier run left (it ended mid-download).
                    crate::download::remove_stale_parts(&target);
                    // `create_new`: never truncate a file that happens to have that name.
                    std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| local_error("could not create the local file", &path, &e))
                })
                .await?;
                report.started();
                let mut sink = Sink::File { file: Some(file), path: part.clone() };
                let result = download(session, &remote, max_bytes, report, &mut sink).await;
                let file = match sink {
                    Sink::File { file, .. } => file,
                    Sink::Memory(_) => None,
                };
                let (part_for_io, local_for_io) = (part.clone(), local.clone());
                blocking(move || {
                    // Flushed and synced to disk before the rename, as the HTTP download and
                    // `SecretFile`: a power cut never leaves a renamed but empty or partial file.
                    let synced = match file {
                        Some(mut file) => {
                            file.flush().and_then(|()| file.sync_all()).map_err(|e| local_error_during("could not write the local file", &part_for_io, &e))
                        }
                        None => Ok(()),
                    };
                    match result.and_then(|bytes| synced.map(|()| bytes)) {
                        Ok(bytes) => match std::fs::rename(&part_for_io, &local_for_io) {
                            Ok(()) => {
                                let folder = match local_for_io.parent() {
                                    Some(parent) if !parent.as_os_str().is_empty() => parent,
                                    _ => Path::new("."),
                                };
                                crate::secret_file::sync_dir(folder);
                                Ok(SftpOutcome::Downloaded { bytes })
                            }
                            Err(e) => {
                                let _ = std::fs::remove_file(&part_for_io);
                                Err(local_error_during("could not move the download into place", &local_for_io, &e))
                            }
                        },
                        Err(error) => {
                            let _ = std::fs::remove_file(&part_for_io);
                            Err(error)
                        }
                    }
                })
                .await
            }
            SftpOp::List { path } => {
                report.started();
                list(session, &path).await.map(SftpOutcome::Listing)
            }
            SftpOp::CreateDir { path } => {
                report.started();
                session.mkdir(path, FileAttributes::empty()).await.map_err(map_error)?;
                Ok(SftpOutcome::Done)
            }
            SftpOp::RemoveFile { path } => {
                report.started();
                session.remove(path).await.map_err(map_error)?;
                Ok(SftpOutcome::Done)
            }
            SftpOp::RemoveDir { path } => {
                report.started();
                session.rmdir(path).await.map_err(map_error)?;
                Ok(SftpOutcome::Done)
            }
            SftpOp::Rename { from, to } => {
                report.started();
                session.rename(from, to).await.map_err(map_error)?;
                Ok(SftpOutcome::Done)
            }
        }
    }

    enum ChunkSource {
        Memory { data: Vec<u8>, at: usize },
        File { file: Option<std::fs::File>, path: PathBuf },
    }

    impl ChunkSource {
        /// The next chunk (empty at the end). A local file is read on the blocking pool.
        async fn next(&mut self) -> Result<Vec<u8>, BackendError> {
            match self {
                ChunkSource::Memory { data, at } => {
                    let end = at.saturating_add(WRITE_CHUNK).min(data.len());
                    let chunk = data.get(*at..end).map(<[u8]>::to_vec).unwrap_or_default();
                    *at = end;
                    Ok(chunk)
                }
                ChunkSource::File { file, path } => {
                    let Some(mut handle) = file.take() else { return Ok(Vec::new()) };
                    let path = path.clone();
                    let (handle, chunk) = blocking(move || {
                        let mut chunk = vec![0; WRITE_CHUNK];
                        let mut filled = 0;
                        while filled < chunk.len() {
                            let Some(rest) = chunk.get_mut(filled..) else { break };
                            match handle.read(rest) {
                                Ok(0) => break,
                                Ok(n) => filled = filled.saturating_add(n),
                                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                                Err(e) => return Err(local_error_during("could not read the local file", &path, &e)),
                            }
                        }
                        chunk.truncate(filled);
                        Ok((handle, chunk))
                    })
                    .await?;
                    *file = Some(handle);
                    Ok(chunk)
                }
            }
        }
    }

    enum Sink {
        Memory(Vec<u8>),
        File { file: Option<std::fs::File>, path: PathBuf },
    }

    impl Sink {
        /// Keep a chunk. A local file is written on the blocking pool.
        async fn put(&mut self, chunk: Vec<u8>) -> Result<(), BackendError> {
            match self {
                Sink::Memory(data) => {
                    data.extend_from_slice(&chunk);
                    Ok(())
                }
                Sink::File { file, path } => {
                    let Some(mut handle) = file.take() else { return Err(BackendError::Ssh("internal: the local file is gone".into())) };
                    let path = path.clone();
                    let handle = blocking(move || {
                        handle.write_all(&chunk).map_err(|e| local_error_during("could not write the local file", &path, &e))?;
                        Ok(handle)
                    })
                    .await?;
                    *file = Some(handle);
                    Ok(())
                }
            }
        }
    }

    async fn upload(
        session: &std::sync::Arc<RawSftpSession>,
        remote: &str,
        total: u64,
        report: &mut impl Reporter,
        mut source: ChunkSource,
    ) -> Result<u64, BackendError> {
        let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE;
        let handle = session.open(remote, flags, FileAttributes::empty()).await.map_err(map_error)?.handle;
        let guard = HandleGuard::new(session, &handle);
        let result = write_all(session, &handle, total, report, &mut source).await;
        let closed = guard.close().await.map_err(map_error);
        let written = check_written(result?, total)?;
        closed?;
        report.progress(written, Some(total));
        Ok(written)
    }

    /// A local file that shrank while it was read ends early: the remote file is incomplete, so
    /// that is an error (bytes were written: `Ssh`, never `InvalidRequest`), not a short success.
    pub(super) fn check_written(written: u64, total: u64) -> Result<u64, BackendError> {
        if written == total {
            Ok(written)
        } else {
            Err(changed_size())
        }
    }

    /// The answer when the local file changed size during an upload (it grew or shrank).
    pub(super) fn changed_size() -> BackendError {
        BackendError::Ssh("the local file changed size during the upload; the remote file is incomplete".into())
    }

    /// Pipelined writes: up to `WRITE_WINDOW` in flight; the first failure stops the upload.
    async fn write_all(
        session: &std::sync::Arc<RawSftpSession>,
        handle: &str,
        total: u64,
        report: &mut impl Reporter,
        source: &mut ChunkSource,
    ) -> Result<u64, BackendError> {
        let mut in_flight: JoinSet<Result<u64, SftpError>> = JoinSet::new();
        let mut offset: u64 = 0;
        let mut acked: u64 = 0;
        let mut throttle = Throttle { last: None };
        let mut finished_reading = false;
        loop {
            while !finished_reading && in_flight.len() < WRITE_WINDOW {
                let chunk = source.next().await?;
                if chunk.is_empty() {
                    finished_reading = true;
                    break;
                }
                let len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
                if offset.saturating_add(len) > total {
                    // The local file grew while it was uploaded: stop at the size that was checked.
                    // Bytes were already written: not `InvalidRequest` (that would say "never sent").
                    return Err(changed_size());
                }
                let (session, handle, at) = (std::sync::Arc::clone(session), handle.to_string(), offset);
                in_flight.spawn(async move { session.write(handle, at, chunk).await.map(|_| len) });
                offset = offset.saturating_add(len);
            }
            match in_flight.join_next().await {
                None => return Ok(acked),
                Some(Ok(Ok(len))) => {
                    acked = acked.saturating_add(len);
                    if throttle.due() {
                        report.progress(acked, Some(total));
                    }
                }
                Some(Ok(Err(error))) => return Err(map_error(error)),
                Some(Err(join)) => return Err(BackendError::Ssh(format!("an SFTP write task failed: {join}"))),
            }
        }
    }

    /// An open remote handle that is closed when the operation ends, also when the operation is
    /// cancelled or times out (its future is dropped): then the close is sent from a task of its
    /// own, so the server never keeps handles of abandoned transfers.
    struct HandleGuard {
        session: Arc<RawSftpSession>,
        handle: Option<String>,
    }

    impl HandleGuard {
        fn new(session: &Arc<RawSftpSession>, handle: &str) -> Self {
            Self { session: Arc::clone(session), handle: Some(handle.to_string()) }
        }

        async fn close(mut self) -> Result<(), SftpError> {
            match self.handle.take() {
                Some(handle) => self.session.close(handle).await.map(|_| ()),
                None => Ok(()),
            }
        }
    }

    impl Drop for HandleGuard {
        fn drop(&mut self) {
            // Dropped on the SSH thread, inside its runtime. Without a runtime (the thread is
            // already ending) there is nothing to send the close with: the session closes anyway.
            if let (Some(handle), Ok(runtime)) = (self.handle.take(), tokio::runtime::Handle::try_current()) {
                let session = Arc::clone(&self.session);
                drop(runtime.spawn(async move {
                    let _ = session.close(handle).await;
                }));
            }
        }
    }

    /// What one read at an offset gave.
    enum ReadOutcome {
        /// Bytes (at most the length asked for; fewer is a short read, not the end).
        Data(Vec<u8>),
        /// Nothing at this offset: the end of the file.
        Eof,
    }

    /// Reads a remote file at an offset: an open SFTP handle (tests: an in-memory file).
    trait ReadAt: Clone + Send + Sync + 'static {
        fn read_at(&self, offset: u64, len: u32) -> impl Future<Output = Result<ReadOutcome, BackendError>> + Send + 'static;
    }

    /// An open SFTP handle.
    #[derive(Clone)]
    struct SftpFile {
        session: Arc<RawSftpSession>,
        handle: Arc<str>,
    }

    impl ReadAt for SftpFile {
        fn read_at(&self, offset: u64, len: u32) -> impl Future<Output = Result<ReadOutcome, BackendError>> + Send + 'static {
            let (session, handle) = (Arc::clone(&self.session), self.handle.to_string());
            async move {
                match session.read(handle, offset, len).await {
                    Ok(data) if data.data.is_empty() => Ok(ReadOutcome::Eof),
                    Ok(data) => Ok(ReadOutcome::Data(data.data)),
                    Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => Ok(ReadOutcome::Eof),
                    Err(error) => Err(map_error(error)),
                }
            }
        }
    }

    /// Download `remote` into `sink`. Returns the bytes written.
    async fn download(session: &Arc<RawSftpSession>, remote: &str, max_bytes: u64, report: &mut impl Reporter, sink: &mut Sink) -> Result<u64, BackendError> {
        let handle = session.open(remote, OpenFlags::READ, FileAttributes::empty()).await.map_err(map_error)?.handle;
        let guard = HandleGuard::new(session, &handle);
        let total = session.fstat(handle.as_str()).await.ok().and_then(|attrs| attrs.attrs.size);
        let result = if total.is_some_and(|total| total > max_bytes) {
            // The server says it is too large: refused before any data is read.
            Err(too_large(max_bytes))
        } else {
            if let (Sink::Memory(data), Some(size)) = (&mut *sink, total) {
                // Into memory: the reported size once (at most the transfer limit), instead of a
                // Vec that doubles while it grows (up to about twice the file at the peak).
                // Best effort: without the memory the Vec grows as usual.
                let _ = data.try_reserve_exact(usize::try_from(size).unwrap_or(0));
            }
            let file = SftpFile { session: Arc::clone(session), handle: Arc::from(handle.as_str()) };
            read_pipelined(&file, max_bytes, total, report, sink, READ_WINDOW_BYTES).await.and_then(|(written, _)| check_downloaded(written, total))
        };
        let _ = guard.close().await;
        if let Ok(done) = result {
            report.progress(done, total);
        }
        result
    }

    /// Pipelined reads into `sink`, in file order: reads of [`READ_CHUNK`] bytes go out ahead of
    /// the answers, as long as the bytes requested plus the bytes received but not yet written stay
    /// within `window` (bounded memory). Answers may arrive in any order; they are written to the
    /// sink in order. A short read (fewer bytes than asked, not at the end) asks again for the rest
    /// of its range. The file ends at the lowest offset the server answered with end of file; once
    /// that is known, no new reads go out. At most `max_bytes + 1` bytes are asked for (one byte
    /// more proves the file is too large). The first error stops everything (reads still in
    /// flight are dropped and their answers ignored).
    ///
    /// Returns the bytes written and the largest number of bytes that were requested or waiting
    /// at once.
    async fn read_pipelined<R: ReadAt>(
        reader: &R,
        max_bytes: u64,
        total: Option<u64>,
        report: &mut impl Reporter,
        sink: &mut Sink,
        window: u64,
    ) -> Result<(u64, u64), BackendError> {
        let chunk = u64::from(READ_CHUNK).min(window.max(1));
        let limit = max_bytes.saturating_add(1);
        let mut in_flight: JoinSet<(u64, u64, Result<ReadOutcome, BackendError>)> = JoinSet::new();
        let spawn = |in_flight: &mut JoinSet<(u64, u64, Result<ReadOutcome, BackendError>)>, offset: u64, len: u64| {
            let read = reader.read_at(offset, u32::try_from(len).unwrap_or(READ_CHUNK));
            in_flight.spawn(async move { (offset, len, read.await) });
        };
        // Received, not yet written, by offset.
        let mut waiting: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        let (mut next, mut written, mut outstanding, mut peak) = (0u64, 0u64, 0u64, 0u64);
        let mut end: Option<u64> = None;
        let mut throttle = Throttle { last: None };
        loop {
            while end.is_none() && next < limit && outstanding.saturating_add(chunk) <= window {
                let len = chunk.min(limit - next);
                spawn(&mut in_flight, next, len);
                next = next.saturating_add(len);
                outstanding = outstanding.saturating_add(len);
            }
            peak = peak.max(outstanding);
            let Some(joined) = in_flight.join_next().await else { break };
            let (offset, asked, result) = joined.map_err(|e| BackendError::Ssh(format!("an SFTP read task failed: {e}")))?;
            outstanding = outstanding.saturating_sub(asked);
            match result? {
                ReadOutcome::Eof => end = Some(end.map_or(offset, |end| end.min(offset))),
                ReadOutcome::Data(data) => {
                    let got = u64::try_from(data.len()).unwrap_or(u64::MAX);
                    if got > asked {
                        return Err(BackendError::Ssh("SFTP: the server sent more bytes than were asked for".into()));
                    }
                    if got < asked {
                        // A short read: the rest of the range.
                        spawn(&mut in_flight, offset.saturating_add(got), asked - got);
                        outstanding = outstanding.saturating_add(asked - got);
                    }
                    outstanding = outstanding.saturating_add(got);
                    waiting.insert(offset, data);
                    while let Some(data) = waiting.remove(&written) {
                        let len = u64::try_from(data.len()).unwrap_or(u64::MAX);
                        outstanding = outstanding.saturating_sub(len);
                        let after = written.saturating_add(len);
                        if after > max_bytes {
                            return Err(too_large(max_bytes));
                        }
                        sink.put(data).await?;
                        written = after;
                        if throttle.due() {
                            report.progress(written, total);
                        }
                    }
                }
            }
        }
        // Every read is answered. Bytes the server sent beyond the end it reported (the file
        // grew meanwhile) are dropped; everything before the end must have been written.
        match end {
            Some(end) if end == written => Ok((written, peak)),
            Some(_) => Err(BackendError::Ssh("SFTP: the remote file changed size during the download".into())),
            None => Err(BackendError::Ssh("SFTP: the download ended without reaching the end of the file".into())),
        }
    }

    async fn list(session: &RawSftpSession, path: &str) -> Result<Vec<SftpEntry>, BackendError> {
        let handle = session.opendir(path).await.map_err(map_error)?.handle;
        let mut entries = Vec::new();
        let mut name_bytes = 0usize;
        let result = loop {
            match session.readdir(handle.as_str()).await {
                Ok(name) => {
                    let mut problem = None;
                    for file in name.files {
                        if file.filename == "." || file.filename == ".." {
                            continue;
                        }
                        if entries.len() >= MAX_LIST_ENTRIES {
                            problem = Some(format!("the directory has more than {MAX_LIST_ENTRIES} entries"));
                            break;
                        }
                        if file.filename.len() > MAX_ENTRY_NAME {
                            problem = Some(format!("the server listed a name longer than {MAX_ENTRY_NAME} bytes"));
                            break;
                        }
                        name_bytes = name_bytes.saturating_add(file.filename.len());
                        if name_bytes > MAX_LIST_NAME_BYTES {
                            problem = Some(format!("the listing's names are longer than {MAX_LIST_NAME_BYTES} bytes together"));
                            break;
                        }
                        let kind = match file.attrs.permissions.map(|_| file.attrs.file_type()) {
                            Some(FileType::File) => SftpEntryKind::File,
                            Some(FileType::Dir) => SftpEntryKind::Dir,
                            Some(FileType::Symlink) => SftpEntryKind::Symlink,
                            _ => SftpEntryKind::Other,
                        };
                        entries.push(SftpEntry {
                            name: file.filename,
                            kind,
                            size: file.attrs.size,
                            modified: file.attrs.mtime,
                            permissions: file.attrs.permissions.map(|p| p & 0o7777),
                        });
                    }
                    if let Some(problem) = problem {
                        break Err(BackendError::Ssh(problem));
                    }
                }
                Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => break Ok(()),
                Err(error) => break Err(map_error(error)),
            }
        };
        let _ = session.close(handle).await;
        result?;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// The download pipeline against an in-memory file that answers out of order, with short
    /// reads, slowly, or with an error.
    #[cfg(test)]
    mod pipeline_tests {
        use std::sync::Mutex;

        use super::*;

        #[derive(Default)]
        struct Seen {
            running: usize,
            peak_running: usize,
            asked: Vec<(u64, u32)>,
        }

        #[derive(Clone)]
        struct FakeFile {
            data: Arc<Mutex<Vec<u8>>>,
            short_reads: bool,
            fail_at: Option<u64>,
            /// Grow the file by this many bytes on the first read past its end.
            grow_once: Arc<Mutex<Option<usize>>>,
            seen: Arc<Mutex<Seen>>,
        }

        impl FakeFile {
            fn new(data: Vec<u8>) -> Self {
                Self { data: Arc::new(Mutex::new(data)), short_reads: false, fail_at: None, grow_once: Arc::default(), seen: Arc::default() }
            }
        }

        /// A deterministic scramble of an offset (answer order, short-read lengths).
        fn mix(offset: u64) -> u64 {
            offset.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17)
        }

        impl ReadAt for FakeFile {
            fn read_at(&self, offset: u64, len: u32) -> impl Future<Output = Result<ReadOutcome, BackendError>> + Send + 'static {
                let me = self.clone();
                async move {
                    {
                        let mut seen = me.seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        seen.running += 1;
                        seen.peak_running = seen.peak_running.max(seen.running);
                        seen.asked.push((offset, len));
                    }
                    // Answers come back in a scrambled order.
                    tokio::time::sleep(Duration::from_micros(mix(offset) % 3000)).await;
                    let outcome = (|| {
                        if me.fail_at.is_some_and(|at| offset >= at) {
                            return Err(BackendError::Ssh("SFTP: Failure".into()));
                        }
                        let mut data = me.data.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        let start = usize::try_from(offset).unwrap_or(usize::MAX);
                        if start >= data.len() {
                            if let Some(grow) = me.grow_once.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
                                let len = data.len();
                                data.resize(len + grow, 0xEE);
                            }
                            return Ok(ReadOutcome::Eof);
                        }
                        let mut want = usize::try_from(len).unwrap_or(0);
                        if me.short_reads {
                            want = 1 + usize::try_from(mix(offset) % u64::from(len)).unwrap_or(0);
                        }
                        let end = start.saturating_add(want).min(data.len());
                        Ok(ReadOutcome::Data(data[start..end].to_vec()))
                    })();
                    me.seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner).running -= 1;
                    outcome
                }
            }
        }

        #[derive(Default)]
        struct Collect {
            started: bool,
            progress: Vec<(u64, Option<u64>)>,
        }

        impl Reporter for Collect {
            fn started(&mut self) {
                self.started = true;
            }
            fn progress(&mut self, done: u64, total: Option<u64>) {
                self.progress.push((done, total));
            }
        }

        fn content(len: usize) -> Vec<u8> {
            (0..len).map(|i| u8::try_from(mix(i as u64) >> 56).unwrap_or(0)).collect()
        }

        fn run(file: &FakeFile, max_bytes: u64, window: u64) -> (Result<(u64, u64), BackendError>, Vec<u8>, Collect) {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap_or_else(|e| panic!("{e}"));
            let mut sink = Sink::Memory(Vec::new());
            let mut report = Collect::default();
            let total = u64::try_from(file.data.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len()).ok();
            let result = runtime.block_on(read_pipelined(file, max_bytes, total, &mut report, &mut sink, window));
            let Sink::Memory(bytes) = sink else { panic!("wrong sink") };
            (result, bytes, report)
        }

        #[test]
        fn every_size_arrives_whole_and_in_order() {
            let chunk = usize::try_from(READ_CHUNK).unwrap_or(0);
            for len in [0, 1, chunk - 1, chunk, chunk + 1, 3 * chunk, 16 * chunk, 16 * chunk + 7, 40 * chunk + 12_345] {
                for short_reads in [false, true] {
                    let data = content(len);
                    let mut file = FakeFile::new(data.clone());
                    file.short_reads = short_reads;
                    let (result, bytes, _) = run(&file, u64::MAX - 1, READ_WINDOW_BYTES);
                    let (written, peak) = result.unwrap_or_else(|e| panic!("{len} bytes, short reads {short_reads}: {e}"));
                    assert_eq!(written, len as u64);
                    assert!(bytes == data, "{len} bytes, short reads {short_reads}: content differs");
                    assert!(peak <= READ_WINDOW_BYTES, "memory bound: {peak}");
                }
            }
        }

        #[test]
        fn reads_really_run_in_parallel_within_the_window() {
            let file = FakeFile::new(content(64 * usize::try_from(READ_CHUNK).unwrap_or(0)));
            let (result, _, _) = run(&file, u64::MAX - 1, READ_WINDOW_BYTES);
            let (_, peak) = result.unwrap_or_else(|e| panic!("{e}"));
            let seen = file.seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(seen.peak_running, 16, "16 reads of 64 KiB in flight");
            assert_eq!(peak, READ_WINDOW_BYTES);
            // Never more than the file + the one read that finds its end per slot of the window.
            assert!(seen.asked.len() <= 64 + 16, "{} reads", seen.asked.len());
        }

        #[test]
        fn a_smaller_window_bounds_memory_with_short_reads() {
            let mut file = FakeFile::new(content(5_000_000));
            file.short_reads = true;
            let window = 3 * u64::from(READ_CHUNK);
            let (result, bytes, _) = run(&file, u64::MAX - 1, window);
            let (written, peak) = result.unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(written, 5_000_000);
            assert!(bytes == content(5_000_000));
            assert!(peak <= window, "{peak} > {window}");
        }

        #[test]
        fn the_size_limit_holds_exactly() {
            let file = FakeFile::new(content(300_000));
            let (result, _, _) = run(&file, 300_000, READ_WINDOW_BYTES);
            assert_eq!(result.map(|(w, _)| w).ok(), Some(300_000), "exactly the limit is fine");
            let (result, _, _) = run(&file, 299_999, READ_WINDOW_BYTES);
            assert!(matches!(result, Err(BackendError::BodyTooLarge { limit: 299_999 })), "{result:?}");
            let seen = file.seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(seen.asked.iter().all(|(offset, len)| offset + u64::from(*len) <= 300_000 + 300_000), "never asks past the limit + 1");
        }

        #[test]
        fn an_error_midway_stops_the_download() {
            let mut file = FakeFile::new(content(2_000_000));
            file.fail_at = Some(1_000_000);
            let (result, bytes, _) = run(&file, u64::MAX - 1, READ_WINDOW_BYTES);
            assert!(matches!(&result, Err(BackendError::Ssh(why)) if why.contains("Failure")), "{result:?}");
            assert!(bytes.len() <= 1_000_000 + usize::try_from(READ_CHUNK).unwrap_or(0), "nothing past the read that failed is written");
            assert!(bytes == content(2_000_000)[..bytes.len()], "what was written is the file's start");
        }

        #[test]
        fn progress_only_grows_and_ends_at_the_written_size() {
            let file = FakeFile::new(content(3_000_000));
            let (result, _, report) = run(&file, u64::MAX - 1, READ_WINDOW_BYTES);
            assert!(result.is_ok());
            assert!(report.progress.windows(2).all(|w| w[0].0 <= w[1].0), "{:?}", report.progress);
            assert!(report.progress.iter().all(|(done, total)| *done <= 3_000_000 && *total == Some(3_000_000)));
        }

        #[test]
        fn a_file_that_grows_while_it_is_read_gives_a_consistent_prefix_or_an_honest_error() {
            let data = content(200_000);
            let file = FakeFile::new(data.clone());
            *file.grow_once.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(100_000);
            let (result, bytes, _) = run(&file, u64::MAX - 1, READ_WINDOW_BYTES);
            match result {
                Ok((written, _)) => {
                    assert_eq!(written, bytes.len() as u64);
                    assert!(bytes[..200_000] == data[..], "the original bytes first");
                }
                Err(error) => assert!(matches!(&error, BackendError::Ssh(why) if why.contains("changed size")), "{error:?}"),
            }
        }

        /// Local file errors once a transfer runs (reading the file mid-upload, writing the
        /// download) are `Ssh`: bytes already went out, so never "never sent".
        #[test]
        fn local_file_errors_during_a_transfer_are_not_never_sent() {
            let dir = std::env::temp_dir().join(format!("bnb-sftp-local-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
            let path = dir.join("local.bin");
            std::fs::write(&path, vec![5u8; 100_000]).unwrap_or_else(|e| panic!("{e}"));
            let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap_or_else(|e| panic!("{e}"));

            // A file that cannot be read (opened for writing only) fails on its first chunk.
            let write_only = std::fs::OpenOptions::new().write(true).open(&path).unwrap_or_else(|e| panic!("{e}"));
            let mut source = ChunkSource::File { file: Some(write_only), path: path.clone() };
            let error = runtime.block_on(source.next()).err();
            assert!(matches!(&error, Some(BackendError::Ssh(why)) if why.contains("could not read the local file")), "{error:?}");
            assert_ne!(error.as_ref().and_then(BackendError::was_sent), Some(false));

            // A file that cannot be written (opened for reading only) fails on its first chunk.
            let read_only = std::fs::File::open(&path).unwrap_or_else(|e| panic!("{e}"));
            let mut sink = Sink::File { file: Some(read_only), path: path.clone() };
            let error = runtime.block_on(sink.put(vec![1u8; 10])).err();
            assert!(matches!(&error, Some(BackendError::Ssh(why)) if why.contains("could not write the local file")), "{error:?}");
            assert_ne!(error.as_ref().and_then(BackendError::was_sent), Some(false));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{safe_file_name, SftpEntry, SftpEntryKind};
    use crate::response::BackendError;

    #[test]
    fn a_local_file_that_changed_size_is_an_honest_error_not_a_short_success() {
        assert_eq!(super::ops::check_written(4096, 4096).ok(), Some(4096));
        for (written, total) in [(1024, 4096), (0, 4096), (5000, 4096)] {
            let error = super::ops::check_written(written, total).err();
            assert!(matches!(&error, Some(BackendError::Ssh(why)) if why.contains("changed size")), "{error:?}");
            // Bytes may have been written: never "not sent".
            assert_eq!(error.and_then(|e| e.was_sent()), None);
        }
    }

    #[test]
    fn a_remote_file_cut_short_is_an_error_with_both_sizes_and_no_size_reads_to_the_end() {
        let error = super::ops::check_downloaded(1_000, Some(4_096)).err();
        assert!(
            matches!(&error, Some(BackendError::Ssh(why)) if why == "SFTP: the remote file was cut short during the download (expected 4096 bytes, its size when it was opened; received 1000)"),
            "{error:?}"
        );
        assert_eq!(super::ops::check_downloaded(0, Some(1)).ok(), None);
        // Complete, grown, or no size promised (0 or none): the end of the file is the end.
        for (written, total) in [(4_096, Some(4_096)), (5_000, Some(4_096)), (123, Some(0)), (0, Some(0)), (123, None), (0, None)] {
            assert_eq!(super::ops::check_downloaded(written, total).ok(), Some(written), "{written} of {total:?}");
        }
    }

    #[test]
    fn a_lost_sftp_channel_is_a_disconnect_and_server_errors_stay_ssh_errors() {
        use russh_sftp::client::error::Error as SftpError;
        use russh_sftp::protocol::{Status, StatusCode};
        for gone in ["sender dropped", "session closed", "SendError: channel closed", "RecvError: channel closed"] {
            let error = super::ops::map_error(SftpError::UnexpectedBehavior(gone.into()));
            assert!(matches!(&error, BackendError::Disconnected { sent: None, .. }), "{gone}: {error:?}");
        }
        assert!(matches!(super::ops::map_error(SftpError::IO("broken pipe".into())), BackendError::Disconnected { sent: None, .. }));
        let status = Status { id: 1, status_code: StatusCode::NoSuchFile, error_message: "no such file".into(), language_tag: "en".into() };
        assert_eq!(super::ops::map_error(SftpError::Status(status)), BackendError::Ssh("SFTP: no such file".into()));
        assert!(matches!(super::ops::map_error(SftpError::UnexpectedBehavior("Duplicate version".into())), BackendError::Ssh(_)));
        assert!(matches!(super::ops::map_error(SftpError::Timeout), BackendError::Timeout(_)));
    }

    #[test]
    fn listed_names_that_are_not_one_plain_file_name_are_unsafe() {
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "a/b",
            "a\\b",
            "C:\\Windows\\evil.dll",
            "c:evil",
            "/etc/passwd",
            "x\u{0}y",
            "tab\there",
            "new\nline",
            " lead",
            "trail ",
            "dot.",
            "CON",
            "nul.txt",
            "Com1.log",
            "lpt9",
        ] {
            assert_eq!(safe_file_name(bad), None, "{bad:?} passed");
        }
        assert_eq!(safe_file_name(&"x".repeat(256)), None);
        for good in ["notes.txt", ".bashrc", "a b.tar.gz", "console.log", "über.txt", "CONFIG"] {
            assert_eq!(safe_file_name(good), Some(good));
        }
        assert_eq!(SftpEntry::new("../../.ssh/authorized_keys", SftpEntryKind::File).safe_file_name(), None);
    }
}
