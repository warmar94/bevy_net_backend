//! Resolving a connection's settings: directly from the `SshTarget`, or from a `Host` alias of an
//! ssh_config file (ssh2-config). Runs on the SSH runtime's blocking pool when connecting.
//!
//! `Include` is expanded HERE, before ssh2-config sees the text (ssh2-config follows includes
//! recursively without any limit: a file that includes itself overflows the stack and aborts the
//! process). Our expansion: depth at most 16 (OpenSSH's limit), at most 64 files, 1 MiB for all
//! files together, `~` and relative paths as OpenSSH (relative to `~/.ssh`), glob patterns. The
//! text handed to ssh2-config contains no `Include` line.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ssh2_config::{ParseRule, SshConfig};

use super::{check_host, check_user, file_name, SshTarget, TargetSource};
use crate::response::BackendError;

/// The most bytes read for one ssh_config, all included files together.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
/// How deep `Include` may nest (OpenSSH's limit).
const MAX_INCLUDE_DEPTH: usize = 16;
/// How many files one ssh_config may consist of.
const MAX_CONFIG_FILES: usize = 64;

/// Where and as whom to connect.
pub(crate) struct Resolved {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) user: String,
    /// `IdentityFile` entries of the config (no passphrase).
    pub(crate) identity_files: Vec<PathBuf>,
    /// The config's `ConnectTimeout`, if shorter than the target's.
    pub(crate) connect_timeout: Option<Duration>,
}

/// The user's home directory.
pub(crate) fn home() -> Option<PathBuf> {
    std::env::home_dir().filter(|p| !p.as_os_str().is_empty())
}

/// Read a small text file (at most `limit` bytes).
pub(crate) fn read_limited(path: &Path, limit: u64, what: &str) -> Result<String, BackendError> {
    let file = std::fs::File::open(path).map_err(|e| BackendError::InvalidRequest(format!("{what} `{}` cannot be read: {e}", file_name(path))))?;
    let mut text = String::new();
    file.take(limit.saturating_add(1))
        .read_to_string(&mut text)
        .map_err(|e| BackendError::InvalidRequest(format!("{what} `{}` cannot be read: {e}", file_name(path))))?;
    if u64::try_from(text.len()).unwrap_or(u64::MAX) > limit {
        return Err(BackendError::InvalidRequest(format!("{what} `{}` is larger than {limit} bytes", file_name(path))));
    }
    Ok(text)
}

#[derive(Default)]
struct Budget {
    bytes: u64,
    files: usize,
}

/// Append the lines of `path` to `out`, replacing every `Include` line by the included files.
fn expand(path: &Path, depth: usize, budget: &mut Budget, out: &mut String) -> Result<(), BackendError> {
    if depth > MAX_INCLUDE_DEPTH {
        return Err(BackendError::InvalidRequest(format!("ssh_config `Include` nested deeper than {MAX_INCLUDE_DEPTH} (an include loop?)")));
    }
    budget.files = budget.files.saturating_add(1);
    if budget.files > MAX_CONFIG_FILES {
        return Err(BackendError::InvalidRequest(format!("the ssh_config includes more than {MAX_CONFIG_FILES} files")));
    }
    let remaining = MAX_CONFIG_BYTES.saturating_sub(budget.bytes);
    let text = read_limited(path, remaining, "the ssh_config file").map_err(|e| match e {
        BackendError::InvalidRequest(why) if why.contains("larger than") => {
            BackendError::InvalidRequest(format!("the ssh_config files together are larger than {MAX_CONFIG_BYTES} bytes"))
        }
        other => other,
    })?;
    budget.bytes = budget.bytes.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));
    for line in text.lines() {
        match include_args(line) {
            None => {
                out.push_str(line);
                out.push('\n');
            }
            Some(patterns) => {
                for pattern in patterns {
                    for included in include_paths(&pattern)? {
                        expand(&included, depth.saturating_add(1), budget, out)?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// The arguments of an `Include` line, or `None` for any other line. The keyword is found exactly
/// as ssh2-config finds it (first token before whitespace or `=`, case-insensitive, comments
/// outside quotes stripped), so no include can slip through to it.
fn include_args(line: &str) -> Option<Vec<String>> {
    let mut stripped = String::new();
    let mut in_quotes = false;
    for c in line.trim().chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                stripped.push(c);
            }
            '#' if !in_quotes => break,
            _ => stripped.push(c),
        }
    }
    let trimmed = stripped.trim();
    let split = trimmed.find(|c: char| c == '=' || c.is_whitespace()).unwrap_or(trimmed.len());
    let (field, rest) = trimmed.split_at(split);
    if field.to_lowercase() != "include" {
        return None;
    }
    let rest = rest.trim().trim_start_matches('=').trim();
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in rest.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    Some(args)
}

/// The files an `Include` pattern names: `~/` is the home directory, a relative path is relative
/// to `~/.ssh` (as OpenSSH does for a user's config), glob patterns are expanded (sorted). A
/// pattern that matches nothing is ignored, as OpenSSH does.
fn include_paths(pattern: &str) -> Result<Vec<PathBuf>, BackendError> {
    let expanded = if let Some(rest) = pattern.strip_prefix("~/") {
        home().ok_or_else(|| BackendError::InvalidRequest("no home directory for an ssh_config `Include`".into()))?.join(rest)
    } else if Path::new(pattern).is_absolute() {
        PathBuf::from(pattern)
    } else {
        home().ok_or_else(|| BackendError::InvalidRequest("no home directory for an ssh_config `Include`".into()))?.join(".ssh").join(pattern)
    };
    let text = expanded.to_string_lossy().into_owned();
    let paths = glob::glob(&text).map_err(|e| BackendError::InvalidRequest(format!("a bad ssh_config `Include` pattern ({e})")))?;
    let mut found: Vec<PathBuf> = paths.filter_map(Result::ok).filter(|p| p.is_file()).collect();
    found.sort();
    if found.len() > MAX_CONFIG_FILES {
        return Err(BackendError::InvalidRequest(format!("an ssh_config `Include` matches more than {MAX_CONFIG_FILES} files")));
    }
    Ok(found)
}

pub(crate) fn resolve(target: &SshTarget) -> Result<Resolved, BackendError> {
    let resolved = match &target.source {
        TargetSource::Direct => Resolved {
            host: target.host.clone(),
            port: target.port.unwrap_or(22),
            user: target.user.clone().unwrap_or_default(),
            identity_files: Vec::new(),
            connect_timeout: None,
        },
        TargetSource::Config(path) => {
            let path = match path {
                Some(path) => path.clone(),
                None => home()
                    .map(|home| home.join(".ssh").join("config"))
                    .ok_or_else(|| BackendError::InvalidRequest("no home directory to find ~/.ssh/config in".into()))?,
            };
            let mut budget = Budget::default();
            let mut text = String::new();
            expand(&path, 0, &mut budget, &mut text)?;
            let config = SshConfig::default()
                .parse(&mut text.as_bytes(), ParseRule::ALLOW_UNKNOWN_FIELDS | ParseRule::ALLOW_UNSUPPORTED_FIELDS)
                .map_err(|e| BackendError::InvalidRequest(format!("the ssh_config file `{}` could not be parsed: {e}", file_name(&path))))?;
            let params = config.query(&target.host);
            let user = target.user.clone().or(params.user).ok_or_else(|| {
                BackendError::InvalidRequest(format!("no user for `{}`: set `User` in the ssh_config file or call SshTarget::with_user", target.host))
            })?;
            Resolved {
                host: params.host_name.unwrap_or_else(|| target.host.clone()),
                port: target.port.or(params.port).unwrap_or(22),
                user,
                identity_files: params.identity_file.unwrap_or_default(),
                connect_timeout: params.connect_timeout,
            }
        }
    };
    check_host(&resolved.host)?;
    check_user(&resolved.user)?;
    if resolved.port == 0 {
        return Err(BackendError::InvalidRequest("the SSH port must not be 0".into()));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::resolve;
    use crate::ssh::SshTarget;

    fn config_file(name: &str, text: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("tmp").join("ssh-config-tests");
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap_or_else(|e| panic!("{e}"));
        path
    }

    #[test]
    fn an_alias_takes_host_port_user_and_identity_files_from_the_file() {
        let path = config_file(
            "config-a",
            "Host build\n  HostName build.example.com\n  Port 2222\n  User deploy\n  IdentityFile /keys/id_test\n  ConnectTimeout 5\n\nHost *\n  User nobody\n",
        );
        let resolved = resolve(&SshTarget::from_ssh_config_file(&path, "build")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((resolved.host.as_str(), resolved.port, resolved.user.as_str()), ("build.example.com", 2222, "deploy"));
        assert_eq!(resolved.identity_files, vec![PathBuf::from("/keys/id_test")]);
        assert_eq!(resolved.connect_timeout, Some(Duration::from_secs(5)));
        // Settings given in code win.
        let resolved = resolve(&SshTarget::from_ssh_config_file(&path, "build").with_port(22).with_user("admin")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((resolved.port, resolved.user.as_str()), (22, "admin"));
        // An unknown alias falls back to `Host *` and the alias as host name.
        let resolved = resolve(&SshTarget::from_ssh_config_file(&path, "other.example.com")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((resolved.host.as_str(), resolved.user.as_str()), ("other.example.com", "nobody"));
    }

    #[test]
    fn includes_are_expanded_with_limits_and_loops_are_errors() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("tmp").join("ssh-config-tests");
        let fwd = |p: &PathBuf| p.to_string_lossy().replace('\\', "/");
        // An include with host settings, found through a glob, keyword spelled oddly.
        let inc = config_file("inc-hosts.conf", "Host build\n  HostName included.example.com\n  User deploy\n");
        let main = config_file("config-inc", &format!("INCLUDE=\"{}\"  # comment\nHost *\n  Port 2200\n", fwd(&dir.join("inc-host*.conf"))));
        let resolved = resolve(&SshTarget::from_ssh_config_file(&main, "build")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((resolved.host.as_str(), resolved.user.as_str(), resolved.port), ("included.example.com", "deploy", 2200));
        let _ = inc;
        // A file that includes itself: an error, not a stack overflow.
        let looped = dir.join("config-loop");
        std::fs::write(&looped, format!("Include {}\nHost x\n  User u\n", fwd(&looped))).unwrap_or_else(|e| panic!("{e}"));
        let error = resolve(&SshTarget::from_ssh_config_file(&looped, "x")).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(error.contains("deeper than 16"), "{error}");
        // Two files including each other.
        let (a, b) = (dir.join("config-a-b"), dir.join("config-b-a"));
        std::fs::write(&a, format!("include {}\n", fwd(&b))).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(&b, format!("  Include\t{}\n", fwd(&a))).unwrap_or_else(|e| panic!("{e}"));
        assert!(resolve(&SshTarget::from_ssh_config_file(&a, "x")).is_err());
        // The 1 MiB budget counts included files too.
        let big = config_file("config-big-part", &"# filler line\n".repeat(50_000));
        let top = config_file("config-big", &format!("Include {}\nInclude {}\nHost x\n  User u\n", fwd(&big), fwd(&big)));
        let error = resolve(&SshTarget::from_ssh_config_file(&top, "x")).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(error.contains("together are larger"), "{error}");
        // A pattern that matches nothing is ignored.
        let none = config_file("config-none", &format!("Include {}\nHost x\n  User u\n", fwd(&dir.join("no-such-*.conf"))));
        assert!(resolve(&SshTarget::from_ssh_config_file(&none, "x")).is_ok());
    }

    #[test]
    fn bad_config_files_are_errors_not_panics() {
        for (name, text) in
            [("config-b", "Host\n"), ("config-c", "Port notanumber\n"), ("config-d", "\u{0}\u{1}\u{2}"), ("config-e", "Host x\n  HostName bad host\n")]
        {
            let path = config_file(name, text);
            let _ = resolve(&SshTarget::from_ssh_config_file(&path, "x"));
        }
        let missing = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("tmp").join("no-such-ssh-config");
        assert!(resolve(&SshTarget::from_ssh_config_file(missing, "x")).is_err());
    }
}
