//! Live multipart checks: `#[ignore]`d; they only run on request, against servers from environment
//! variables (never from a file in this crate):
//!
//! ```text
//! BNB_TEST_HTTPS_URL=https://<test host>            # the mock behind a TLS proxy: <url>/upload
//! BNB_TEST_MULTIPART_URLS=https://<host>/php/upload,https://<host>/node/upload,...   # echo servers
//!   cargo test --test live_multipart -- --ignored --test-threads 1
//! ```
//!
//! Every URL must answer a `multipart/form-data` POST with `200` and this JSON echo (the shape
//! `examples/mock_server.rs` serves):
//! `{"fields":[{"name":…,"value":…}],"files":[{"name":…,"filename":…,"content_type":…,"size":N,"crc32":"8 lowercase hex"}]}`
//! - `fields`: every text part, its name and its value as a string (a framework that groups
//!   repeated names may answer an array of strings as the value);
//! - `files`: every file part: the field name, the file name as the server parsed it, the content
//!   type (or `null`), the size in bytes, and the zlib / IEEE CRC-32 of the bytes as 8 lowercase
//!   hex digits (PHP `hash('crc32b', $data)`, Python `format(zlib.crc32(data), '08x')`, Go
//!   `fmt.Sprintf("%08x", crc32.ChecksumIEEE(b))`).
//!
//! Order within `fields` / `files` does not matter here.

use std::time::Duration;

use bevy::prelude::*;
use bevy_headless_test::prelude::*;
use bevy_net_backend::*;
use serde::Deserialize;

#[derive(Deserialize, Debug)]
struct Echo {
    fields: Vec<Field>,
    files: Vec<FileInfo>,
}

#[derive(Deserialize, Debug)]
struct Field {
    name: String,
    value: serde_json::Value,
}

#[derive(Deserialize, Debug)]
struct FileInfo {
    name: String,
    filename: String,
    content_type: Option<String>,
    size: u64,
    crc32: String,
}

fn crc32(data: &[u8]) -> String {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    format!("{:08x}", crc ^ 0xffff_ffff)
}

/// Every upload URL to test.
fn urls() -> Vec<String> {
    let mut urls = Vec::new();
    if let Ok(base) = std::env::var("BNB_TEST_HTTPS_URL") {
        if !base.trim().is_empty() {
            urls.push(format!("{}/upload", base.trim().trim_end_matches('/')));
        }
    }
    if let Ok(list) = std::env::var("BNB_TEST_MULTIPART_URLS") {
        urls.extend(list.split(',').map(str::trim).filter(|u| !u.is_empty()).map(str::to_string));
    }
    urls
}

/// `https://host[:port]/some/path` → (`https://host[:port]`, `/some/path`).
fn split(url: &str) -> (String, String) {
    let after_scheme = url.find("://").map_or(0, |i| i + 3);
    match url[after_scheme..].find('/') {
        Some(slash) => (url[..after_scheme + slash].to_string(), url[after_scheme + slash..].to_string()),
        None => (url.to_string(), "/".to_string()),
    }
}

/// Send `form` to `url` and read the echo.
fn send(url: &str, form: &Multipart) -> Echo {
    let (base, path) = split(url);
    let mut app = TestApp::builder().frame_duration(Duration::from_millis(1)).real_pause(Duration::from_millis(5)).build();
    app.add_plugins(BackendPlugin::new(HttpConfig::new(base).with_timeout(Duration::from_secs(30))));
    app.watch::<HttpResponse>();
    let id = app.world().resource::<HttpClient>().post_multipart(&path, form);
    let mut answer = None;
    for _ in 0..8000 {
        app.step();
        if let Some(found) = app.messages::<HttpResponse>().into_iter().find(|a| a.id == id) {
            answer = Some(found.result);
            break;
        }
    }
    let response = answer.unwrap_or_else(|| panic!("{url}: no answer")).unwrap_or_else(|e| panic!("{url}: {e}"));
    response.json().unwrap_or_else(|e| panic!("{url}: not the echo shape ({e}): {}", response.text()))
}

fn file<'a>(url: &str, echo: &'a Echo, name: &str) -> &'a FileInfo {
    echo.files.iter().find(|f| f.name == name).unwrap_or_else(|| panic!("{url}: no `{name}` file in {echo:?}"))
}

fn field<'a>(url: &str, echo: &'a Echo, name: &str) -> Option<&'a str> {
    echo.fields.iter().find(|f| f.name == name).map(|f| f.value.as_str().unwrap_or_else(|| panic!("{url}: `{name}` is not a string")))
}

fn upload(url: &str) -> Echo {
    let avatar: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 17) as u8).chain(b"\r\n--boundary-like\r\n".iter().copied()).collect();
    let form = Multipart::new().text("title", "Hello from bevy_net_backend ✓").file("avatar", "avatar.png", "image/png", avatar.clone());
    let echo = send(url, &form);
    let title = echo.fields.iter().find(|f| f.name == "title").unwrap_or_else(|| panic!("{url}: no title field in {echo:?}"));
    assert_eq!(title.value.as_str(), Some("Hello from bevy_net_backend ✓"), "{url}");
    let file = echo.files.iter().find(|f| f.name == "avatar").unwrap_or_else(|| panic!("{url}: no avatar file in {echo:?}"));
    assert_eq!(file.filename, "avatar.png", "{url}");
    assert_eq!(file.content_type.as_deref(), Some("image/png"), "{url}");
    assert_eq!(file.size, u64::try_from(avatar.len()).unwrap_or(0), "{url}");
    assert_eq!(file.crc32, crc32(&avatar), "{url}");
    echo
}

#[test]
#[ignore = "live: needs BNB_TEST_HTTPS_URL and / or BNB_TEST_MULTIPART_URLS"]
fn live_upload_to_every_url() {
    let urls = urls();
    assert!(!urls.is_empty(), "set BNB_TEST_HTTPS_URL and / or BNB_TEST_MULTIPART_URLS");
    for url in urls {
        let echo = upload(&url);
        println!("{url}: ok ({} fields, {} files)", echo.fields.len(), echo.files.len());
    }
}

/// The cases every tested framework (PHP, multer, FastAPI, Go, Django) read the same way: text
/// before and after files, an empty value, CRLF inside a value, a space and a `"` (sent as `%22`)
/// in file names, a 0-byte file, binary with boundary-like lines, repeated `tags[]` (matched by
/// value, since PHP / multer group them), and an empty form.
#[test]
#[ignore = "live: needs BNB_TEST_HTTPS_URL and / or BNB_TEST_MULTIPART_URLS"]
fn live_portable_cases_on_every_url() {
    let urls = urls();
    assert!(!urls.is_empty(), "set BNB_TEST_HTTPS_URL and / or BNB_TEST_MULTIPART_URLS");
    let blob: Vec<u8> = (0..70_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).chain(b"\r\n--\r\n--bnb-\r\n\r\n".iter().copied()).collect();
    let form = Multipart::new()
        .text("first", "one")
        .text("tags[]", "ranger")
        .file("blob", "my file.png", "image/png", blob.clone())
        .text("tags[]", "elf")
        .text("empty", "")
        .text("lines", "line1\r\nline2")
        .file("zero", "zero.bin", "application/octet-stream", Vec::new())
        .file("quoted", "a\"b.png", "image/png", b"q".to_vec())
        .text("after", "last");
    for url in urls {
        let echo = send(&url, &form);
        assert_eq!(field(&url, &echo, "first"), Some("one"), "{url}");
        assert_eq!(field(&url, &echo, "empty"), Some(""), "{url}");
        assert_eq!(field(&url, &echo, "lines"), Some("line1\r\nline2"), "{url}");
        assert_eq!(field(&url, &echo, "after"), Some("last"), "{url}");
        let mut tags: Vec<String> = echo
            .fields
            .iter()
            .filter(|f| f.name.starts_with("tags"))
            .flat_map(|f| match &f.value {
                serde_json::Value::Array(values) => values.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
                value => value.as_str().map(str::to_string).into_iter().collect::<Vec<_>>(),
            })
            .collect();
        tags.sort();
        assert_eq!(tags, vec!["elf".to_string(), "ranger".to_string()], "{url}: {echo:?}");
        let blob_echo = file(&url, &echo, "blob");
        assert_eq!(
            (blob_echo.filename.as_str(), blob_echo.size, blob_echo.crc32.as_str()),
            ("my file.png", u64::try_from(blob.len()).unwrap_or(0), crc32(&blob).as_str()),
            "{url}"
        );
        let zero = file(&url, &echo, "zero");
        assert_eq!((zero.size, zero.crc32.as_str()), (0, "00000000"), "{url}");
        assert_eq!(file(&url, &echo, "quoted").filename, "a%22b.png", "{url}: a quote in a file name arrives as %22");
        let empty = send(&url, &Multipart::new());
        assert!(empty.fields.is_empty() && empty.files.is_empty(), "{url}: {empty:?}");
        println!("{url}: portable cases ok");
    }
}
