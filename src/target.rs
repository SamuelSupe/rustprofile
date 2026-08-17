use std::{
    fs,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{
    cli::TargetArgs,
    diagnostics::{TargetKind, TargetMetadata},
};

const DOCKER_SOCKET: &str = "/var/run/docker.sock";
const K8S_TOKEN: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";
const K8S_CA: &str = "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt";
const CONTROL_PLANE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_DOCKER_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_K8S_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
enum LogicalIdentity {
    Process {
        pid: i32,
        start_time_ticks: u64,
    },
    Docker {
        container_id: String,
    },
    Kubernetes {
        namespace: String,
        pod: String,
        pod_uid: String,
        container: String,
    },
}

#[derive(Clone, Debug)]
pub struct ResolvedTarget {
    pub pid: i32,
    pub metadata: TargetMetadata,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetState {
    Running,
    Waiting,
    Gone,
}

#[derive(Clone, Debug)]
enum Selector {
    Process {
        pid: i32,
    },
    Docker {
        reference: String,
    },
    Kubernetes {
        namespace: String,
        pod: String,
        container: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub struct TargetResolver {
    selector: Selector,
    identity: LogicalIdentity,
    last: ResolvedTarget,
}

impl TargetResolver {
    pub fn resolve_initial(args: &TargetArgs) -> Result<Self> {
        let selector = Selector::from_args(args)?;
        let (identity, last) = selector.resolve_initial()?;
        Ok(Self {
            selector,
            identity,
            last,
        })
    }

    pub fn current(&self) -> &ResolvedTarget {
        &self.last
    }

    pub fn supports_restart(&self) -> bool {
        !matches!(self.identity, LogicalIdentity::Process { .. })
    }

    pub fn cgroup_path(&self) -> Result<Option<PathBuf>> {
        if matches!(self.identity, LogicalIdentity::Process { .. }) {
            return Ok(None);
        }
        let contents = fs::read_to_string(format!("/proc/{}/cgroup", self.last.pid))
            .context("failed to read target cgroup membership")?;
        if let Some(path) = contents.lines().find_map(|line| {
            let mut fields = line.splitn(3, ':');
            let hierarchy = fields.next()?;
            let controllers = fields.next()?;
            let path = fields.next()?;
            (hierarchy == "0" && controllers.is_empty()).then_some(path)
        }) {
            return Ok(Some(
                Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/')),
            ));
        }
        Ok(contents.lines().find_map(|line| {
            let mut fields = line.splitn(3, ':');
            let _ = fields.next()?;
            let controllers = fields.next()?;
            let path = fields.next()?;
            controllers
                .split(',')
                .any(|item| item == "perf_event")
                .then(|| Path::new("/sys/fs/cgroup/perf_event").join(path.trim_start_matches('/')))
        }))
    }

    pub fn validate_current(&self) -> Result<()> {
        let current = process_start_time(self.last.pid)?;
        if current != self.last.metadata.process_start_time_ticks {
            bail!(
                "target process {} changed identity while it was being resolved",
                self.last.pid
            );
        }
        Ok(())
    }

    pub fn refresh(&mut self) -> Result<TargetState> {
        match self.selector.resolve_again(&self.identity)? {
            Resolution::Running(target) => {
                self.last = target;
                Ok(TargetState::Running)
            }
            Resolution::Waiting => Ok(TargetState::Waiting),
            Resolution::Gone => Ok(TargetState::Gone),
        }
    }
}

impl Selector {
    fn from_args(args: &TargetArgs) -> Result<Self> {
        if let Some(pid) = args.pid {
            return Ok(Self::Process { pid });
        }
        if let Some(reference) = args.docker_container.as_deref() {
            let reference = reference.trim();
            if reference.is_empty() {
                bail!("--docker-container must not be empty");
            }
            return Ok(Self::Docker {
                reference: reference.to_owned(),
            });
        }
        let pod = args
            .k8s_pod
            .as_deref()
            .context("one target selector is required")?;
        let (namespace, pod) = pod
            .split_once('/')
            .context("--k8s-pod must use NAMESPACE/NAME format")?;
        if namespace.is_empty() || pod.is_empty() || pod.contains('/') {
            bail!("--k8s-pod must use NAMESPACE/NAME format");
        }
        Ok(Self::Kubernetes {
            namespace: namespace.to_owned(),
            pod: pod.to_owned(),
            container: args.container.clone(),
        })
    }

    fn resolve_initial(&self) -> Result<(LogicalIdentity, ResolvedTarget)> {
        match self {
            Self::Process { pid } => {
                let start_time_ticks = process_start_time(*pid)?;
                let metadata = process_metadata(*pid, start_time_ticks);
                Ok((
                    LogicalIdentity::Process {
                        pid: *pid,
                        start_time_ticks,
                    },
                    ResolvedTarget {
                        pid: *pid,
                        metadata,
                    },
                ))
            }
            Self::Docker { reference } => {
                let inspect = docker_inspect(reference)?
                    .with_context(|| format!("Docker container {reference:?} was not found"))?;
                let target = docker_target(&inspect)?.context("Docker container is not running")?;
                Ok((
                    LogicalIdentity::Docker {
                        container_id: inspect.id,
                    },
                    target,
                ))
            }
            Self::Kubernetes {
                namespace,
                pod,
                container,
            } => {
                let pod_report = kubernetes_pod(namespace, pod)?
                    .with_context(|| format!("Kubernetes pod {namespace}/{pod} was not found"))?;
                let container = select_container(&pod_report, container.as_deref())?;
                let target = kubernetes_target(namespace, pod, &pod_report, &container)?
                    .context("Kubernetes container is not running")?;
                Ok((
                    LogicalIdentity::Kubernetes {
                        namespace: namespace.clone(),
                        pod: pod.clone(),
                        pod_uid: pod_report.metadata.uid,
                        container,
                    },
                    target,
                ))
            }
        }
    }

    fn resolve_again(&self, identity: &LogicalIdentity) -> Result<Resolution> {
        match identity {
            LogicalIdentity::Process {
                pid,
                start_time_ticks,
            } => match process_start_time(*pid) {
                Ok(current) if current == *start_time_ticks => {
                    Ok(Resolution::Running(ResolvedTarget {
                        pid: *pid,
                        metadata: process_metadata(*pid, current),
                    }))
                }
                _ => Ok(Resolution::Gone),
            },
            LogicalIdentity::Docker { container_id } => {
                let Some(inspect) = docker_inspect(container_id)? else {
                    return Ok(Resolution::Gone);
                };
                if inspect.id != *container_id {
                    return Ok(Resolution::Gone);
                }
                match docker_target(&inspect) {
                    Ok(target) => Ok(target.map_or(Resolution::Waiting, Resolution::Running)),
                    Err(error) if process_disappeared(&error) => Ok(Resolution::Waiting),
                    Err(error) => Err(error),
                }
            }
            LogicalIdentity::Kubernetes {
                namespace,
                pod,
                pod_uid,
                container,
            } => {
                let Some(report) = kubernetes_pod(namespace, pod)? else {
                    return Ok(Resolution::Gone);
                };
                if report.metadata.uid != *pod_uid {
                    return Ok(Resolution::Gone);
                }
                match kubernetes_target(namespace, pod, &report, container) {
                    Ok(target) => Ok(target.map_or(Resolution::Waiting, Resolution::Running)),
                    Err(error) if process_disappeared(&error) => Ok(Resolution::Waiting),
                    Err(error) => Err(error),
                }
            }
        }
    }
}

enum Resolution {
    Running(ResolvedTarget),
    Waiting,
    Gone,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerInspect {
    id: String,
    name: String,
    state: DockerState,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerState {
    running: bool,
    pid: i32,
}

fn docker_inspect(reference: &str) -> Result<Option<DockerInspect>> {
    let socket = std::env::var_os("RUSTPROFILE_DOCKER_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DOCKER_SOCKET));
    let mut stream = UnixStream::connect(&socket)
        .with_context(|| format!("failed to connect to Docker socket {}", socket.display()))?;
    stream
        .set_read_timeout(Some(CONTROL_PLANE_TIMEOUT))
        .context("failed to configure Docker socket read timeout")?;
    stream
        .set_write_timeout(Some(CONTROL_PLANE_TIMEOUT))
        .context("failed to configure Docker socket write timeout")?;
    let path = format!("/containers/{}/json", percent_encode_path(reference));
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = Vec::new();
    (&mut stream)
        .take((MAX_DOCKER_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut response)
        .context("failed to read Docker API response")?;
    if response.len() > MAX_DOCKER_RESPONSE_BYTES {
        bail!("Docker API response exceeded {MAX_DOCKER_RESPONSE_BYTES} bytes");
    }
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("Docker API returned an invalid HTTP response")?;
    let head = std::str::from_utf8(&response[..split]).context("invalid Docker HTTP headers")?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .context("Docker API response is missing a status code")?;
    if status == 404 {
        return Ok(None);
    }
    if status != 200 {
        bail!("Docker API returned HTTP {status}");
    }
    let body = &response[split + 4..];
    let body = if head.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                && value
                    .split(',')
                    .any(|encoding| encoding.trim().eq_ignore_ascii_case("chunked"))
        })
    }) {
        decode_chunked(body)?
    } else {
        body.to_vec()
    };
    serde_json::from_slice(&body)
        .context("failed to decode Docker inspect response")
        .map(Some)
}

fn decode_chunked(mut encoded: &[u8]) -> Result<Vec<u8>> {
    let mut decoded = Vec::new();
    loop {
        let line_end = encoded
            .windows(2)
            .position(|window| window == b"\r\n")
            .context("invalid chunked Docker response")?;
        let size = std::str::from_utf8(&encoded[..line_end])
            .context("invalid Docker chunk size")?
            .split(';')
            .next()
            .unwrap_or_default();
        let size = usize::from_str_radix(size.trim(), 16).context("invalid Docker chunk size")?;
        encoded = &encoded[line_end + 2..];
        if size == 0 {
            return Ok(decoded);
        }
        let chunk_end = size
            .checked_add(2)
            .context("Docker chunk size is too large")?;
        if encoded.len() < chunk_end || &encoded[size..chunk_end] != b"\r\n" {
            bail!("truncated chunked Docker response");
        }
        decoded.extend_from_slice(&encoded[..size]);
        encoded = &encoded[chunk_end..];
    }
}

fn docker_target(inspect: &DockerInspect) -> Result<Option<ResolvedTarget>> {
    if !inspect.state.running || inspect.state.pid <= 0 {
        return Ok(None);
    }
    let start_time_ticks = process_start_time(inspect.state.pid)?;
    Ok(Some(ResolvedTarget {
        pid: inspect.state.pid,
        metadata: TargetMetadata {
            kind: TargetKind::Docker,
            pid: inspect.state.pid,
            process_start_time_ticks: start_time_ticks,
            container_id: Some(inspect.id.clone()),
            container_name: Some(inspect.name.trim_start_matches('/').to_owned()),
            k8s_namespace: None,
            k8s_pod_name: None,
            k8s_pod_uid: None,
            k8s_container_name: None,
            k8s_node_name: None,
        },
    }))
}

#[derive(Deserialize)]
struct Pod {
    metadata: PodMetadata,
    spec: PodSpec,
    #[serde(default)]
    status: PodStatus,
}

#[derive(Deserialize)]
struct PodMetadata {
    uid: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PodSpec {
    node_name: Option<String>,
    containers: Vec<PodContainer>,
}

#[derive(Deserialize)]
struct PodContainer {
    name: String,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PodStatus {
    #[serde(default)]
    container_statuses: Vec<ContainerStatus>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContainerStatus {
    name: String,
    container_id: Option<String>,
}

fn kubernetes_pod(namespace: &str, pod: &str) -> Result<Option<Pod>> {
    let host = std::env::var("KUBERNETES_SERVICE_HOST").context(
        "KUBERNETES_SERVICE_HOST is not set; Kubernetes targets require in-cluster execution",
    )?;
    let port = std::env::var("KUBERNETES_SERVICE_PORT_HTTPS")
        .or_else(|_| std::env::var("KUBERNETES_SERVICE_PORT"))
        .unwrap_or_else(|_| "443".to_owned());
    let token_path = std::env::var_os("RUSTPROFILE_K8S_TOKEN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(K8S_TOKEN));
    let ca_path = std::env::var_os("RUSTPROFILE_K8S_CA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(K8S_CA));
    let token = fs::read_to_string(&token_path)
        .with_context(|| format!("failed to read Kubernetes token {}", token_path.display()))?;
    let ca = fs::read(&ca_path)
        .with_context(|| format!("failed to read Kubernetes CA {}", ca_path.display()))?;
    let certificate = ureq::tls::Certificate::from_pem(&ca)
        .context("failed to parse Kubernetes service account CA")?;
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(CONTROL_PLANE_TIMEOUT))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::new_with_certs(&[certificate]))
                .build(),
        )
        .build()
        .new_agent();
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host
    };
    let url = format!("https://{host}:{port}/api/v1/namespaces/{namespace}/pods/{pod}");
    let response = agent
        .get(&url)
        .header("Authorization", &format!("Bearer {}", token.trim()))
        .call()
        .context("Kubernetes API request failed")?;
    if response.status().as_u16() == 404 {
        return Ok(None);
    }
    if response.status().as_u16() != 200 {
        bail!("Kubernetes API returned HTTP {}", response.status());
    }
    let body = response
        .into_body()
        .with_config()
        .limit(MAX_K8S_RESPONSE_BYTES as u64)
        .read_to_vec()
        .context("failed to read Kubernetes Pod response")?;
    serde_json::from_slice(&body)
        .context("failed to decode Kubernetes Pod response")
        .map(Some)
}

fn select_container(pod: &Pod, requested: Option<&str>) -> Result<String> {
    if let Some(requested) = requested {
        if pod
            .spec
            .containers
            .iter()
            .any(|item| item.name == requested)
        {
            return Ok(requested.to_owned());
        }
        bail!("container {requested:?} is not an application container in the selected Pod");
    }
    match pod.spec.containers.as_slice() {
        [container] => Ok(container.name.clone()),
        [] => bail!("selected Pod has no application containers"),
        _ => bail!("selected Pod has multiple application containers; specify --container"),
    }
}

fn kubernetes_target(
    namespace: &str,
    pod_name: &str,
    pod: &Pod,
    container: &str,
) -> Result<Option<ResolvedTarget>> {
    let expected_node = std::env::var("NODE_NAME")
        .context("NODE_NAME is not set; the Kubernetes DaemonSet must inject spec.nodeName")?;
    let actual_node = pod.spec.node_name.as_deref().unwrap_or_default();
    if actual_node != expected_node {
        bail!(
            "Pod {namespace}/{pod_name} runs on node {actual_node:?}, not profiler node {expected_node:?}"
        );
    }
    let container_id = pod
        .status
        .container_statuses
        .iter()
        .find(|status| status.name == container)
        .and_then(|status| status.container_id.as_deref());
    let Some(container_id) = container_id else {
        return Ok(None);
    };
    if container_id.is_empty() {
        return Ok(None);
    }
    let runtime_id = container_id
        .split_once("://")
        .map_or(container_id, |(_, id)| id);
    if runtime_id.len() < 12 || !runtime_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("unsupported Kubernetes container ID {container_id:?}");
    }
    let proc_root = std::env::var_os("RUSTPROFILE_PROC_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/proc"));
    let Some(pid) = find_container_init_pid(&proc_root, runtime_id)? else {
        return Ok(None);
    };
    let start_time_ticks = process_start_time(pid)?;
    Ok(Some(ResolvedTarget {
        pid,
        metadata: TargetMetadata {
            kind: TargetKind::Kubernetes,
            pid,
            process_start_time_ticks: start_time_ticks,
            container_id: Some(runtime_id.to_owned()),
            container_name: None,
            k8s_namespace: Some(namespace.to_owned()),
            k8s_pod_name: Some(pod_name.to_owned()),
            k8s_pod_uid: Some(pod.metadata.uid.clone()),
            k8s_container_name: Some(container.to_owned()),
            k8s_node_name: pod.spec.node_name.clone(),
        },
    }))
}

pub(crate) fn find_container_init_pid(proc_root: &Path, container_id: &str) -> Result<Option<i32>> {
    let mut found = None;
    for entry in fs::read_dir(proc_root)
        .with_context(|| format!("failed to read procfs at {}", proc_root.display()))?
    {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let process = entry.path();
        let Ok(cgroup) = fs::read_to_string(process.join("cgroup")) else {
            continue;
        };
        if !cgroup.contains(container_id) {
            continue;
        }
        let Ok(status) = fs::read_to_string(process.join("status")) else {
            continue;
        };
        let is_init = status.lines().find_map(|line| {
            line.strip_prefix("NSpid:").map(|values| {
                values
                    .split_whitespace()
                    .last()
                    .is_some_and(|value| value == "1")
            })
        });
        if is_init == Some(true) {
            found = Some(found.map_or(pid, |current: i32| current.min(pid)));
        }
    }
    Ok(found)
}

pub(crate) fn process_start_time(pid: i32) -> Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))
        .with_context(|| format!("failed to read process {pid} start time"))?;
    let after_name = stat
        .rsplit_once(')')
        .map(|(_, fields)| fields)
        .context("invalid proc stat format")?;
    after_name
        .split_whitespace()
        .nth(19)
        .context("proc stat is missing starttime")?
        .parse::<u64>()
        .context("invalid process starttime")
}

fn process_disappeared(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

fn process_metadata(pid: i32, start_time_ticks: u64) -> TargetMetadata {
    TargetMetadata {
        kind: TargetKind::Process,
        pid,
        process_start_time_ticks: start_time_ticks,
        container_id: None,
        container_name: None,
        k8s_namespace: None,
        k8s_pod_name: None,
        k8s_pod_uid: None,
        k8s_container_name: None,
        k8s_node_name: None,
    }
}

fn percent_encode_path(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests;
