#![cfg(target_os = "linux")]

//! Black-box coverage for the Docker target identity boundary.
//!
//! The daemon socket is intentionally replaced with a private Unix socket. This
//! exercises the same HTTP and identity parsing path without requiring a Docker
//! daemon or granting the test process access to a host socket.

use std::{
    io::{Read, Write},
    os::unix::net::UnixListener,
    process::{Child, Command},
    thread,
};

use serde_json::Value;
use tempfile::TempDir;

struct ChildGuard(Child);

impl ChildGuard {
    fn spawn() -> Self {
        Self(
            Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("sleep fixture should be available"),
        )
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn profiler() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rustprofile"))
}

#[test]
fn docker_selector_keeps_container_identity_in_check_report() {
    let target = ChildGuard::spawn();
    let socket_dir = TempDir::new().expect("socket tempdir");
    let socket_path = socket_dir.path().join("docker.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind fake Docker socket");
    let container_id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let pid = target.pid();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("Docker request should arrive");
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).expect("read Docker request");
            request.push(byte[0]);
        }
        let request = String::from_utf8_lossy(&request);
        assert!(
            request.starts_with("GET /containers/demo%20container/json HTTP/1.1"),
            "Docker selector must URL-escape the reference: {request}"
        );
        let body = format!(
            r#"{{"Id":"{container_id}","Name":"/demo","State":{{"Running":true,"Pid":{pid}}}}}"#
        );
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .expect("write Docker response headers");
        // Docker's API may use chunked transfer encoding when the daemon does
        // not know the final response size up front. Split the JSON across
        // multiple chunks so the client has to decode the framing rather than
        // accidentally accepting only a single contiguous body.
        let split = body.len() / 2;
        for (index, chunk) in [body.as_bytes().get(..split), body.as_bytes().get(split..)]
            .into_iter()
            .flatten()
            .enumerate()
        {
            write!(stream, "{:x};part={index}\r\n", chunk.len()).expect("write chunk header");
            stream.write_all(chunk).expect("write chunk body");
            stream.write_all(b"\r\n").expect("write chunk terminator");
        }
        stream
            .write_all(b"0\r\n\r\n")
            .expect("write chunked response terminator");
    });

    let output = profiler()
        .env("RUSTPROFILE_DOCKER_SOCKET", &socket_path)
        .args(["check", "--docker-container", "demo container", "--json"])
        .output()
        .expect("check command should be runnable");
    server.join().expect("fake Docker server should finish");

    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "Docker check should emit JSON even when host preflight rejects it; status={} stderr={} parse error={error}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(report["schema_version"], 3);
    assert!(report["capabilities"].is_object());
    assert_eq!(report["pid"].as_u64(), Some(u64::from(pid)));
    assert_eq!(report["target"]["kind"], "docker");
    assert_eq!(report["target"]["pid"].as_u64(), Some(u64::from(pid)));
    assert_eq!(report["target"]["container_id"], container_id);
    assert_eq!(report["target"]["container_name"], "demo");
    assert_eq!(
        report["target"]["process_start_time_ticks"].is_u64(),
        true,
        "target identity must include the PID start time to prevent PID reuse"
    );
}
