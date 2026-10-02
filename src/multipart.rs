//! `multipart/form-data` uploads (feature `http`): [`Multipart`], a per-request form builder
//! encoded per RFC 7578 with the escaping of names that browsers use (WHATWG HTML
//! "multipart/form-data encoding algorithm"). Unlike a browser it never rewrites line breaks in
//! names or values. How real backends read a form (and where they differ) is in the README's
//! "File uploads" section.

use std::fmt;
use std::path::PathBuf;

use http::header::HeaderValue;

use crate::body::{Segment, StreamingBody};
use crate::response::BackendError;

/// Default limit of one encoded form (the whole request body): 32 MiB.
pub const DEFAULT_MULTIPART_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Default limit of parts in one form.
pub const DEFAULT_MULTIPART_MAX_PARTS: usize = 256;
/// The most parts [`Multipart::with_max_parts`] allows.
const MAX_PARTS_CEILING: usize = 10_000;
/// How many random boundaries are tried before giving up (a collision needs content that contains
/// 128 random bits it cannot know; this is a formality).
const BOUNDARY_TRIES: usize = 4;

#[derive(Clone)]
struct Part {
    name: String,
    /// `Some` for a file part (may be empty; see [`Multipart::file`]).
    filename: Option<String>,
    content_type: Option<String>,
    data: Data,
}

/// A part's content: in memory, or a local file read while the request is sent.
#[derive(Clone)]
enum Data {
    Bytes(Vec<u8>),
    File(PathBuf),
}

impl Data {
    /// The bytes in memory (none for a file read later).
    fn bytes(&self) -> &[u8] {
        match self {
            Data::Bytes(bytes) => bytes,
            Data::File(_) => &[],
        }
    }
}

/// An encoded form: the whole body in memory, or (with file parts read from disk) a body that is
/// read while it is sent.
pub(crate) enum Encoded {
    Memory { content_type: String, body: Vec<u8> },
    Stream { content_type: String, body: StreamingBody },
}

/// A `multipart/form-data` form: text fields and files, in order. Build it, then send it with
/// [`HttpClient::post_multipart`](crate::HttpClient::post_multipart) (or `post_multipart_json`,
/// `send_multipart`, [`OutgoingRequest::with_multipart`](crate::OutgoingRequest::with_multipart)).
///
/// ```
/// use bevy_net_backend::Multipart;
///
/// # let png_bytes: Vec<u8> = vec![0x89, b'P', b'N', b'G'];
/// let form = Multipart::new()
///     .text("display_name", "Ayla")
///     .text("tags[]", "ranger") // repeated names are fine: PHP / Laravel read `tags[]` as an array
///     .text("tags[]", "elf")
///     .file("avatar", "avatar.png", "image/png", png_bytes);
/// assert_eq!(form.len(), 4);
/// ```
///
/// Two ways to add a file:
/// - [`file`](Self::file) takes the bytes. Encoding copies them once and scans them for the
///   boundary, on the thread that sends the request (usually the main thread; measured about 4 ms
///   per 10 MiB in a release build on a desktop PC). Memory: while the request is being sent, the
///   form and its encoded body both exist, so the peak is about twice the form.
/// - [`file_from_path`](Self::file_from_path) takes a path. The file is opened, measured and read
///   on the HTTP worker thread while the request is sent, in small pieces: it is never loaded
///   into memory as a whole and the main thread does no file I/O. A form with such a part is sent
///   with its exact `Content-Length`.
///
/// A non-file part with its own content type: [`part`](Self::part), or `json` (feature `json`)
/// for a JSON value (`Content-Type: application/json`, the way Spring's `@RequestPart` reads it).
///
/// Upload progress: a request with a form reports [`HttpProgress`](crate::HttpProgress) messages
/// (see [`OutgoingRequest::with_upload_progress`](crate::OutgoingRequest::with_upload_progress)).
///
/// Invalid input never panics: the request is answered with an error and never sent
/// (`InvalidRequest`: an empty field name, a name or file name that ends with a backslash or
/// contains a control character other than CR / LF, a content type that is not a valid header
/// value, more parts than [`with_max_parts`](Self::with_max_parts), a file from
/// [`file_from_path`](Self::file_from_path) that cannot be opened; `RequestTooLarge`: a body over
/// [`with_max_bytes`](Self::with_max_bytes); `Encode`: a `json` value that cannot be serialized).
/// `Debug` shows field names and sizes, never values, file names, paths or file contents.
#[derive(Clone)]
pub struct Multipart {
    parts: Vec<Part>,
    max_bytes: u64,
    max_parts: usize,
    error: Option<BackendError>,
}

impl Default for Multipart {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Multipart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // In-memory bytes as a size; a file read from disk as `None`.
        let size = |data: &Data| match data {
            Data::Bytes(bytes) => Some(bytes.len()),
            Data::File(_) => None,
        };
        let parts: Vec<(&str, bool, Option<usize>)> = self.parts.iter().map(|p| (p.name.as_str(), p.filename.is_some(), size(&p.data))).collect();
        f.debug_struct("Multipart").field("parts_name_isfile_bytes", &parts).field("max_bytes", &self.max_bytes).field("max_parts", &self.max_parts).finish()
    }
}

impl Multipart {
    /// An empty form (limits: [`DEFAULT_MULTIPART_MAX_BYTES`], [`DEFAULT_MULTIPART_MAX_PARTS`]).
    pub fn new() -> Self {
        Self { parts: Vec::new(), max_bytes: DEFAULT_MULTIPART_MAX_BYTES, max_parts: DEFAULT_MULTIPART_MAX_PARTS, error: None }
    }

    fn fail(&mut self, why: String) {
        if self.error.is_none() {
            self.error = Some(BackendError::InvalidRequest(why));
        }
    }

    fn check_name(&mut self, name: &str) -> bool {
        if name.is_empty() {
            self.fail("a multipart field name is empty".into());
            return false;
        }
        match unsafe_name(name) {
            Some(why) => {
                self.fail(format!("a multipart field name {why}"));
                false
            }
            None => true,
        }
    }

    /// A text field (no `Content-Type`, like a browser's `<input type="text">`). The value is sent
    /// as UTF-8, exactly as given (line breaks are not rewritten).
    pub fn text(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let name = name.into();
        if self.check_name(&name) {
            self.parts.push(Part { name, filename: None, content_type: None, data: Data::Bytes(value.into().into_bytes()) });
        }
        self
    }

    /// A file field: `filename` (what the server sees as the original name), `content_type` (e.g.
    /// `image/png`; an empty string means `application/octet-stream`) and the bytes.
    ///
    /// Pass a plain file name (`avatar.png`), not a path: servers disagree about `\` and `/`
    /// inside a file name (some keep them, some keep only the last segment). A file name that
    /// ends with `\` is refused (`InvalidRequest`): every tested server loses such a part. An
    /// empty file name is sent as `filename=""`, but several servers (multer, Go, Django) then
    /// treat the part as a text field.
    pub fn file(self, name: impl Into<String>, filename: impl Into<String>, content_type: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        self.file_part(name.into(), filename.into(), content_type.into(), Data::Bytes(data.into()))
    }

    /// A file field read from the local file `path` while the request is sent (see
    /// [`file`](Self::file) for `filename` and `content_type`; the path itself is never sent).
    /// The file is opened on the HTTP worker thread when the request goes out and streamed in
    /// small pieces; its size counts against [`with_max_bytes`](Self::with_max_bytes) then. A file
    /// that cannot be opened answers the request `InvalidRequest` and nothing is sent; a file that
    /// changes size while it is sent cuts the request off with an error.
    ///
    /// The file's bytes are not scanned for the boundary (128 random bits, like a browser's
    /// boundary). It needs a transport that streams bodies (the built-in one does).
    pub fn file_from_path(self, name: impl Into<String>, filename: impl Into<String>, content_type: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        self.file_part(name.into(), filename.into(), content_type.into(), Data::File(path.into()))
    }

    fn file_part(mut self, name: String, filename: String, content_type: String, data: Data) -> Self {
        if !self.check_name(&name) {
            return self;
        }
        if let Some(why) = unsafe_name(&filename) {
            self.fail(format!("the file name of multipart field `{name}` {why}"));
            return self;
        }
        let content_type = if content_type.is_empty() { "application/octet-stream".to_string() } else { content_type };
        if !self.check_content_type(&name, &content_type) {
            return self;
        }
        self.parts.push(Part { name, filename: Some(filename), content_type: Some(content_type), data });
        self
    }

    /// A non-file field with its own `Content-Type` (no file name), for servers that read a part
    /// by its type, such as Spring's `@RequestPart`. An empty content type sends none (like
    /// [`text`](Self::text), with any bytes). Frameworks that read forms by name treat a part
    /// without a file name as a text field and give you its value as text.
    pub fn part(mut self, name: impl Into<String>, content_type: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        let name = name.into();
        if !self.check_name(&name) {
            return self;
        }
        let content_type = content_type.into();
        if !content_type.is_empty() && !self.check_content_type(&name, &content_type) {
            return self;
        }
        let content_type = (!content_type.is_empty()).then_some(content_type);
        self.parts.push(Part { name, filename: None, content_type, data: Data::Bytes(data.into()) });
        self
    }

    /// A JSON field (feature `json`): `value` serialized, with `Content-Type: application/json`
    /// and no file name (see [`part`](Self::part)). A value that cannot be serialized answers the
    /// request `Encode` and nothing is sent.
    #[cfg(feature = "json")]
    #[cfg_attr(docsrs, doc(cfg(feature = "json")))]
    pub fn json<T: serde::Serialize + ?Sized>(mut self, name: impl Into<String>, value: &T) -> Self {
        match serde_json::to_vec(value) {
            Ok(bytes) => self.part(name, "application/json", bytes),
            Err(e) => {
                if self.error.is_none() {
                    self.error = Some(BackendError::Encode(e.to_string()));
                }
                self
            }
        }
    }

    fn check_content_type(&mut self, name: &str, content_type: &str) -> bool {
        if HeaderValue::try_from(content_type).is_err() || content_type.contains(['\r', '\n']) {
            self.fail(format!("the content type of multipart field `{name}` is not a valid header value"));
            return false;
        }
        true
    }

    /// The largest encoded form (the whole request body, part headers and boundaries included)
    /// that may be sent (default 32 MiB, at least 1 KiB): with the default, a single file of
    /// exactly 32 MiB is just over it. A bigger form is answered `RequestTooLarge` and never sent
    /// (with [`file_from_path`](Self::file_from_path) parts it is checked when the files are
    /// opened, still before anything is sent). Raise it for big files read from disk: they are
    /// not held in memory.
    /// Check the server's own limits too (PHP `upload_max_filesize` / `post_max_size`, nginx
    /// `client_max_body_size`, …): not every server answers `413` over them (the README lists
    /// what real servers do).
    pub fn with_max_bytes(mut self, bytes: u64) -> Self {
        self.max_bytes = bytes.max(1024);
        self
    }

    /// The most parts (default 256, 1..=10 000).
    pub fn with_max_parts(mut self, parts: usize) -> Self {
        self.max_parts = parts.clamp(1, MAX_PARTS_CEILING);
        self
    }

    /// How many parts the form has.
    pub fn len(&self) -> usize {
        self.parts.len()
    }

    /// Whether the form has no parts.
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// The encoded size in bytes (the request body), whatever boundary is chosen. Files added
    /// with [`file_from_path`](Self::file_from_path) count with 0 bytes here (they are measured
    /// when the request is sent).
    pub fn encoded_len(&self) -> u64 {
        encoded_len(&self.parts, BOUNDARY_LEN)
    }

    /// Whether a part is a file read from disk (the body is then streamed).
    fn has_files(&self) -> bool {
        self.parts.iter().any(|p| matches!(p.data, Data::File(_)))
    }

    /// Encode with a fresh random boundary.
    pub(crate) fn encode(&self) -> Result<Encoded, BackendError> {
        self.check()?;
        for _ in 0..BOUNDARY_TRIES {
            let boundary = random_boundary()?;
            if !self.parts.iter().any(|p| contains(p.data.bytes(), boundary.as_bytes()) || part_head(p).contains(&boundary)) {
                let content_type = format!("multipart/form-data; boundary={boundary}");
                return Ok(if self.has_files() {
                    Encoded::Stream { content_type, body: StreamingBody::new(self.segments_with(&boundary), self.max_bytes) }
                } else {
                    Encoded::Memory { content_type, body: self.encode_with(&boundary).1 }
                });
            }
        }
        Err(BackendError::InvalidRequest("could not find a multipart boundary that does not occur in the content".into()))
    }

    /// The body as pieces: bytes in memory (merged) and the files read while sending.
    pub(crate) fn segments_with(&self, boundary: &str) -> Vec<Segment> {
        let mut segments = Vec::new();
        let mut bytes = Vec::new();
        for part in &self.parts {
            bytes.extend_from_slice(b"--");
            bytes.extend_from_slice(boundary.as_bytes());
            bytes.extend_from_slice(b"\r\n");
            bytes.extend_from_slice(part_head(part).as_bytes());
            match &part.data {
                Data::Bytes(data) => bytes.extend_from_slice(data),
                Data::File(path) => {
                    segments.push(Segment::Bytes(std::mem::take(&mut bytes)));
                    segments.push(Segment::File { path: path.clone(), field: part.name.clone() });
                }
            }
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(b"--");
        bytes.extend_from_slice(boundary.as_bytes());
        bytes.extend_from_slice(b"--\r\n");
        segments.push(Segment::Bytes(bytes));
        segments
    }

    fn check(&self) -> Result<(), BackendError> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        if self.parts.len() > self.max_parts {
            return Err(BackendError::InvalidRequest(format!("the form has {} parts, more than the limit of {}", self.parts.len(), self.max_parts)));
        }
        let size = self.encoded_len();
        if size > self.max_bytes {
            return Err(BackendError::RequestTooLarge { limit: self.max_bytes, size });
        }
        Ok(())
    }

    /// Encode with a given boundary (tests; the boundary must not occur in the content).
    pub(crate) fn encode_with(&self, boundary: &str) -> (String, Vec<u8>) {
        let size = usize::try_from(encoded_len(&self.parts, boundary.len())).unwrap_or(0);
        let mut body = Vec::with_capacity(size);
        for part in &self.parts {
            body.extend_from_slice(b"--");
            body.extend_from_slice(boundary.as_bytes());
            body.extend_from_slice(b"\r\n");
            body.extend_from_slice(part_head(part).as_bytes());
            body.extend_from_slice(part.data.bytes());
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(b"--");
        body.extend_from_slice(boundary.as_bytes());
        body.extend_from_slice(b"--\r\n");
        (format!("multipart/form-data; boundary={boundary}"), body)
    }
}

/// `bnb-` + 32 lowercase hex characters.
const BOUNDARY_LEN: usize = 36;

/// 128 bits from the operating system (through ring, the crypto provider `http` already uses).
fn random_boundary() -> Result<String, BackendError> {
    let mut bytes = [0u8; 16];
    crate::tls::random_bytes(&mut bytes).map_err(|()| BackendError::InvalidRequest("no randomness for the multipart boundary".into()))?;
    let mut boundary = String::with_capacity(BOUNDARY_LEN);
    boundary.push_str("bnb-");
    for byte in bytes {
        boundary.push_str(&format!("{byte:02x}"));
    }
    Ok(boundary)
}

/// Why a field name or file name cannot be sent safely, if it cannot. A trailing backslash turns
/// the closing quote into an escaped quote for most parsers (live: the part is dropped or becomes a
/// text field on PHP, multer, FastAPI, Go and Django). A control character other than CR / LF
/// (which are escaped) is cut or refused by some parsers (PHP stops at NUL).
fn unsafe_name(value: &str) -> Option<&'static str> {
    if value.ends_with('\\') {
        return Some("ends with a backslash (servers would misread the part)");
    }
    if value.chars().any(|c| c.is_control() && c != '\r' && c != '\n') {
        return Some("contains a control character");
    }
    None
}

/// A field name or file name for a `Content-Disposition` quoted string, as browsers write it
/// (WHATWG HTML): `"` → `%22`, CR → `%0D`, LF → `%0A`; everything else (backslashes, non-ASCII
/// as UTF-8) as it is.
pub(crate) fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '"' => out.push_str("%22"),
            '\r' => out.push_str("%0D"),
            '\n' => out.push_str("%0A"),
            c => out.push(c),
        }
    }
    out
}

/// A part's headers and the empty line after them.
fn part_head(part: &Part) -> String {
    let mut head = format!("Content-Disposition: form-data; name=\"{}\"", escape(&part.name));
    if let Some(filename) = &part.filename {
        head.push_str(&format!("; filename=\"{}\"", escape(filename)));
    }
    head.push_str("\r\n");
    if let Some(content_type) = &part.content_type {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    head.push_str("\r\n");
    head
}

fn encoded_len(parts: &[Part], boundary_len: usize) -> u64 {
    let mut total: u64 = 0;
    let add = |total: &mut u64, n: usize| *total = total.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
    for part in parts {
        add(&mut total, 2 + boundary_len + 2);
        add(&mut total, part_head(part).len());
        add(&mut total, part.data.bytes().len());
        add(&mut total, 2);
    }
    add(&mut total, 2 + boundary_len + 4);
    total
}

/// Whether `needle` occurs in `haystack` (first-byte scan, then compare).
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    let Some(&first) = needle.first() else { return true };
    if haystack.len() < needle.len() {
        return false;
    }
    let last_start = haystack.len() - needle.len();
    let mut start = 0;
    while start <= last_start {
        match haystack.get(start..=last_start).and_then(|rest| rest.iter().position(|&b| b == first)) {
            None => return false,
            Some(offset) => {
                let at = start + offset;
                if haystack.get(at..at + needle.len()) == Some(needle) {
                    return true;
                }
                start = at + 1;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{contains, escape, Encoded, Multipart, BOUNDARY_LEN};
    use crate::response::BackendError;

    /// An in-memory form encoded with a random boundary: (content type, body).
    fn memory(form: &Multipart) -> (String, Vec<u8>) {
        match form.encode() {
            Ok(Encoded::Memory { content_type, body }) => (content_type, body),
            Ok(Encoded::Stream { .. }) => panic!("streamed"),
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn golden_encoding_byte_for_byte() {
        let form = Multipart::new()
            .text("title", "Hello")
            .text("tags[]", "a")
            .text("tags[]", "b")
            .file("avatar", "a.png", "image/png", vec![0u8, 1, 2])
            .text("empty", "");
        let (content_type, body) = form.encode_with("XyZ");
        assert_eq!(content_type, "multipart/form-data; boundary=XyZ");
        let expected: &[u8] = b"--XyZ\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nHello\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"tags[]\"\r\n\r\na\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"tags[]\"\r\n\r\nb\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"avatar\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n\x00\x01\x02\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"empty\"\r\n\r\n\r\n\
--XyZ--\r\n";
        assert_eq!(body, expected, "\n{}", String::from_utf8_lossy(&body));
        assert_eq!(form.encoded_len() - u64::try_from(BOUNDARY_LEN - 3).unwrap_or(0) * 6, u64::try_from(body.len()).unwrap_or(0));
    }

    #[test]
    fn names_and_filenames_are_escaped_like_browsers() {
        assert_eq!(escape("a\"b\r\nc\\d é"), "a%22b%0D%0Ac\\d é");
        let form = Multipart::new().file("fi\"le", "evil\"\r\nContent-Type: text/html.png", "", b"x".to_vec());
        let (_, body) = form.encode_with("B");
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("name=\"fi%22le\"; filename=\"evil%22%0D%0AContent-Type: text/html.png\"\r\nContent-Type: application/octet-stream\r\n"),
            "{text}"
        );
        assert_eq!(text.matches("\r\nContent-Type:").count(), 1, "a header was injected");
    }

    #[test]
    fn the_random_boundary_is_fresh_valid_and_never_in_the_content() {
        let form = Multipart::new().text("a", "--bnb-").file("f", "x", "", b"--bnb-0000\r\n".to_vec());
        let (ct1, body1) = memory(&form);
        let (ct2, _) = memory(&form);
        assert_ne!(ct1, ct2, "the boundary must be random per request");
        let boundary = ct1.strip_prefix("multipart/form-data; boundary=").unwrap_or_default();
        assert_eq!(boundary.len(), BOUNDARY_LEN);
        assert!(boundary.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
        // Exactly one delimiter per part plus the closing one.
        let delimiter = format!("--{boundary}");
        assert_eq!(String::from_utf8_lossy(&body1).matches(&delimiter).count(), 3);
        assert_eq!(u64::try_from(body1.len()).unwrap_or(0), form.encoded_len());
    }

    #[test]
    fn boundary_search_finds_every_position() {
        assert!(contains(b"abc--XyZ", b"--XyZ"));
        assert!(contains(b"--XyZabc", b"--XyZ"));
        assert!(!contains(b"--XyabcZ", b"--XyZ"));
        assert!(!contains(b"", b"x"));
        assert!(contains(b"---XyZ", b"--XyZ"));
    }

    #[test]
    fn large_binary_and_empty_forms() {
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
        let form = Multipart::new().file("blob", "blob.bin", "", data.clone());
        let (_, body) = memory(&form);
        assert_eq!(u64::try_from(body.len()).unwrap_or(0), form.encoded_len());
        assert!(contains(&body, &data[1000..1100]));
        let (_, empty) = Multipart::new().encode_with("B");
        assert_eq!(empty, b"--B--\r\n");
    }

    #[test]
    fn trailing_backslashes_and_control_characters_are_refused() {
        for form in [
            Multipart::new().text("n\\", "v"),
            Multipart::new().file("f\\", "a.png", "image/png", vec![1u8]),
            Multipart::new().file("up", "x\\", "image/png", vec![1u8]),
            Multipart::new().file("up", "C:\\dir\\", "", vec![1u8]),
            Multipart::new().text("a\u{0}b", "v"),
            Multipart::new().file("up", "a\u{0}.png", "", vec![1u8]),
            Multipart::new().file("up", "tab\there.png", "", vec![1u8]),
            Multipart::new().text("del\u{7f}", "v"),
        ] {
            assert!(matches!(form.encode(), Err(BackendError::InvalidRequest(_))), "{form:?}");
        }
        // A backslash inside a name, CR / LF (escaped) in names, anything in VALUES, an empty file name: fine.
        let ok = Multipart::new().text("a\\b", "line\u{0}\r\n").text("x\r\ny", "v").file("up", "C:\\Users\\me\\a.png", "", vec![0u8]).file("e", "", "", vec![]);
        assert!(ok.encode().is_ok());
        let (_, body) = Multipart::new().file("up", "C:\\Users\\me\\a.png", "", vec![0u8]).encode_with("B");
        assert!(String::from_utf8_lossy(&body).contains("filename=\"C:\\Users\\me\\a.png\""));
        // The first error wins; it names the field, never a value or a file name.
        let error = Multipart::new().file("up", "secret\\", "", vec![]).text("", "").encode().err().map(|e| e.to_string()).unwrap_or_default();
        assert!(error.contains("`up`") && error.contains("backslash") && !error.contains("secret"), "{error}");
    }

    #[test]
    fn limits_and_bad_input_are_errors_not_panics() {
        let big = Multipart::new().with_max_bytes(2048).file("f", "f", "", vec![0u8; 4096]);
        assert!(matches!(big.encode(), Err(BackendError::RequestTooLarge { limit: 2048, size, .. }) if size > 4096));
        let many = (0..5).fold(Multipart::new().with_max_parts(4), |form, i| form.text("x", i.to_string()));
        assert!(matches!(many.encode(), Err(BackendError::InvalidRequest(why)) if why.contains("parts")));
        assert!(matches!(Multipart::new().text("", "v").encode(), Err(BackendError::InvalidRequest(_))));
        assert!(matches!(Multipart::new().file("f", "n", "image/png\r\nX: y", vec![]).encode(), Err(BackendError::InvalidRequest(_))));
        assert!(matches!(Multipart::new().file("f", "n", "bad\u{0}type", vec![]).encode(), Err(BackendError::InvalidRequest(_))));
        assert!(Multipart::new().file("f", "n", String::from("image/png"), vec![]).encode().is_ok());
        let debug = format!("{:?}", Multipart::new().text("token", "fake-secret-3").file("f", "private.txt", "", b"fake-secret-4".to_vec()));
        assert!(!debug.contains("fake-secret") && !debug.contains("private.txt"), "{debug}");
    }
}
