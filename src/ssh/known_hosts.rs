//! The crate's own strict known_hosts check (russh's helper is too weak: single-space splitting, exact
//! host match only, no `@revoked`, TOFU writes). Read-only; built on `ssh_key`'s key parser.
//!
//! Rules (OpenSSH's): host patterns with `*`, `?` and `!` negation (a matching negated pattern
//! makes the line not match), hashed hosts (`|1|salt|hash`, HMAC-SHA1), `[host]:port` for a port
//! other than 22, case-insensitive host names, fields split on any whitespace. `@revoked` keys are
//! refused even when pinned; `@cert-authority` lines are ignored (host certificates are not
//! supported). A line whose key cannot be read is skipped, except an `@revoked` line for this host
//! (then the host is refused: fail closed).

use hmac::{Hmac, KeyInit, Mac};
use russh::keys::ssh_key::known_hosts::{HostPatterns, Marker};
use russh::keys::ssh_key::{Algorithm, HashAlg, PublicKey};
use sha1::Sha1;

use crate::response::{BackendError, HostKeyProblem};

/// The largest known_hosts file read (OpenSSH has no limit; this one keeps memory bounded).
pub(crate) const MAX_KNOWN_HOSTS_BYTES: u64 = 4 * 1024 * 1024;
/// Lines longer than this are skipped (no key is that long; RSA 16384 is about 2.8 KB).
const MAX_LINE: usize = 16 * 1024;

/// The name a host is looked up by: `host` for port 22, `[host]:port` otherwise; lowercase.
pub(crate) fn lookup_name(host: &str, port: u16) -> String {
    let host = host.to_ascii_lowercase();
    if port == 22 {
        host
    } else {
        format!("[{host}]:{port}")
    }
}

struct Line {
    marker: Option<Marker>,
    hosts: HostPatterns,
    /// `None` when the key could not be read.
    key: Option<PublicKey>,
}

/// The parsed lines of one or more known_hosts files.
#[derive(Default)]
pub(crate) struct KnownHosts {
    lines: Vec<Line>,
}

impl KnownHosts {
    /// Add the lines of one file's text. Lines that cannot be parsed at all are skipped.
    pub(crate) fn add(&mut self, text: &str) {
        for raw in text.lines() {
            if raw.len() > MAX_LINE {
                continue;
            }
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut fields = line.split_whitespace();
            let Some(first) = fields.next() else { continue };
            let (marker, hosts) = if first.starts_with('@') {
                match (first.parse::<Marker>(), fields.next()) {
                    (Ok(marker), Some(hosts)) => (Some(marker), hosts),
                    // An unknown marker: OpenSSH skips the line.
                    _ => continue,
                }
            } else {
                (None, first)
            };
            let Ok(hosts) = hosts.parse::<HostPatterns>() else { continue };
            let key = match (fields.next(), fields.next()) {
                (Some(kind), Some(base64)) => PublicKey::from_openssh(&format!("{kind} {base64}")).ok(),
                _ => None,
            };
            self.lines.push(Line { marker, hosts, key });
        }
    }

    /// Check `key` for the host looked up as `name` (see [`lookup_name`]) against these lines and
    /// the pinned fingerprints. `Ok` holds the key's fingerprint.
    pub(crate) fn verify(&self, name: &str, key: &PublicKey, pinned: &[String]) -> Result<String, BackendError> {
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let refuse = |problem| BackendError::HostKey { host: name.to_string(), fingerprint: fingerprint.clone(), problem };
        let matching: Vec<&Line> = self.lines.iter().filter(|line| host_matches(&line.hosts, name)).collect();
        // Revocation first: it wins over everything, pins included.
        for line in matching.iter().filter(|l| l.marker == Some(Marker::Revoked)) {
            match &line.key {
                Some(revoked) if revoked.key_data() == key.key_data() => return Err(refuse(HostKeyProblem::Revoked)),
                Some(_) => {}
                None => return Err(refuse(HostKeyProblem::Revoked)),
            }
        }
        if pinned.iter().any(|pin| pin == &fingerprint) {
            return Ok(fingerprint);
        }
        let plain: Vec<&PublicKey> = matching.iter().filter(|l| l.marker.is_none()).filter_map(|l| l.key.as_ref()).collect();
        if plain.iter().any(|known| known.key_data() == key.key_data()) {
            return Ok(fingerprint);
        }
        // Only a key of the SAME type that differs is a change (as OpenSSH): a host known with an
        // RSA key that now shows an ed25519 key is unknown for that type, not changed.
        if plain.iter().any(|known| same_family(&known.algorithm(), &key.algorithm())) {
            Err(refuse(HostKeyProblem::Changed))
        } else {
            Err(refuse(HostKeyProblem::Unknown))
        }
    }

    /// The key algorithms known for `name` (plain lines, file order, no duplicates): the client
    /// prefers these, so a host listed with one key type is asked for that type (OpenSSH's
    /// `order_hostkeyalgs`).
    pub(crate) fn known_algorithms(&self, name: &str) -> Vec<Algorithm> {
        let mut known: Vec<Algorithm> = Vec::new();
        for line in self.lines.iter().filter(|l| l.marker.is_none() && host_matches(&l.hosts, name)) {
            if let Some(key) = &line.key {
                let algorithm = key.algorithm();
                if !known.iter().any(|k| same_family(k, &algorithm)) {
                    known.push(algorithm);
                }
            }
        }
        known
    }
}

/// The same key type (every RSA hash counts as RSA).
pub(crate) fn same_family(a: &Algorithm, b: &Algorithm) -> bool {
    match (a, b) {
        (Algorithm::Rsa { .. }, Algorithm::Rsa { .. }) => true,
        (a, b) => a == b,
    }
}

fn host_matches(hosts: &HostPatterns, name: &str) -> bool {
    match hosts {
        HostPatterns::HashedName { salt, hash } => {
            let Ok(mut mac) = <Hmac<Sha1> as KeyInit>::new_from_slice(salt) else { return false };
            mac.update(name.as_bytes());
            mac.verify_slice(hash).is_ok()
        }
        HostPatterns::Patterns(patterns) => {
            let mut positive = false;
            for pattern in patterns {
                let pattern = pattern.to_ascii_lowercase();
                match pattern.strip_prefix('!') {
                    Some(negated) if glob(negated, name) => return false,
                    Some(_) => {}
                    None => positive |= glob(&pattern, name),
                }
            }
            positive
        }
    }
}

/// `*` (any run) and `?` (one character) wildcard match, iterative (no exponential backtracking).
fn glob(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, ti));
                pi += 1;
            }
            Some(&c) if c == '?' || Some(&c) == t.get(ti) => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    pi = sp + 1;
                    ti = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    p.get(pi..).is_some_and(|rest| rest.iter().all(|c| *c == '*'))
}

#[cfg(test)]
mod tests {
    use hmac::{Hmac, KeyInit, Mac};
    use russh::keys::ssh_key::private::Ed25519Keypair;
    use russh::keys::ssh_key::{HashAlg, LineEnding, PrivateKey, PublicKey};
    use sha1::Sha1;

    use super::{glob, lookup_name, KnownHosts};
    use crate::response::{BackendError, HostKeyProblem};

    fn key(seed: u8) -> PublicKey {
        PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32])).public_key().clone()
    }

    fn openssh(key: &PublicKey) -> String {
        key.to_openssh().unwrap_or_else(|e| panic!("{e}"))
    }

    fn known(text: &str) -> KnownHosts {
        let mut known = KnownHosts::default();
        known.add(text);
        known
    }

    fn problem(result: Result<String, BackendError>) -> Option<HostKeyProblem> {
        match result {
            Err(BackendError::HostKey { problem, .. }) => Some(problem),
            Err(other) => panic!("unexpected error {other:?}"),
            Ok(_) => None,
        }
    }

    #[test]
    fn globs() {
        assert!(glob("*.example.com", "a.example.com"));
        assert!(!glob("*.example.com", "example.com"));
        assert!(glob("h?st", "host"));
        assert!(glob("*", "anything"));
        assert!(glob("[*]:2222", "[x]:2222"));
        assert!(!glob("a*b*c", "aXbY"));
        assert!(glob("a*b*c", "aXbYc"));
        // Many stars against a long name stays fast.
        assert!(!glob(&"*a".repeat(200), &"a".repeat(150)));
    }

    #[test]
    fn lookup_names() {
        assert_eq!(lookup_name("Host.Example.COM", 22), "host.example.com");
        assert_eq!(lookup_name("10.0.0.1", 2222), "[10.0.0.1]:2222");
    }

    #[test]
    fn plain_patterns_ports_and_whitespace() {
        let (server, other) = (key(1), key(2));
        let text = format!("# comment\n\nother.example.com {}\nhost.example.com,alias\t{}  trailing comment\n", openssh(&other), openssh(&server));
        let known = known(&text);
        assert_eq!(known.verify("host.example.com", &server, &[]).ok(), Some(server.fingerprint(HashAlg::Sha256).to_string()));
        assert_eq!(problem(known.verify("alias", &server, &[])), None);
        assert_eq!(problem(known.verify("unknown.example.com", &server, &[])), Some(HostKeyProblem::Unknown));
        // Port 2222 is a different name.
        assert_eq!(problem(known.verify("[host.example.com]:2222", &server, &[])), Some(HostKeyProblem::Unknown));
        let known = known_with_port(&server);
        assert_eq!(problem(known.verify("[host.example.com]:2222", &server, &[])), None);
    }

    fn known_with_port(server: &PublicKey) -> KnownHosts {
        known(&format!("[host.example.com]:2222 {}\n", openssh(server)))
    }

    #[test]
    fn a_different_key_for_a_known_host_is_changed() {
        let known = known(&format!("host.example.com {}\n", openssh(&key(1))));
        assert_eq!(problem(known.verify("host.example.com", &key(9), &[])), Some(HostKeyProblem::Changed));
    }

    #[test]
    fn another_key_type_for_a_known_host_is_unknown_not_changed() {
        use russh::keys::ssh_key::rand_core::UnwrapErr;
        use russh::keys::{Algorithm, EcdsaCurve};
        let ecdsa = PrivateKey::random(&mut UnwrapErr(getrandom::SysRng), Algorithm::Ecdsa { curve: EcdsaCurve::NistP256 }).unwrap_or_else(|e| panic!("{e}"));
        let known = known(&format!("host.example.com {}\n", openssh(ecdsa.public_key())));
        // The server shows an ed25519 key: a different TYPE, so unknown (no false "CHANGED").
        assert_eq!(problem(known.verify("host.example.com", &key(1), &[])), Some(HostKeyProblem::Unknown));
        assert_eq!(known.known_algorithms("host.example.com"), vec![ecdsa.algorithm()]);
        assert!(known.known_algorithms("other.example.com").is_empty());
        // A different ECDSA key for the same host IS a change.
        let other = PrivateKey::random(&mut UnwrapErr(getrandom::SysRng), Algorithm::Ecdsa { curve: EcdsaCurve::NistP256 }).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(problem(known.verify("host.example.com", other.public_key(), &[])), Some(HostKeyProblem::Changed));
    }

    #[test]
    fn wildcards_and_negation() {
        let server = key(3);
        let known = known(&format!("*.example.com,!secret.example.com {}\n", openssh(&server)));
        assert_eq!(problem(known.verify("a.example.com", &server, &[])), None);
        assert_eq!(problem(known.verify("secret.example.com", &server, &[])), Some(HostKeyProblem::Unknown));
        assert_eq!(problem(known.verify("example.org", &server, &[])), Some(HostKeyProblem::Unknown));
    }

    #[test]
    fn hashed_hosts() {
        let server = key(4);
        let salt = [7u8; 20];
        let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(&salt).unwrap_or_else(|e| panic!("{e}"));
        mac.update(b"hashed.example.com");
        let hash = mac.finalize().into_bytes();
        let b64 = |bytes: &[u8]| {
            use russh::keys::ssh_key::known_hosts::HostPatterns;
            HostPatterns::HashedName { salt: salt.to_vec(), hash: bytes.try_into().unwrap_or([0; 20]) }.to_string()
        };
        let known = known(&format!("{} {}\n", b64(&hash), openssh(&server)));
        assert_eq!(problem(known.verify("hashed.example.com", &server, &[])), None);
        assert_eq!(problem(known.verify("other.example.com", &server, &[])), Some(HostKeyProblem::Unknown));
    }

    #[test]
    fn revoked_wins_over_known_and_pinned() {
        let server = key(5);
        let fingerprint = server.fingerprint(HashAlg::Sha256).to_string();
        let text = format!("host.example.com {k}\n@revoked * {k}\n", k = openssh(&server));
        let known = known(&text);
        assert_eq!(problem(known.verify("host.example.com", &server, &[fingerprint])), Some(HostKeyProblem::Revoked));
        // A revoked line for another key does not affect this one.
        let known = known_other_revoked(&server);
        assert_eq!(problem(known.verify("host.example.com", &server, &[])), None);
    }

    fn known_other_revoked(server: &PublicKey) -> KnownHosts {
        known(&format!("host.example.com {}\n@revoked * {}\n", openssh(server), openssh(&key(6))))
    }

    #[test]
    fn an_unreadable_revoked_line_for_the_host_fails_closed() {
        let server = key(7);
        let text = format!("host.example.com {}\n@revoked host.example.com ssh-ed25519 not-base64!!\n", openssh(&server));
        assert_eq!(problem(known(&text).verify("host.example.com", &server, &[])), Some(HostKeyProblem::Revoked));
        // ... but an unreadable ordinary line is only skipped.
        let text = format!("host.example.com ssh-ed25519 garbage\nhost.example.com {}\n", openssh(&server));
        assert_eq!(problem(known(&text).verify("host.example.com", &server, &[])), None);
    }

    #[test]
    fn cert_authority_lines_never_vouch_for_a_host() {
        let server = key(8);
        let known = known(&format!("@cert-authority * {}\n", openssh(&server)));
        assert_eq!(problem(known.verify("host.example.com", &server, &[])), Some(HostKeyProblem::Unknown));
    }

    #[test]
    fn a_pinned_fingerprint_is_trusted_without_known_hosts() {
        let server = key(10);
        let pin = server.fingerprint(HashAlg::Sha256).to_string();
        let empty = KnownHosts::default();
        assert_eq!(problem(empty.verify("host.example.com", &server, std::slice::from_ref(&pin))), None);
        assert_eq!(problem(empty.verify("host.example.com", &key(11), &[pin])), Some(HostKeyProblem::Unknown));
    }

    #[test]
    fn the_comment_of_a_key_does_not_matter() {
        let server = key(12);
        let mut commented = server.clone();
        commented.set_comment("admin@laptop");
        let known = known(&format!("host.example.com {}\n", openssh(&commented)));
        assert_eq!(problem(known.verify("host.example.com", &server, &[])), None);
    }

    #[test]
    fn garbage_never_panics() {
        let server = key(13);
        let inputs = [
            "@",
            "@revoked",
            "@unknown host ssh-ed25519 AAAA",
            "|1|",
            "|1|x|y ssh-ed25519 AAAA",
            ",,, ssh-ed25519",
            "host",
            "\u{0}\u{1}",
            "host ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA",
        ];
        for input in inputs {
            let _ = known(input).verify("host", &server, &[]);
        }
        let long = format!("{} {}", "h".repeat(20_000), openssh(&server));
        assert_eq!(problem(known(&long).verify("host", &server, &[])), Some(HostKeyProblem::Unknown));
        let _ = LineEnding::LF;
    }
}
