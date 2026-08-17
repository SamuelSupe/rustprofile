#![cfg(target_os = "linux")]

//! HTTP-level coverage for the Firefox gallery and the single-profile route.

use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

use flate2::{Compression, write::GzEncoder};
use json_slabs::Builder;
use serde_json::{Value, json};
use tempfile::TempDir;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn profiler() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rustprofile"))
}

fn profile_json() -> Value {
    json!({
        "meta": {"startTime": 1_000.0, "interval": 1.0},
        "shared": {
            "stringArray": ["root"],
            "stackTable": {"prefixOffset": [0], "frame": [0]},
            "frameTable": {"func": [0]},
            "funcTable": {"name": [0]}
        },
        "threads": [{
            "pid": 41,
            "tid": 42,
            "name": "gallery-worker",
            "samples": {
                "stack": [0],
                "weight": [1],
                "timeDeltas": [1.5],
                "threadCPUDelta": [2.0]
            }
        }]
    })
}

fn write_gzip(path: &Path, bytes: &[u8]) {
    let file = File::create(path).expect("create Firefox profile");
    let mut encoder = GzEncoder::new(file, Compression::fast());
    encoder.write_all(bytes).expect("gzip Firefox fixture");
    encoder.finish().expect("finish Firefox fixture");
}

fn write_profile(path: &Path) {
    let bytes = serde_json::to_vec(&profile_json()).expect("serialize Firefox fixture");
    write_gzip(path, &bytes);
}

fn write_jslb_profile(path: &Path) {
    let root = serde_json::to_vec(&profile_json()).expect("serialize Firefox fixture");
    let bytes = Builder::new().finish(&root);
    write_gzip(path, &bytes);
}

fn start_server(source_flag: &str, source: &Path) -> (ChildGuard, u16) {
    start_server_with_cors(source_flag, source, None)
}

fn start_server_with_cors(
    source_flag: &str,
    source: &Path,
    cors_origin: Option<&str>,
) -> (ChildGuard, u16) {
    let mut command = profiler();
    command.args([
        "serve",
        source_flag,
        source.to_str().expect("source path is UTF-8"),
    ]);
    if let Some(origin) = cors_origin {
        command.args(["--cors-origin", origin]);
    }
    let mut child = command
        .args(["--listen", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("serve should start");
    let stdout = child.stdout.take().expect("serve stdout");
    let mut stdout = BufReader::new(stdout);
    let mut line = String::new();
    stdout.read_line(&mut line).expect("read serve address");
    assert!(line.contains("serving "), "unexpected serve output: {line}");
    let port = line
        .trim()
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
        .expect("serve output should contain a port");
    (ChildGuard(child), port)
}

fn http_request(
    port: u16,
    method: &str,
    path: &str,
    extra_headers: &[(&str, &str)],
) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to serve");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set HTTP read timeout");
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\n").expect("write HTTP request");
    for (name, value) in extra_headers {
        write!(stream, "{name}: {value}\r\n").expect("write HTTP request header");
    }
    write!(stream, "Connection: close\r\n\r\n").expect("finish HTTP request");
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).expect("read HTTP response");
    let split = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP response headers");
    let headers = String::from_utf8(bytes[..split].to_vec()).expect("HTTP headers are UTF-8");
    let body = bytes[split + 4..].to_vec();
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .expect("HTTP status");
    (status, headers, body)
}

fn http_get(port: u16, path: &str) -> (u16, String, Vec<u8>) {
    http_request(port, "GET", path, &[])
}

fn json_body(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("JSON response")
}

#[test]
fn directory_gallery_serves_html_manifest_profile_and_safe_ids() {
    let fixture = TempDir::new().expect("gallery fixture directory");
    let profile = fixture.path().join("firefox-gallery-000001-100.json.gz");
    write_profile(&profile);
    let (_server, port) = start_server("--directory", fixture.path());

    let (status, headers, body) = http_get(port, "/");
    assert_eq!(status, 200);
    assert!(headers.contains("text/html; charset=utf-8"));
    assert!(
        !headers
            .to_ascii_lowercase()
            .contains("access-control-allow-origin:")
    );
    let html = String::from_utf8(body).expect("viewer HTML");
    assert!(html.contains("/api/profiles"));
    assert!(html.contains("CPU flame graph"));
    assert!(html.contains("<svg"));

    let (status, _, body) = http_get(port, "/api/profiles");
    assert_eq!(status, 200);
    let entries = json_body(&body).as_array().cloned().expect("profile list");
    assert_eq!(entries.len(), 1);
    let id = entries[0]["id"].as_str().expect("profile id");
    assert_eq!(entries[0]["format"], "json");
    assert_eq!(entries[0]["filename"], "firefox-gallery-000001-100.json.gz");

    let (status, _, body) = http_get(port, &format!("/api/profile/{id}"));
    assert_eq!(status, 200);
    let decoded = json_body(&body);
    assert_eq!(decoded["sample_count"], 1);
    assert_eq!(decoded["threads"][0]["pid"], "41");
    assert_eq!(decoded["threads"][0]["tid"], "42");
    assert_eq!(decoded["threads"][0]["name"], "gallery-worker");

    let (status, _, _) = http_get(port, "/api/profile/%2e%2e%2fetc%2fpasswd");
    assert_eq!(status, 404, "profile IDs must not become filesystem paths");
    let (status, _, _) = http_get(port, "/profile.json");
    assert_eq!(status, 404, "legacy profile route is single-file only");

    let (status, headers, _) = http_request(
        port,
        "OPTIONS",
        "/api/profiles",
        &[("Origin", "https://viewer.example")],
    );
    assert_eq!(status, 405, "CORS preflight is disabled by default");
    assert!(
        !headers
            .to_ascii_lowercase()
            .contains("access-control-allow-origin:")
    );
}

#[test]
fn single_profile_keeps_legacy_profile_route() {
    let fixture = TempDir::new().expect("profile fixture directory");
    let profile = fixture.path().join("firefox-gallery-000001-100.json.gz");
    write_profile(&profile);
    let (_server, port) = start_server("--profile", &profile);

    let (status, headers, body) = http_get(port, "/profile.json");
    assert_eq!(status, 200);
    assert!(headers.contains("content-type: application/json"));
    assert!(headers.contains("content-encoding: gzip"));
    assert!(body.starts_with(&[0x1f, 0x8b]));

    let (status, _, body) = http_get(port, "/api/profiles");
    assert_eq!(status, 200);
    assert_eq!(json_body(&body).as_array().map(Vec::len), Some(1));
}

#[test]
fn single_jslb_profile_uses_binary_legacy_content_type() {
    let fixture = TempDir::new().expect("profile fixture directory");
    let profile = fixture.path().join("firefox-gallery-000001-100.jslb.gz");
    write_jslb_profile(&profile);
    let (_server, port) = start_server("--profile", &profile);

    let (status, headers, body) = http_get(port, "/profile.json");
    let headers = headers.to_ascii_lowercase();
    assert_eq!(status, 200);
    assert!(headers.contains("content-type: application/octet-stream"));
    assert!(headers.contains("content-encoding: gzip"));
    assert!(body.starts_with(&[0x1f, 0x8b]));
}

#[test]
fn explicit_cors_origin_allows_preflight_and_get() {
    let fixture = TempDir::new().expect("profile fixture directory");
    let profile = fixture.path().join("firefox-gallery-000001-100.json.gz");
    write_profile(&profile);
    let (_server, port) =
        start_server_with_cors("--profile", &profile, Some("https://viewer.example"));

    let (status, headers, _) = http_request(
        port,
        "OPTIONS",
        "/api/profiles",
        &[("Origin", "https://viewer.example")],
    );
    let headers = headers.to_ascii_lowercase();
    assert_eq!(status, 204);
    assert!(headers.contains("access-control-allow-origin: https://viewer.example"));
    assert!(headers.contains("access-control-allow-methods:"));

    let (status, headers, _) = http_get(port, "/healthz");
    assert_eq!(status, 200);
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("access-control-allow-origin: https://viewer.example")
    );
}
