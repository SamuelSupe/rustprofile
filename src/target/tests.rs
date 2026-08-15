#![cfg(target_os = "linux")]

use std::{
    env, fs,
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    process::{Child, Command},
    sync::Mutex,
    thread,
    time::Duration,
};

use tempfile::TempDir;

use crate::cli::TargetArgs;

use super::{
    Pod, PodContainer, PodMetadata, PodSpec, PodStatus, TargetResolver, TargetState,
    process_start_time,
};

static DOCKER_ENV_LOCK: Mutex<()> = Mutex::new(());

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

    fn pid(&self) -> i32 {
        i32::try_from(self.0.id()).expect("sleep PID should fit i32")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct DockerSocketEnv {
    previous: Option<std::ffi::OsString>,
}

impl DockerSocketEnv {
    fn set(path: &std::path::Path) -> Self {
        let previous = env::var_os("RUSTPROFILE_DOCKER_SOCKET");
        // SAFETY: the process-wide environment mutation is serialized by the
        // test's DOCKER_ENV_LOCK, and no other test mutates this variable.
        unsafe { env::set_var("RUSTPROFILE_DOCKER_SOCKET", path) };
        Self { previous }
    }
}

impl Drop for DockerSocketEnv {
    fn drop(&mut self) {
        match self.previous.take() {
            // SAFETY: see DockerSocketEnv::set; the same lock remains held
            // until this guard is dropped.
            Some(value) => unsafe { env::set_var("RUSTPROFILE_DOCKER_SOCKET", value) },
            None => unsafe { env::remove_var("RUSTPROFILE_DOCKER_SOCKET") },
        }
    }
}

fn docker_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn read_request(stream: &mut UnixStream) {
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        stream
            .read_exact(&mut byte)
            .expect("Docker request headers should be complete");
        request.push(byte[0]);
    }
}

#[test]
fn docker_refresh_tracks_restart_and_reports_deletion() {
    let _lock = DOCKER_ENV_LOCK.lock().expect("Docker env lock");
    let first = ChildGuard::spawn();
    thread::sleep(Duration::from_millis(100));
    let second = ChildGuard::spawn();
    let socket_dir = TempDir::new().expect("socket tempdir");
    let socket_path = socket_dir.path().join("docker.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind fake Docker socket");

    let container_id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let responses = [
        docker_response(
            "200 OK",
            &format!(
                r#"{{"Id":"{container_id}","Name":"/demo","State":{{"Running":true,"Pid":{}}}}}"#,
                first.0.id()
            ),
        ),
        docker_response(
            "200 OK",
            &format!(
                r#"{{"Id":"{container_id}","Name":"/demo","State":{{"Running":true,"Pid":{}}}}}"#,
                second.0.id()
            ),
        ),
        docker_response("404 Not Found", ""),
    ];
    let server = thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = listener.accept().expect("Docker request should arrive");
            read_request(&mut stream);
            stream
                .write_all(response.as_bytes())
                .expect("Docker response should be written");
        }
    });

    let _socket_env = DockerSocketEnv::set(&socket_path);
    let args = TargetArgs {
        pid: None,
        docker_container: Some("demo".to_owned()),
        k8s_pod: None,
        container: None,
    };
    let mut resolver = TargetResolver::resolve_initial(&args).expect("initial Docker resolve");
    let initial_pid = resolver.current().pid;
    let initial_start = resolver.current().metadata.process_start_time_ticks;

    let restart_result = resolver.refresh();
    let restarted = resolver.current().clone();
    let removed_result = resolver.refresh();
    server.join().expect("fake Docker server should finish");

    assert_eq!(initial_pid, first.pid());
    assert_eq!(
        restart_result.expect("restart refresh"),
        TargetState::Running
    );
    assert_eq!(restarted.pid, second.pid());
    assert_eq!(
        restarted.metadata.process_start_time_ticks,
        process_start_time(second.pid()).expect("second process start time")
    );
    assert_ne!(initial_start, restarted.metadata.process_start_time_ticks);
    assert_eq!(
        restarted.metadata.container_id.as_deref(),
        Some(container_id)
    );
    assert!(matches!(
        &resolver.identity,
        super::LogicalIdentity::Docker { container_id: id } if id == container_id
    ));
    assert_eq!(removed_result.expect("deletion refresh"), TargetState::Gone);
}

#[test]
fn docker_inspect_rejects_oversized_http_response() {
    let _lock = DOCKER_ENV_LOCK.lock().expect("Docker env lock");
    let socket_dir = TempDir::new().expect("socket tempdir");
    let socket_path = socket_dir.path().join("docker.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind fake Docker socket");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("Docker request should arrive");
        read_request(&mut stream);
        let body = vec![b'x'; 4 * 1024 * 1024 + 1];
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(&body);
    });

    let _socket_env = DockerSocketEnv::set(&socket_path);
    let result = super::docker_inspect("demo");
    server.join().expect("fake Docker server should finish");
    let error = result
        .err()
        .expect("oversized Docker response must be rejected");
    assert!(
        error
            .to_string()
            .contains("Docker API response exceeded 4194304 bytes"),
        "unexpected oversized response error: {error:#}"
    );
}

#[test]
fn container_init_pid_requires_matching_cgroup_and_namespace_init() {
    let proc_root = TempDir::new().expect("fake procfs tempdir");
    let container_id = "0123456789abcdef";
    let other_id = "fedcba9876543210";

    for (pid, cgroup, status) in [
        (
            "101",
            format!("0::/docker/{container_id}\n"),
            "Name:\tinit\nNSpid:\t101\t1\n".to_owned(),
        ),
        (
            "102",
            format!("0::/docker/{container_id}\n"),
            "Name:\tworker\nNSpid:\t102\t2\n".to_owned(),
        ),
        (
            "103",
            format!("0::/docker/{other_id}\n"),
            "Name:\tother\nNSpid:\t103\t1\n".to_owned(),
        ),
    ] {
        let process = proc_root.path().join(pid);
        fs::create_dir(&process).expect("fake process directory");
        fs::write(process.join("cgroup"), cgroup).expect("fake cgroup");
        fs::write(process.join("status"), status).expect("fake status");
    }

    assert_eq!(
        super::find_container_init_pid(proc_root.path(), container_id).expect("scan fake procfs"),
        Some(101)
    );
}

#[test]
fn select_container_requires_explicit_name_for_multi_container_pods() {
    let single = Pod {
        metadata: PodMetadata {
            uid: "single-uid".to_owned(),
        },
        spec: PodSpec {
            node_name: None,
            containers: vec![PodContainer {
                name: "app".to_owned(),
            }],
        },
        status: PodStatus::default(),
    };
    assert_eq!(
        super::select_container(&single, None).expect("single app container"),
        "app"
    );

    let multi = Pod {
        metadata: PodMetadata {
            uid: "multi-uid".to_owned(),
        },
        spec: PodSpec {
            node_name: None,
            containers: vec![
                PodContainer {
                    name: "app".to_owned(),
                },
                PodContainer {
                    name: "sidecar".to_owned(),
                },
            ],
        },
        status: PodStatus::default(),
    };
    assert!(super::select_container(&multi, None).is_err());
    assert_eq!(
        super::select_container(&multi, Some("sidecar")).expect("explicit sidecar"),
        "sidecar"
    );
    assert!(super::select_container(&multi, Some("missing")).is_err());
}
