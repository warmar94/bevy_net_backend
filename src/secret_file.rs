//! [`SecretFile`]: one [`Secret`] (e.g. a refresh token) kept in a file between runs.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use zeroize::Zeroizing;

use crate::credentials::Secret;

/// The first line of every secret file (the format and its version).
const HEADER: &[u8] = b"bevy_net_backend secret file 1\n";
/// A secret file is a token or a key; anything above this is not one.
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// A file that holds one [`Secret`] between runs of the game, e.g. the refresh token the game got
/// from its own login: store it after the login, load it at the next start, remove it on logout.
/// The crate never logs in or refreshes by itself; what the secret is and when it changes is the
/// game's.
///
/// - **Written atomically:** a new file next to it, flushed to disk, then renamed over the old one,
///   so a crash never leaves half a file. A missing folder is created.
/// - **Owner-only:** on Unix the file is created with mode `0600` (owner read / write), a missing
///   folder with `0700`; loading a file other users can read logs a warning (the next save writes
///   it `0600`). On Windows the file gets the permissions (ACL) it inherits from its folder: under
///   the user's profile (e.g. `%APPDATA%\<game>\` or `%LOCALAPPDATA%\<game>\`) that is the user,
///   `SYSTEM` and the Administrators group. Keep the file there.
/// - **Wiped and never logged:** the crate's buffers that hold the secret are overwritten with
///   zeros when dropped; errors and logs name the file and the I/O problem, never the content.
/// - **Format:** a first line naming the format, then the secret's text. A file without that line
///   (damaged, or not written by `SecretFile`) is refused with [`io::ErrorKind::InvalidData`].
///
/// Blocking file I/O: call it at startup, on login / logout, or from a task, not every frame.
///
/// ```no_run
/// use bevy_net_backend::{Secret, SecretFile};
///
/// let file = SecretFile::new("saves/refresh-token");
/// // After the game's own login:
/// file.save(&Secret::new("refresh-token-from-the-login-answer"))?;
/// // At the next start:
/// if let Some(refresh) = file.load()? {
///     // The game's own refresh call with `refresh.expose()`, then `BackendCredentials::set(..)`.
///     # let _ = refresh;
/// }
/// // On logout:
/// file.remove()?;
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct SecretFile {
    path: PathBuf,
}

impl fmt::Debug for SecretFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretFile").field("path", &self.path).finish()
    }
}

impl SecretFile {
    /// A secret file at `path` (nothing is read or written yet).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Where the file is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn fail(&self, kind: io::ErrorKind, what: &str, detail: impl fmt::Display) -> io::Error {
        io::Error::new(kind, format!("secret file `{}`: {what}: {detail}", self.path.display()))
    }

    /// The stored secret. `Ok(None)` when there is no file. A file that is not a secret file
    /// (damaged, another format, not UTF-8, larger than 64 KiB) is
    /// [`InvalidData`](io::ErrorKind::InvalidData); the message never quotes the content.
    pub fn load(&self) -> io::Result<Option<Secret>> {
        let mut file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(self.fail(e.kind(), "could not be opened", e)),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if file.metadata().is_ok_and(|meta| meta.permissions().mode() & 0o077 != 0) {
                tracing::warn!(">>> NET-BACKEND: secret file `{}` can be read by other users; the next save writes it owner-only (0600)", self.path.display());
            }
        }
        let limit = usize::try_from(MAX_FILE_BYTES).unwrap_or(usize::MAX);
        // Sized up front, so reading never moves the bytes to a new (unwiped) allocation.
        let mut bytes = Zeroizing::new(Vec::with_capacity(limit.saturating_add(1)));
        (&mut file).take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).map_err(|e| self.fail(e.kind(), "could not be read", e))?;
        if bytes.len() > limit {
            return Err(self.fail(io::ErrorKind::InvalidData, "not a secret file", format_args!("larger than {MAX_FILE_BYTES} bytes")));
        }
        let Some(body) = bytes.strip_prefix(HEADER) else {
            return Err(self.fail(io::ErrorKind::InvalidData, "not a secret file", "damaged, or not written by SecretFile"));
        };
        let text = std::str::from_utf8(body).map_err(|_| self.fail(io::ErrorKind::InvalidData, "not a secret file", "the secret is not UTF-8"))?;
        Ok(Some(Secret::new(text)))
    }

    /// Store `secret`, atomically and owner-only (see the type's notes). A missing folder is
    /// created.
    pub fn save(&self, secret: &Secret) -> io::Result<()> {
        let mut bytes = Zeroizing::new(Vec::with_capacity(HEADER.len() + secret.expose().len()));
        bytes.extend_from_slice(HEADER);
        bytes.extend_from_slice(secret.expose().as_bytes());
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(self.fail(io::ErrorKind::InvalidInput, "not written", format_args!("the secret is larger than {MAX_FILE_BYTES} bytes")));
        }
        let dir = match self.path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
            _ => PathBuf::from("."),
        };
        create_dir(&dir).map_err(|e| self.fail(e.kind(), "its folder could not be created", e))?;
        let name = self.path.file_name().ok_or_else(|| self.fail(io::ErrorKind::InvalidInput, "not a file path", "it has no file name"))?;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut temp_name = OsString::from(".");
        temp_name.push(name);
        temp_name.push(format!(".{}.{}.tmp", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
        let temp = dir.join(temp_name);
        let written = (|| {
            let mut file = create_owner_only(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp, &self.path)
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&temp);
            return Err(self.fail(e.kind(), "could not be written", e));
        }
        sync_dir(&dir);
        Ok(())
    }

    /// Delete the file (no file is fine).
    pub fn remove(&self) -> io::Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(self.fail(e.kind(), "could not be removed", e)),
        }
    }
}

/// A new file only the owner can read and write (Unix `0600`; Windows: the folder's inherited ACL).
fn create_owner_only(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// The folder and its missing parents (Unix `0700` for the new ones).
fn create_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Make the rename durable (Unix: flush the folder entry; best effort).
pub(crate) fn sync_dir(_dir: &Path) {
    #[cfg(unix)]
    if let Ok(dir) = fs::File::open(_dir) {
        let _ = dir.sync_all();
    }
}
