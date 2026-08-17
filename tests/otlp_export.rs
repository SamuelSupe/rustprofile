#![cfg(target_os = "linux")]

//! End-to-end OTLP/HTTP Profiles coverage.
//!
//! The fixture server intentionally speaks only the small v1.11.0 subset that
//! rustprofile emits. Decoding the request here catches wire-shape regressions
//! without depending on a collector installation.

use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use flate2::read::GzDecoder;
use prost::Message;
use serde_json::Value;
use tempfile::{TempDir, tempdir};

#[derive(Clone, PartialEq, Message)]
struct ExportRequest {
    #[prost(message, repeated, tag = "1")]
    resource_profiles: Vec<ResourceProfiles>,
    #[prost(message, optional, tag = "2")]
    dictionary: Option<ProfilesDictionary>,
}

#[derive(Clone, PartialEq, Message)]
struct ExportResponse {
    #[prost(message, optional, tag = "1")]
    partial_success: Option<PartialSuccess>,
}

#[derive(Clone, PartialEq, Message)]
struct PartialSuccess {
    #[prost(int64, tag = "1")]
    rejected_profiles: i64,
    #[prost(string, tag = "2")]
    error_message: String,
}

#[derive(Clone, PartialEq, Message)]
struct ResourceProfiles {
    #[prost(message, optional, tag = "1")]
    resource: Option<Resource>,
    #[prost(message, repeated, tag = "2")]
    scope_profiles: Vec<ScopeProfiles>,
}

#[derive(Clone, PartialEq, Message)]
struct Resource {
    #[prost(message, repeated, tag = "1")]
    attributes: Vec<KeyValue>,
}

#[derive(Clone, PartialEq, Message)]
struct ScopeProfiles {
    #[prost(message, optional, tag = "1")]
    scope: Option<InstrumentationScope>,
    #[prost(message, repeated, tag = "2")]
    profiles: Vec<Profile>,
}

#[derive(Clone, PartialEq, Message)]
struct InstrumentationScope {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(string, tag = "2")]
    version: String,
    #[prost(message, repeated, tag = "3")]
    attributes: Vec<KeyValue>,
    #[prost(uint32, tag = "4")]
    dropped_attributes_count: u32,
}

#[derive(Clone, PartialEq, Message)]
struct KeyValue {
    #[prost(string, tag = "1")]
    key: String,
    #[prost(message, optional, tag = "2")]
    value: Option<AnyValue>,
}

#[derive(Clone, PartialEq, Message)]
struct AnyValue {
    #[prost(oneof = "any_value::Value", tags = "1, 2, 3, 4, 5, 6, 7")]
    value: Option<any_value::Value>,
}

mod any_value {
    use prost::Oneof;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Value {
        #[prost(string, tag = "1")]
        StringValue(String),
        #[prost(bool, tag = "2")]
        BoolValue(bool),
        #[prost(int64, tag = "3")]
        IntValue(i64),
        #[prost(double, tag = "4")]
        DoubleValue(f64),
        #[prost(message, tag = "5")]
        ArrayValue(super::ArrayValue),
        #[prost(message, tag = "6")]
        KvlistValue(super::KeyValueList),
        #[prost(bytes, tag = "7")]
        BytesValue(Vec<u8>),
    }
}

#[derive(Clone, PartialEq, Message)]
struct ArrayValue {
    #[prost(message, repeated, tag = "1")]
    values: Vec<AnyValue>,
}

#[derive(Clone, PartialEq, Message)]
struct KeyValueList {
    #[prost(message, repeated, tag = "1")]
    values: Vec<KeyValue>,
}

#[derive(Clone, PartialEq, Message)]
struct Profile {
    #[prost(message, optional, tag = "1")]
    sample_type: Option<ValueType>,
    #[prost(message, repeated, tag = "2")]
    samples: Vec<Sample>,
    #[prost(fixed64, tag = "3")]
    time_unix_nano: u64,
    #[prost(uint64, tag = "4")]
    duration_nano: u64,
    #[prost(message, optional, tag = "5")]
    period_type: Option<ValueType>,
    #[prost(int64, tag = "6")]
    period: i64,
    #[prost(bytes, tag = "7")]
    profile_id: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct ValueType {
    #[prost(int32, tag = "1")]
    type_strindex: i32,
    #[prost(int32, tag = "2")]
    unit_strindex: i32,
}

#[derive(Clone, PartialEq, Message)]
struct Sample {
    #[prost(int32, tag = "1")]
    stack_index: i32,
    #[prost(int32, repeated, packed = "true", tag = "2")]
    attribute_indices: Vec<i32>,
    #[prost(int64, repeated, packed = "true", tag = "4")]
    values: Vec<i64>,
    #[prost(fixed64, repeated, packed = "true", tag = "5")]
    timestamps_unix_nano: Vec<u64>,
}

#[derive(Clone, PartialEq, Message)]
struct ProfilesDictionary {
    #[prost(message, repeated, tag = "1")]
    mapping_table: Vec<Mapping>,
    #[prost(message, repeated, tag = "2")]
    location_table: Vec<Location>,
    #[prost(message, repeated, tag = "3")]
    function_table: Vec<Function>,
    #[prost(message, repeated, tag = "6")]
    attribute_table: Vec<KeyValueAndUnit>,
    #[prost(string, repeated, tag = "5")]
    string_table: Vec<String>,
    #[prost(message, repeated, tag = "7")]
    stack_table: Vec<Stack>,
}

#[derive(Clone, PartialEq, Message)]
struct KeyValueAndUnit {
    #[prost(int32, tag = "1")]
    key_strindex: i32,
    #[prost(message, optional, tag = "2")]
    value: Option<AnyValue>,
    #[prost(int32, tag = "3")]
    unit_strindex: i32,
}

#[derive(Clone, PartialEq, Message)]
struct Mapping {
    #[prost(uint64, tag = "1")]
    memory_start: u64,
    #[prost(uint64, tag = "2")]
    memory_limit: u64,
    #[prost(uint64, tag = "3")]
    file_offset: u64,
    #[prost(int32, tag = "4")]
    filename_strindex: i32,
}

#[derive(Clone, PartialEq, Message)]
struct Location {
    #[prost(int32, tag = "1")]
    mapping_index: i32,
    #[prost(uint64, tag = "2")]
    address: u64,
}

#[derive(Clone, PartialEq, Message)]
struct Function {
    #[prost(int32, tag = "1")]
    name_strindex: i32,
}

#[derive(Clone, PartialEq, Message)]
struct Stack {
    #[prost(int32, repeated, packed = "true", tag = "1")]
    location_indices: Vec<i32>,
}

#[derive(Debug)]
struct CapturedRequest {
    headers: String,
    body: Vec<u8>,
}

struct ChildGuard(Child);

impl ChildGuard {
    fn spawn(path: &std::path::Path) -> Self {
        Self(
            Command::new(path)
                .spawn()
                .unwrap_or_else(|error| panic!("failed to start {}: {error}", path.display())),
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

fn compile_fixture(output: &std::path::Path) {
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cpu_target.rs");
    let status = Command::new("rustc")
        .args([
            "--edition=2024",
            source.to_str().expect("fixture path is valid UTF-8"),
            "-C",
            "debuginfo=2",
            "-C",
            "force-frame-pointers=yes",
            "-o",
            output.to_str().expect("fixture path is valid UTF-8"),
        ])
        .status()
        .expect("rustc should be available");
    assert!(status.success(), "failed to compile profiling fixture");
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<CapturedRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    while !bytes.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte)?;
        bytes.push(byte[0]);
    }
    let header_end = bytes.len();
    let headers = String::from_utf8_lossy(&bytes).into_owned();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let _ = bytes.split_off(header_end);
    let mut body = vec![0_u8; content_length];
    stream.read_exact(&mut body)?;
    Ok(CapturedRequest { headers, body })
}

fn decoded_request_body(request: &CapturedRequest) -> Vec<u8> {
    let is_gzip = request.headers.lines().any(|line| {
        let Some((key, value)) = line.split_once(':') else {
            return false;
        };
        key.eq_ignore_ascii_case("content-encoding") && value.trim().eq_ignore_ascii_case("gzip")
    });
    if !is_gzip {
        return request.body.clone();
    }
    let mut decoder = GzDecoder::new(request.body.as_slice());
    let mut decoded = Vec::new();
    decoder
        .read_to_end(&mut decoded)
        .expect("gzip OTLP request body should be decodable");
    decoded
}

fn partial_response() -> Vec<u8> {
    ExportResponse {
        partial_success: Some(PartialSuccess {
            rejected_profiles: 1,
            error_message: "one profile rejected by test receiver".to_owned(),
        }),
    }
    .encode_to_vec()
}

fn serve(listener: TcpListener, requests: Arc<Mutex<Vec<CapturedRequest>>>) {
    listener
        .set_nonblocking(true)
        .expect("set OTLP listener nonblocking");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let request = read_request(&mut stream).expect("read OTLP request");
                let attempt = {
                    let mut requests = requests.lock().expect("request lock");
                    requests.push(request);
                    requests.len()
                };
                if attempt == 1 {
                    write!(
                        stream,
                        "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .expect("write retry response");
                } else {
                    let body = partial_response();
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .expect("write OTLP response headers");
                    stream.write_all(&body).expect("write OTLP response body");
                }
                if attempt >= 2 {
                    return;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept OTLP request: {error}"),
        }
    }
}

fn resource_string(resource: &Resource, key: &str) -> Option<String> {
    resource.attributes.iter().find_map(|item| {
        if item.key != key {
            return None;
        }
        match item.value.as_ref()?.value.as_ref()? {
            any_value::Value::StringValue(value) => Some(value.clone()),
            _ => None,
        }
    })
}

fn dictionary_string(dictionary: &ProfilesDictionary, index: i32) -> &str {
    dictionary
        .string_table
        .get(usize::try_from(index).expect("dictionary string index is non-negative"))
        .map(String::as_str)
        .expect("dictionary string index is in range")
}

fn sample_attribute_keys(dictionary: &ProfilesDictionary, sample: &Sample) -> Vec<String> {
    sample
        .attribute_indices
        .iter()
        .map(|index| {
            let attribute = dictionary
                .attribute_table
                .get(usize::try_from(*index).expect("attribute index is non-negative"))
                .expect("sample attribute index is in range");
            dictionary_string(dictionary, attribute.key_strindex).to_owned()
        })
        .collect()
}

fn preflight_unavailable(output: &std::process::Output) -> bool {
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    [
        "kernel ",
        "perf access probe failed",
        "rustprofile MVP must run as root",
        "failed to load",
        "failed to attach tracepoint",
        "preflight failed",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

#[test]
fn otlp_http_retries_partial_success_and_emits_v111_profiles() {
    let fixtures = tempdir().expect("fixture tempdir");
    let fixture = fixtures.path().join("cpu-target");
    compile_fixture(&fixture);
    let target = ChildGuard::spawn(&fixture);

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind OTLP fixture listener");
    let endpoint = format!(
        "http://{}/v1development/profiles",
        listener.local_addr().expect("OTLP listener address")
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_for_server = Arc::clone(&requests);
    let server = thread::spawn(move || serve(listener, requests_for_server));
    let output_dir = TempDir::new().expect("profile output tempdir");

    let output = profiler()
        .args([
            "record",
            "--pid",
            &target.pid().to_string(),
            "--profiles",
            "cpu",
            "--duration",
            "2s",
            "--window",
            "500ms",
            "--unwind",
            "fp",
            "--allow-partial",
            "--otlp-timeline",
            "--max-timeline-samples",
            "1",
            "--output",
            output_dir
                .path()
                .to_str()
                .expect("output path is valid UTF-8"),
            "--otlp-endpoint",
            &endpoint,
            "--otlp-header",
            "Authorization=Bearer test-secret",
            "--otlp-compression",
            "none",
            "--resource-attribute",
            "deployment.environment=test",
        ])
        .output()
        .expect("record command should be runnable");
    server.join().expect("OTLP server should finish");

    let requests = requests.lock().expect("request lock");
    if requests.is_empty() && preflight_unavailable(&output) {
        eprintln!(
            "skipping OTLP runtime test because profiling preflight is unavailable: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    assert!(
        output.status.success(),
        "record with OTLP export failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        requests.len() >= 2,
        "retryable HTTP failure must be retried"
    );
    let request = &requests[1];
    assert!(
        request
            .headers
            .contains("POST /v1development/profiles HTTP/1.1")
    );
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("content-type: application/x-protobuf")
    );
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer test-secret")
    );
    assert!(
        !request
            .headers
            .to_ascii_lowercase()
            .contains("content-encoding")
    );

    let payload =
        ExportRequest::decode(request.body.as_slice()).expect("decode OTLP v1.11 payload");
    assert_eq!(payload.resource_profiles.len(), 1);
    let resource_profiles = &payload.resource_profiles[0];
    let resource = resource_profiles
        .resource
        .as_ref()
        .expect("resource metadata");
    assert_eq!(
        resource_string(resource, "deployment.environment").as_deref(),
        Some("test")
    );
    assert!(
        resource_string(resource, "process.pid").is_none(),
        "process.pid must be an integer OTLP value"
    );
    assert!(
        resource
            .attributes
            .iter()
            .any(|item| item.key == "process.pid")
    );
    let scope = resource_profiles.scope_profiles[0]
        .scope
        .as_ref()
        .expect("instrumentation scope");
    assert_eq!(scope.name, "rustprofile");
    assert!(scope.version.starts_with("0."));
    let scope_attribute_keys = scope
        .attributes
        .iter()
        .map(|attribute| attribute.key.as_str())
        .collect::<HashSet<_>>();
    assert!(
        scope_attribute_keys.contains("pprof.scope.sample_type_order"),
        "OTLP scope must preserve pprof sample type ordering"
    );
    assert!(
        scope_attribute_keys.contains("pprof.scope.default_sample_type"),
        "OTLP scope must preserve the pprof default sample type"
    );
    let profiles = &resource_profiles.scope_profiles[0].profiles;
    assert_eq!(
        profiles.len(),
        1,
        "a CPU timeline must emit one OTLP profile, not one profile per pprof value"
    );
    let dictionary = payload
        .dictionary
        .as_ref()
        .expect("shared profile dictionary");
    let mut profile_types = HashSet::new();
    let mut profile_ids = HashSet::new();
    let mut sample_attribute_key_set = HashSet::new();
    for profile in profiles {
        assert_eq!(profile.profile_id.len(), 16);
        assert!(profile.profile_id.iter().any(|byte| *byte != 0));
        assert!(
            profile_ids.insert(profile.profile_id.clone()),
            "profile IDs must be unique"
        );
        assert!(profile.sample_type.is_some());
        assert!(profile.period_type.is_some());
        assert!(profile.time_unix_nano > 0);
        assert!(profile.duration_nano > 0);
        profile_types.insert(dictionary_string(
            dictionary,
            profile
                .sample_type
                .as_ref()
                .expect("sample type")
                .type_strindex,
        ));
        assert!(
            profile.samples.iter().all(|sample| {
                !sample.values.is_empty()
                    && sample.values.len() == 1
                    && sample.values.len() == sample.timestamps_unix_nano.len()
                    && sample.values.iter().all(|value| *value > 0)
                    && sample.timestamps_unix_nano.iter().all(|timestamp| {
                        *timestamp >= profile.time_unix_nano
                            && *timestamp
                                <= profile.time_unix_nano.saturating_add(profile.duration_nano)
                    })
                    && !sample.attribute_indices.is_empty()
            }),
            "timeline samples must carry aligned values, timestamps, and attributes"
        );
        for sample in &profile.samples {
            sample_attribute_key_set.extend(sample_attribute_keys(dictionary, sample));
        }
    }
    assert_eq!(
        profile_types.len(),
        1,
        "timeline must not duplicate CPU profiles"
    );
    assert!(profile_types.contains("cpu"));
    for key in ["process.pid", "thread.id", "thread.name"] {
        assert!(
            sample_attribute_key_set.contains(key),
            "timeline samples must preserve {key} attributes; decoded keys: {sample_attribute_key_set:?}"
        );
    }
    assert!(
        dictionary.attribute_table.len() > 1,
        "timeline labels must be encoded in the OTLP attribute table"
    );
    assert_eq!(
        dictionary.string_table.first().map(String::as_str),
        Some("")
    );
    assert!(dictionary.string_table.iter().any(|value| value == "cpu"));
    assert_eq!(dictionary.mapping_table.first().map(|_| ()), Some(()));
    assert_eq!(dictionary.location_table.first().map(|_| ()), Some(()));
    assert_eq!(dictionary.function_table.first().map(|_| ()), Some(()));
    assert_eq!(dictionary.stack_table.first().map(|_| ()), Some(()));
    let (mapping_index, mapping) = dictionary
        .mapping_table
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, mapping)| mapping.memory_start != 0)
        .expect("a non-default mapping must be exported");
    assert!(
        !dictionary_string(dictionary, mapping.filename_strindex).is_empty(),
        "mapping filename must remain addressable even without a build-id hash"
    );
    assert!(
        dictionary.location_table.iter().skip(1).any(|location| {
            location.mapping_index == i32::try_from(mapping_index).expect("mapping index fits")
                && location.address != 0
        }),
        "a location must retain the non-default mapping and address"
    );

    let diagnostics = fs::read_dir(output_dir.path())
        .expect("read output directory")
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
                .then(|| fs::read(entry.path()).ok())
                .flatten()
        })
        .map(|bytes| serde_json::from_slice::<Value>(&bytes).expect("diagnostics JSON"))
        .collect::<Vec<_>>();
    assert!(!diagnostics.is_empty(), "record should emit diagnostics");
    assert!(
        diagnostics
            .iter()
            .any(|item| item["otlp"]["status"] == "partial")
    );
    assert!(
        diagnostics
            .iter()
            .any(|item| item["otlp"]["timeline_enabled"] == true)
    );
    assert!(diagnostics.iter().any(|item| {
        item["otlp"]["timeline_samples"]
            .as_u64()
            .unwrap_or_default()
            > 0
    }));
    assert!(
        diagnostics.iter().any(|item| {
            item["otlp"]["timeline_dropped_samples"]
                .as_u64()
                .unwrap_or_default()
                > 0
        }),
        "the one-sample cap must be visible in OTLP diagnostics"
    );
    assert!(diagnostics.iter().all(|item| {
        item["otlp"]["timeline_timestamp_errors"]
            .as_u64()
            .unwrap_or_default()
            == 0
    }));
    assert!(
        diagnostics
            .iter()
            .filter(|item| item["otlp"]["status"] == "partial")
            .all(|item| {
                item["otlp"]["rejected_profiles"]
                    .as_i64()
                    .unwrap_or_default()
                    == 1
            })
    );
    for item in diagnostics {
        assert!(!item.to_string().contains("test-secret"));
    }
}

#[test]
fn otlp_gzip_retry_reuses_one_decodable_payload() {
    let fixtures = tempdir().expect("fixture tempdir");
    let fixture = fixtures.path().join("cpu-target");
    compile_fixture(&fixture);
    let target = ChildGuard::spawn(&fixture);

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind OTLP fixture listener");
    let endpoint = format!(
        "http://{}/v1development/profiles",
        listener.local_addr().expect("OTLP listener address")
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_for_server = Arc::clone(&requests);
    let server = thread::spawn(move || serve(listener, requests_for_server));
    let output_dir = TempDir::new().expect("profile output tempdir");

    let output = profiler()
        .args([
            "record",
            "--pid",
            &target.pid().to_string(),
            "--profiles",
            "cpu",
            "--duration",
            "2s",
            "--window",
            "500ms",
            "--unwind",
            "fp",
            "--allow-partial",
            "--output",
            output_dir
                .path()
                .to_str()
                .expect("output path is valid UTF-8"),
            "--otlp-endpoint",
            &endpoint,
            "--otlp-compression",
            "gzip",
        ])
        .output()
        .expect("record command should be runnable");
    server.join().expect("OTLP server should finish");

    let requests = requests.lock().expect("request lock");
    if requests.is_empty() && preflight_unavailable(&output) {
        eprintln!(
            "skipping OTLP gzip runtime test because profiling preflight is unavailable: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    assert!(
        output.status.success(),
        "record with gzip OTLP export failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        requests.len() >= 2,
        "retryable HTTP failure must be retried"
    );
    let first = &requests[0];
    let second = &requests[1];
    for request in [first, second] {
        assert!(
            request
                .headers
                .to_ascii_lowercase()
                .contains("content-encoding: gzip"),
            "gzip export must advertise its content encoding: {}",
            request.headers
        );
    }
    assert_eq!(
        first.body, second.body,
        "a retry must reuse the same compressed OTLP payload"
    );
    let first_decoded = decoded_request_body(first);
    let second_decoded = decoded_request_body(second);
    assert_eq!(first_decoded, second_decoded);
    let payload = ExportRequest::decode(second_decoded.as_slice())
        .expect("retry gzip payload should decode as OTLP v1.11 protobuf");
    assert_eq!(payload.resource_profiles.len(), 1);
}
