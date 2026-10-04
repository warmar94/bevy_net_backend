//! `SecretFile`: the round trip, the atomic replace, owner-only permissions, damaged files, and no
//! secret in `Debug`, errors or logs. Files live under `CARGO_TARGET_TMPDIR` (`target/tmp`).

use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use bevy_net_backend::{Secret, SecretFile};

/// A fresh folder for one test (removed first, so a failed earlier run leaves nothing behind).
fn folder(test: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("secret-file-{test}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = fs::remove_dir_all(&dir);
    dir
}

const SECRET: &str = "fake-refresh-5e5e-not-real";

#[test]
fn a_secret_round_trips_and_a_missing_file_is_none() {
    let dir = folder("round-trip");
    // The folder does not exist yet: `save` creates it.
    let file = SecretFile::new(dir.join("nested").join("refresh-token"));
    assert!(matches!(file.load(), Ok(None)));
    file.save(&Secret::new(SECRET)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(file.load().ok().flatten().as_ref().map(Secret::expose), Some(SECRET));
    // Replacing keeps exactly one file and no temporary file behind.
    file.save(&Secret::new("fake-second-7a7a")).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(file.load().ok().flatten().as_ref().map(Secret::expose), Some("fake-second-7a7a"));
    let names: Vec<_> = fs::read_dir(dir.join("nested")).unwrap_or_else(|e| panic!("{e}")).filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
    assert_eq!(names.len(), 1, "{names:?}");
    // Unicode, newlines and an empty secret survive unchanged.
    for text in ["", "line one\nline two\r\n", "ключ-🔑-鍵"] {
        file.save(&Secret::new(text)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(file.load().ok().flatten().as_ref().map(Secret::expose), Some(text));
    }
    file.remove().unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(file.load(), Ok(None)));
    // Removing a missing file is fine.
    assert!(file.remove().is_ok());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn damaged_files_are_invalid_data_and_never_quoted() {
    let dir = folder("damaged");
    fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
    let path = dir.join("token");
    let file = SecretFile::new(&path);
    let too_big = vec![b'a'; 70_000];
    let mut not_utf8 = b"bevy_net_backend secret file 1\n".to_vec();
    not_utf8.extend_from_slice(&[0xff, 0xfe, 0x00]);
    let cases: [(&str, Vec<u8>); 5] = [
        ("plain text", format!("{SECRET}\n").into_bytes()),
        ("empty", Vec::new()),
        ("another version", format!("bevy_net_backend secret file 2\n{SECRET}").into_bytes()),
        ("not UTF-8", not_utf8),
        ("too large", too_big),
    ];
    for (what, bytes) in cases {
        fs::write(&path, bytes).unwrap_or_else(|e| panic!("{e}"));
        let error = file.load().err().unwrap_or_else(|| panic!("{what}: loaded"));
        assert_eq!(error.kind(), ErrorKind::InvalidData, "{what}: {error}");
        let text = error.to_string();
        assert!(text.contains("not a secret file") && text.contains("token"), "{what}: {text}");
        assert!(!text.contains(SECRET) && !text.contains("aaaa"), "{what}: the content was quoted: {text}");
    }
    // A save over a damaged file replaces it.
    file.save(&Secret::new(SECRET)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(file.load().ok().flatten().as_ref().map(Secret::expose), Some(SECRET));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn bad_paths_and_oversized_secrets_are_errors_without_the_secret() {
    let dir = folder("bad-paths");
    fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
    // A folder where the file should be.
    let file = SecretFile::new(&dir);
    let error = file.save(&Secret::new(SECRET)).err().unwrap_or_else(|| panic!("a folder was overwritten"));
    assert!(!error.to_string().contains(SECRET), "{error}");
    assert!(file.load().is_err());
    // A file where the folder should be.
    fs::write(dir.join("plain"), b"x").unwrap_or_else(|e| panic!("{e}"));
    let error = SecretFile::new(dir.join("plain").join("token")).save(&Secret::new(SECRET)).err().unwrap_or_else(|| panic!("saved under a file"));
    assert!(error.to_string().contains("folder") && !error.to_string().contains(SECRET), "{error}");
    // Larger than a secret file may be: refused before anything is written.
    let big = Secret::new("b".repeat(70_000));
    let error = SecretFile::new(dir.join("big")).save(&big).err().unwrap_or_else(|| panic!("a 70 kB secret was saved"));
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(!dir.join("big").exists());
    // Nothing left behind: no temporary files.
    let names: Vec<_> = fs::read_dir(&dir).unwrap_or_else(|e| panic!("{e}")).filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
    assert_eq!(names.len(), 1, "{names:?}");
    assert!(!format!("{file:?}").contains(SECRET));
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn the_file_and_a_new_folder_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = folder("unix-mode");
    let path = dir.join("private").join("token");
    let file = SecretFile::new(&path);
    file.save(&Secret::new(SECRET)).unwrap_or_else(|e| panic!("{e}"));
    let mode = |p: &std::path::Path| fs::metadata(p).map(|m| m.permissions().mode() & 0o777).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&dir.join("private")), 0o700);
    // A file other users could read becomes owner-only on the next save (a new file is renamed over it).
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(file.load().ok().flatten().as_ref().map(Secret::expose), Some(SECRET));
    file.save(&Secret::new(SECRET)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(mode(&path), 0o600);
    let _ = fs::remove_dir_all(&dir);
}

/// Windows: the file is a plain new file in its folder, so it carries the folder's inherited ACL
/// (documented); nothing to set. The check here: the file is not read-only and replaces cleanly.
#[cfg(windows)]
#[test]
fn the_file_inherits_its_folder_and_replaces_cleanly() {
    let dir = folder("windows");
    let path = dir.join("token");
    let file = SecretFile::new(&path);
    file.save(&Secret::new(SECRET)).unwrap_or_else(|e| panic!("{e}"));
    let meta = fs::metadata(&path).unwrap_or_else(|e| panic!("{e}"));
    assert!(!meta.permissions().readonly());
    // Replacing over an existing file works (rename replaces on Windows too).
    for n in 0..5 {
        file.save(&Secret::new(format!("fake-{n}"))).unwrap_or_else(|e| panic!("{e}"));
    }
    assert_eq!(file.load().ok().flatten().as_ref().map(Secret::expose), Some("fake-4"));
    let _ = fs::remove_dir_all(&dir);
}
