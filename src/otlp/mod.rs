mod proto;

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, VecDeque},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use flate2::{Compression, write::GzEncoder};
use prost::Message;
use sha2::{Digest, Sha256};

use crate::{TargetKind, TargetMetadata, cli::OtlpArgs, pprof as pprof_proto};
use proto::{
    AnyValue, ArrayValue, ExportProfilesServiceRequest, ExportProfilesServiceResponse, Function,
    InstrumentationScope, KeyValue, KeyValueAndUnit, Line, Link, Location, Mapping, Profile,
    ProfilesDictionary, Resource, ResourceProfiles, Sample, ScopeProfiles, Stack, ValueType,
    any_value,
};

pub const PROTO_VERSION: &str = "1.11.0";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ATTEMPTS: u32 = 5;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAPPING_HASH_CACHE_CAPACITY: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum OtlpCompression {
    None,
    Gzip,
}

#[derive(Clone, Debug)]
pub struct OtlpConfig {
    endpoint: String,
    headers: Vec<(String, String)>,
    timeout: Duration,
    compression: OtlpCompression,
    ca: Option<PathBuf>,
    resource_attributes: BTreeMap<String, String>,
}

impl OtlpConfig {
    pub fn from_args(args: &OtlpArgs) -> Result<Option<Self>> {
        let endpoint = args
            .otlp_endpoint
            .clone()
            .or_else(|| std::env::var("OTEL_EXPORTER_OTLP_PROFILES_ENDPOINT").ok())
            .or_else(|| {
                std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
                    .ok()
                    .map(|base| append_profiles_path(&base))
            });
        let Some(endpoint) = endpoint else {
            return Ok(None);
        };
        validate_endpoint(&endpoint)?;

        let protocol = std::env::var("OTEL_EXPORTER_OTLP_PROFILES_PROTOCOL")
            .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL"))
            .unwrap_or_else(|_| "http/protobuf".to_owned());
        if protocol != "http/protobuf" {
            bail!("rustprofile OTLP Profiles supports only http/protobuf, not {protocol:?}");
        }

        let mut headers = parse_pairs(
            &std::env::var("OTEL_EXPORTER_OTLP_PROFILES_HEADERS")
                .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_HEADERS"))
                .unwrap_or_default(),
            ',',
            "OTLP header",
        )?;
        for header in &args.otlp_headers {
            insert_pair(&mut headers, header, "OTLP header")?;
        }
        for (key, value) in &headers {
            if key.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
                bail!("OTLP headers must not contain newlines");
            }
        }

        let timeout = if let Some(timeout) = args.otlp_timeout {
            timeout
        } else if let Ok(value) = std::env::var("OTEL_EXPORTER_OTLP_PROFILES_TIMEOUT")
            .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_TIMEOUT"))
        {
            Duration::from_millis(
                value
                    .parse::<u64>()
                    .context("OTLP timeout environment value must be milliseconds")?,
            )
        } else {
            DEFAULT_TIMEOUT
        };
        if timeout.is_zero() {
            bail!("OTLP timeout must be greater than zero");
        }

        let compression = args.otlp_compression.unwrap_or_else(|| {
            match std::env::var("OTEL_EXPORTER_OTLP_PROFILES_COMPRESSION")
                .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_COMPRESSION"))
                .as_deref()
            {
                Ok("none") => OtlpCompression::None,
                _ => OtlpCompression::Gzip,
            }
        });
        if let Ok(value) = std::env::var("OTEL_EXPORTER_OTLP_PROFILES_COMPRESSION")
            .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_COMPRESSION"))
            && value != "none"
            && value != "gzip"
            && args.otlp_compression.is_none()
        {
            bail!("unsupported OTLP compression {value:?}");
        }

        let ca = args.otlp_ca.clone().or_else(|| {
            std::env::var_os("OTEL_EXPORTER_OTLP_PROFILES_CERTIFICATE")
                .or_else(|| std::env::var_os("OTEL_EXPORTER_OTLP_CERTIFICATE"))
                .map(PathBuf::from)
        });
        if ca.as_ref().is_some_and(|path| !path.is_file()) {
            bail!("OTLP certificate file does not exist or is not a file");
        }

        let mut resource_attributes = parse_pairs(
            &std::env::var("OTEL_RESOURCE_ATTRIBUTES").unwrap_or_default(),
            ',',
            "resource attribute",
        )?;
        for attribute in &args.resource_attributes {
            insert_pair(&mut resource_attributes, attribute, "resource attribute")?;
        }
        if let Ok(service_name) = std::env::var("OTEL_SERVICE_NAME")
            && !service_name.is_empty()
        {
            resource_attributes.insert("service.name".to_owned(), service_name);
        }

        Ok(Some(Self {
            endpoint,
            headers: headers.into_iter().collect(),
            timeout,
            compression,
            ca,
            resource_attributes,
        }))
    }
}

#[derive(Clone)]
pub struct ExportPayload {
    pub bytes: Vec<u8>,
    pub profiles: u32,
}

#[derive(Default)]
pub struct MappingHashCache {
    entries: HashMap<FileIdentity, String>,
    order: VecDeque<FileIdentity>,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

pub fn encode_profiles(
    sources: &[&pprof_proto::Profile],
    target: &TargetMetadata,
    executable: &Path,
    config: &OtlpConfig,
    mapping_hashes: &mut MappingHashCache,
) -> Result<ExportPayload> {
    let mut dictionary = DictionaryBuilder::new(target.pid, mapping_hashes);
    let mut scope_profiles = Vec::new();
    let mut count = 0_u32;

    for source in sources {
        let lookup = SourceLookup::new(source);
        let stack_indices = source
            .sample
            .iter()
            .map(|sample| {
                let locations = sample
                    .location_id
                    .iter()
                    .filter_map(|id| lookup.locations.get(id).copied())
                    .map(|location| dictionary.intern_location(source, location, &lookup))
                    .collect::<Result<Vec<_>>>()?;
                Ok(dictionary.intern_stack(locations))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut profiles = Vec::new();
        for sample_index in 0..source.sample_type.len() {
            profiles.push(convert_profile(
                source,
                sample_index,
                &stack_indices,
                &mut dictionary,
            )?);
            count = count.saturating_add(1);
        }
        let default_sample_type = pprof_string(source, source.default_sample_type).to_owned();
        let order = (0..source.sample_type.len())
            .map(|index| AnyValue {
                value: Some(any_value::Value::IntValue(index as i64)),
            })
            .collect();
        let mut attributes = vec![KeyValue {
            key: "pprof.scope.sample_type_order".to_owned(),
            value: Some(AnyValue {
                value: Some(any_value::Value::ArrayValue(ArrayValue { values: order })),
            }),
        }];
        if !default_sample_type.is_empty() {
            attributes.push(string_key_value(
                "pprof.scope.default_sample_type",
                default_sample_type,
            ));
        }
        scope_profiles.push(ScopeProfiles {
            scope: Some(InstrumentationScope {
                name: "rustprofile".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                attributes,
                dropped_attributes_count: 0,
            }),
            profiles,
            schema_url: String::new(),
        });
    }

    let resource = resource(target, executable, &config.resource_attributes);
    let request = ExportProfilesServiceRequest {
        resource_profiles: vec![ResourceProfiles {
            resource: Some(resource),
            scope_profiles,
            schema_url: String::new(),
        }],
        dictionary: Some(dictionary.finish()),
    };
    Ok(ExportPayload {
        bytes: request.encode_to_vec(),
        profiles: count,
    })
}

fn convert_profile(
    source: &pprof_proto::Profile,
    sample_index: usize,
    stack_indices: &[i32],
    dictionary: &mut DictionaryBuilder,
) -> Result<Profile> {
    let sample_type = source
        .sample_type
        .get(sample_index)
        .context("pprof sample type index is out of bounds")?;
    let sample_type = ValueType {
        type_strindex: dictionary.intern_string(pprof_string(source, sample_type.r#type)),
        unit_strindex: dictionary.intern_string(pprof_string(source, sample_type.unit)),
    };
    let period_type = source.period_type.as_ref().map(|value| ValueType {
        type_strindex: dictionary.intern_string(pprof_string(source, value.r#type)),
        unit_strindex: dictionary.intern_string(pprof_string(source, value.unit)),
    });
    let mut samples = Vec::with_capacity(source.sample.len());
    for (sample, stack_index) in source.sample.iter().zip(stack_indices.iter().copied()) {
        samples.push(Sample {
            stack_index,
            attribute_indices: Vec::new(),
            link_index: 0,
            values: vec![sample.value.get(sample_index).copied().unwrap_or_default()],
            timestamps_unix_nano: Vec::new(),
        });
    }
    Ok(Profile {
        sample_type: Some(sample_type),
        samples,
        time_unix_nano: u64::try_from(source.time_nanos).unwrap_or_default(),
        duration_nano: u64::try_from(source.duration_nanos).unwrap_or_default(),
        period_type,
        period: source.period,
        profile_id: profile_id()?,
        dropped_attributes_count: 0,
        original_payload_format: String::new(),
        original_payload: Vec::new(),
        attribute_indices: Vec::new(),
    })
}

struct SourceLookup<'a> {
    locations: HashMap<u64, &'a pprof_proto::Location>,
    mappings: HashMap<u64, &'a pprof_proto::Mapping>,
    functions: HashMap<u64, &'a pprof_proto::Function>,
}

impl<'a> SourceLookup<'a> {
    fn new(source: &'a pprof_proto::Profile) -> Self {
        Self {
            locations: source
                .location
                .iter()
                .map(|location| (location.id, location))
                .collect(),
            mappings: source
                .mapping
                .iter()
                .map(|mapping| (mapping.id, mapping))
                .collect(),
            functions: source
                .function
                .iter()
                .map(|function| (function.id, function))
                .collect(),
        }
    }
}

struct DictionaryBuilder<'a> {
    process_pid: i32,
    mapping_hashes: &'a mut MappingHashCache,
    strings: Vec<String>,
    string_index: HashMap<String, i32>,
    mappings: Vec<Mapping>,
    mapping_index: HashMap<(u64, u64, u64, String, String), i32>,
    locations: Vec<Location>,
    location_index: HashMap<(u64, i32, Vec<(i32, i64, i64)>), i32>,
    functions: Vec<Function>,
    function_index: HashMap<(String, String, String, i64), i32>,
    attributes: Vec<KeyValueAndUnit>,
    attribute_index: HashMap<(String, String), i32>,
    stacks: Vec<Stack>,
    stack_index: HashMap<Vec<i32>, i32>,
}

impl<'a> DictionaryBuilder<'a> {
    fn new(process_pid: i32, mapping_hashes: &'a mut MappingHashCache) -> Self {
        let mut string_index = HashMap::new();
        string_index.insert(String::new(), 0);
        Self {
            process_pid,
            mapping_hashes,
            strings: vec![String::new()],
            string_index,
            mappings: vec![Mapping::default()],
            mapping_index: HashMap::new(),
            locations: vec![Location::default()],
            location_index: HashMap::new(),
            functions: vec![Function::default()],
            function_index: HashMap::new(),
            attributes: vec![KeyValueAndUnit::default()],
            attribute_index: HashMap::new(),
            stacks: vec![Stack::default()],
            stack_index: HashMap::new(),
        }
    }

    fn intern_string(&mut self, value: impl AsRef<str>) -> i32 {
        let value = value.as_ref();
        if let Some(index) = self.string_index.get(value) {
            return *index;
        }
        let index = self.strings.len() as i32;
        self.strings.push(value.to_owned());
        self.string_index.insert(value.to_owned(), index);
        index
    }

    fn intern_attribute_string(&mut self, key: &str, value: String) -> i32 {
        let cache_key = (key.to_owned(), format!("s:{value}"));
        if let Some(index) = self.attribute_index.get(&cache_key) {
            return *index;
        }
        let entry = KeyValueAndUnit {
            key_strindex: self.intern_string(key),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(value)),
            }),
            unit_strindex: 0,
        };
        let index = self.attributes.len() as i32;
        self.attributes.push(entry);
        self.attribute_index.insert(cache_key, index);
        index
    }

    fn intern_attribute_bool(&mut self, key: &str, value: bool) -> i32 {
        let cache_key = (key.to_owned(), format!("b:{value}"));
        if let Some(index) = self.attribute_index.get(&cache_key) {
            return *index;
        }
        let entry = KeyValueAndUnit {
            key_strindex: self.intern_string(key),
            value: Some(AnyValue {
                value: Some(any_value::Value::BoolValue(value)),
            }),
            unit_strindex: 0,
        };
        let index = self.attributes.len() as i32;
        self.attributes.push(entry);
        self.attribute_index.insert(cache_key, index);
        index
    }

    fn intern_mapping(
        &mut self,
        source: &pprof_proto::Profile,
        mapping: &pprof_proto::Mapping,
    ) -> i32 {
        let filename = pprof_string(source, mapping.filename).to_owned();
        let build_id = pprof_string(source, mapping.build_id).to_owned();
        let key = (
            mapping.memory_start,
            mapping.memory_limit,
            mapping.file_offset,
            filename.clone(),
            build_id.clone(),
        );
        if let Some(index) = self.mapping_index.get(&key) {
            return *index;
        }
        let mut attributes = Vec::new();
        if !build_id.is_empty() {
            attributes
                .push(self.intern_attribute_string("process.executable.build_id.gnu", build_id));
        }
        if let Ok(hash) = htlhash(
            self.process_pid,
            mapping.memory_start,
            mapping.memory_limit,
            self.mapping_hashes,
        ) {
            attributes
                .push(self.intern_attribute_string("process.executable.build_id.htlhash", hash));
        }
        if attributes.is_empty() {
            return 0;
        }
        attributes
            .push(self.intern_attribute_bool("pprof.mapping.has_functions", mapping.has_functions));
        attributes
            .push(self.intern_attribute_bool("pprof.mapping.has_filenames", mapping.has_filenames));
        attributes.push(
            self.intern_attribute_bool("pprof.mapping.has_line_numbers", mapping.has_line_numbers),
        );
        attributes.push(
            self.intern_attribute_bool(
                "pprof.mapping.has_inline_frames",
                mapping.has_inline_frames,
            ),
        );
        let filename_strindex = self.intern_string(filename);
        let index = self.mappings.len() as i32;
        self.mappings.push(Mapping {
            memory_start: mapping.memory_start,
            memory_limit: mapping.memory_limit,
            file_offset: mapping.file_offset,
            filename_strindex,
            attribute_indices: attributes,
        });
        self.mapping_index.insert(key, index);
        index
    }

    fn intern_function(
        &mut self,
        source: &pprof_proto::Profile,
        function: &pprof_proto::Function,
    ) -> i32 {
        let key = (
            pprof_string(source, function.name).to_owned(),
            pprof_string(source, function.system_name).to_owned(),
            pprof_string(source, function.filename).to_owned(),
            function.start_line,
        );
        if let Some(index) = self.function_index.get(&key) {
            return *index;
        }
        let entry = Function {
            name_strindex: self.intern_string(&key.0),
            system_name_strindex: self.intern_string(&key.1),
            filename_strindex: self.intern_string(&key.2),
            start_line: key.3,
        };
        let index = self.functions.len() as i32;
        self.functions.push(entry);
        self.function_index.insert(key, index);
        index
    }

    fn intern_location(
        &mut self,
        source: &pprof_proto::Profile,
        location: &pprof_proto::Location,
        lookup: &SourceLookup<'_>,
    ) -> Result<i32> {
        let mapping_index = lookup
            .mappings
            .get(&location.mapping_id)
            .map(|mapping| self.intern_mapping(source, mapping))
            .unwrap_or_default();
        let mut lines = Vec::new();
        let mut line_key = Vec::new();
        for line in &location.line {
            let function_index = lookup
                .functions
                .get(&line.function_id)
                .map(|function| self.intern_function(source, function))
                .unwrap_or_default();
            lines.push(Line {
                function_index,
                line: line.line,
                column: line.column,
            });
            line_key.push((function_index, line.line, line.column));
        }
        let key = (location.address, mapping_index, line_key);
        if let Some(index) = self.location_index.get(&key) {
            return Ok(*index);
        }
        let attribute_indices = location
            .is_folded
            .then(|| self.intern_attribute_bool("pprof.location.is_folded", true))
            .into_iter()
            .collect();
        let index = self.locations.len() as i32;
        self.locations.push(Location {
            mapping_index,
            address: location.address,
            lines,
            attribute_indices,
        });
        self.location_index.insert(key, index);
        Ok(index)
    }

    fn intern_stack(&mut self, locations: Vec<i32>) -> i32 {
        if let Some(index) = self.stack_index.get(&locations) {
            return *index;
        }
        let index = self.stacks.len() as i32;
        self.stacks.push(Stack {
            location_indices: locations.clone(),
        });
        self.stack_index.insert(locations, index);
        index
    }

    fn finish(self) -> ProfilesDictionary {
        ProfilesDictionary {
            mapping_table: self.mappings,
            location_table: self.locations,
            function_table: self.functions,
            link_table: vec![Link {
                trace_id: vec![0; 16],
                span_id: vec![0; 8],
            }],
            string_table: self.strings,
            attribute_table: self.attributes,
            stack_table: self.stacks,
        }
    }
}

pub struct ExportClient {
    config: OtlpConfig,
    agent: ureq::Agent,
}

#[derive(Clone, Debug)]
pub struct ExportOutcome {
    pub delivered: bool,
    pub attempts: u32,
    pub rejected_profiles: i64,
    pub error: Option<String>,
}

impl ExportClient {
    pub fn new(config: OtlpConfig) -> Result<Self> {
        let mut builder = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(config.timeout));
        if config.endpoint.starts_with("https://") {
            let mut certificates = rustls_native_certs::load_native_certs()
                .certs
                .into_iter()
                .map(|certificate| {
                    ureq::tls::Certificate::from_der(certificate.as_ref()).to_owned()
                })
                .collect::<Vec<_>>();
            if let Some(ca_path) = config.ca.as_deref() {
                let pem = fs::read(ca_path)
                    .with_context(|| format!("failed to read OTLP CA {}", ca_path.display()))?;
                for item in ureq::tls::parse_pem(&pem) {
                    if let ureq::tls::PemItem::Certificate(certificate) =
                        item.context("failed to parse OTLP CA")?
                    {
                        certificates.push(certificate);
                    }
                }
            }
            if certificates.is_empty() {
                bail!("no system or custom trust roots were loaded for the OTLP endpoint");
            }
            builder = builder.tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::new_with_certs(&certificates))
                    .build(),
            );
        }
        let agent = builder.build().new_agent();
        Ok(Self { config, agent })
    }

    pub fn export(&self, payload: &ExportPayload, stopping: &Arc<AtomicBool>) -> ExportOutcome {
        let (body, compressed) = match self.prepare_body(payload) {
            Ok(prepared) => prepared,
            Err(error) => {
                return ExportOutcome {
                    delivered: false,
                    attempts: 0,
                    rejected_profiles: 0,
                    error: Some(error.message),
                };
            }
        };
        let mut last_error = None;
        let mut attempts = 0;
        for attempt in 1..=MAX_ATTEMPTS {
            if attempt > 1 && stopping.load(Ordering::Relaxed) {
                break;
            }
            attempts = attempt;
            match self.send_once(&body, compressed) {
                Ok((rejected, message)) => {
                    return ExportOutcome {
                        delivered: true,
                        attempts: attempt,
                        rejected_profiles: rejected,
                        error: message,
                    };
                }
                Err(error) => {
                    let retryable = error.retryable;
                    last_error = Some(error.message);
                    if !retryable || attempt == MAX_ATTEMPTS || stopping.load(Ordering::Relaxed) {
                        break;
                    }
                    let delay = error
                        .retry_after
                        .unwrap_or_else(|| retry_delay(attempt))
                        .min(MAX_RETRY_DELAY);
                    sleep_interruptibly(delay, stopping);
                }
            }
        }
        ExportOutcome {
            delivered: false,
            attempts,
            rejected_profiles: 0,
            error: last_error,
        }
    }

    fn prepare_body<'a>(
        &self,
        payload: &'a ExportPayload,
    ) -> std::result::Result<(Cow<'a, [u8]>, bool), SendError> {
        Ok(match self.config.compression {
            OtlpCompression::None => (Cow::Borrowed(&payload.bytes), false),
            OtlpCompression::Gzip => {
                let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                encoder.write_all(&payload.bytes).map_err(SendError::io)?;
                (Cow::Owned(encoder.finish().map_err(SendError::io)?), true)
            }
        })
    }

    fn send_once(
        &self,
        body: &[u8],
        compressed: bool,
    ) -> std::result::Result<(i64, Option<String>), SendError> {
        let mut request = self
            .agent
            .post(&self.config.endpoint)
            .header("Content-Type", "application/x-protobuf")
            .header(
                "User-Agent",
                &format!(
                    "rustprofile/{} otlp-proto/{PROTO_VERSION}",
                    env!("CARGO_PKG_VERSION")
                ),
            );
        if compressed {
            request = request.header("Content-Encoding", "gzip");
        }
        for (key, value) in &self.config.headers {
            request = request.header(key, value);
        }
        let response = request.send(body).map_err(|error| SendError {
            retryable: retryable_transport_error(&error),
            retry_after: None,
            message: format!("OTLP request failed: {error}"),
        })?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("Retry-After")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs);
        if status != 200 {
            return Err(SendError {
                retryable: matches!(status, 408 | 429 | 502 | 503 | 504),
                retry_after,
                message: format!("OTLP endpoint returned HTTP {status}"),
            });
        }
        let bytes = response
            .into_body()
            .with_config()
            .limit(MAX_RESPONSE_BYTES as u64)
            .read_to_vec()
            .map_err(|error| SendError {
                retryable: false,
                retry_after: None,
                message: format!("failed to read OTLP response: {error}"),
            })?;
        if bytes.is_empty() {
            return Ok((0, None));
        }
        let response =
            ExportProfilesServiceResponse::decode(bytes.as_slice()).map_err(|error| SendError {
                retryable: false,
                retry_after: None,
                message: format!("failed to decode OTLP response: {error}"),
            })?;
        let Some(partial) = response.partial_success else {
            return Ok((0, None));
        };
        Ok((
            partial.rejected_profiles,
            (!partial.error_message.is_empty()).then_some(partial.error_message),
        ))
    }
}

fn retryable_transport_error(error: &ureq::Error) -> bool {
    matches!(
        error,
        ureq::Error::Io(_)
            | ureq::Error::Timeout(_)
            | ureq::Error::HostNotFound
            | ureq::Error::Protocol(_)
            | ureq::Error::ConnectionFailed
    )
}

struct SendError {
    retryable: bool,
    retry_after: Option<Duration>,
    message: String,
}

impl SendError {
    fn io(error: std::io::Error) -> Self {
        Self {
            retryable: false,
            retry_after: None,
            message: error.to_string(),
        }
    }
}

fn resource(
    target: &TargetMetadata,
    executable: &Path,
    configured: &BTreeMap<String, String>,
) -> Resource {
    let mut attributes = configured.clone();
    attributes
        .entry("service.name".to_owned())
        .or_insert_with(|| {
            executable
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| format!("unknown_service:{name}"))
                .unwrap_or_else(|| "unknown_service".to_owned())
        });
    attributes.insert(
        "process.executable.path".to_owned(),
        executable.to_string_lossy().into_owned(),
    );
    if let Some(name) = executable.file_name().and_then(|name| name.to_str()) {
        attributes.insert("process.executable.name".to_owned(), name.to_owned());
    }
    if let Some(id) = &target.container_id {
        attributes.insert("container.id".to_owned(), id.clone());
    }
    if let Some(name) = &target.container_name {
        attributes.insert("container.name".to_owned(), name.clone());
    }
    for (key, value) in [
        ("k8s.namespace.name", target.k8s_namespace.as_ref()),
        ("k8s.pod.name", target.k8s_pod_name.as_ref()),
        ("k8s.pod.uid", target.k8s_pod_uid.as_ref()),
        ("k8s.container.name", target.k8s_container_name.as_ref()),
        ("k8s.node.name", target.k8s_node_name.as_ref()),
    ] {
        if let Some(value) = value {
            attributes.insert(key.to_owned(), value.clone());
        }
    }
    let mut values = attributes
        .into_iter()
        .map(|(key, value)| string_key_value(key, value))
        .collect::<Vec<_>>();
    values.push(KeyValue {
        key: "process.pid".to_owned(),
        value: Some(AnyValue {
            value: Some(any_value::Value::IntValue(i64::from(target.pid))),
        }),
    });
    values.push(string_key_value(
        "rustprofile.target.kind",
        match target.kind {
            TargetKind::Process => "process",
            TargetKind::Docker => "docker",
            TargetKind::Kubernetes => "kubernetes",
        },
    ));
    Resource {
        attributes: values,
        dropped_attributes_count: 0,
    }
}

fn string_key_value(key: impl Into<String>, value: impl Into<String>) -> KeyValue {
    KeyValue {
        key: key.into(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.into())),
        }),
    }
}

fn pprof_string(profile: &pprof_proto::Profile, index: i64) -> &str {
    usize::try_from(index)
        .ok()
        .and_then(|index| profile.string_table.get(index))
        .map(String::as_str)
        .unwrap_or_default()
}

fn profile_id() -> Result<Vec<u8>> {
    let mut id = vec![0_u8; 16];
    File::open("/dev/urandom")
        .context("failed to open system random source")?
        .read_exact(&mut id)
        .context("failed to create OTLP profile ID")?;
    Ok(id)
}

fn htlhash(pid: i32, start: u64, limit: u64, cache: &mut MappingHashCache) -> Result<String> {
    let path = format!("/proc/{pid}/map_files/{start:x}-{limit:x}");
    let mut file = File::open(&path).with_context(|| format!("failed to open {path}"))?;
    let metadata = file.metadata()?;
    let identity = FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        length: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    };
    if let Some(hash) = cache.entries.get(&identity) {
        return Ok(hash.clone());
    }
    let length = metadata.len();
    let page = 4096_u64.min(length) as usize;
    let mut first = vec![0_u8; page];
    file.read_exact(&mut first)?;
    let mut last = vec![0_u8; page];
    file.seek(SeekFrom::Start(length.saturating_sub(page as u64)))?;
    file.read_exact(&mut last)?;
    let mut hash = Sha256::new();
    hash.update(first);
    hash.update(last);
    hash.update(length.to_be_bytes());
    let hash = hex::encode(&hash.finalize()[..16]);
    if cache.entries.len() >= MAPPING_HASH_CACHE_CAPACITY {
        if let Some(expired) = cache.order.pop_front() {
            cache.entries.remove(&expired);
        }
    }
    cache.entries.insert(identity, hash.clone());
    cache.order.push_back(identity);
    Ok(hash)
}

fn append_profiles_path(base: &str) -> String {
    format!("{}/v1development/profiles", base.trim_end_matches('/'))
}

fn validate_endpoint(endpoint: &str) -> Result<()> {
    if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
        bail!("OTLP endpoint must be an http:// or https:// URL");
    }
    let authority = endpoint
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or_default())
        .unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        bail!("OTLP endpoint must have a host and must not contain credentials");
    }
    Ok(())
}

fn parse_pairs(input: &str, delimiter: char, field: &str) -> Result<BTreeMap<String, String>> {
    let mut pairs = BTreeMap::new();
    for value in input
        .split(delimiter)
        .filter(|value| !value.trim().is_empty())
    {
        insert_pair(&mut pairs, value, field)?;
    }
    Ok(pairs)
}

fn insert_pair(pairs: &mut BTreeMap<String, String>, input: &str, field: &str) -> Result<()> {
    let (key, value) = input
        .split_once('=')
        .with_context(|| format!("{field} must use KEY=VALUE format"))?;
    let key = key.trim();
    if key.is_empty() {
        bail!("{field} key must not be empty");
    }
    pairs.insert(key.to_owned(), value.trim().to_owned());
    Ok(())
}

fn retry_delay(attempt: u32) -> Duration {
    let base_ms = 500_u64.saturating_mul(1_u64 << attempt.saturating_sub(1).min(6));
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_millis() as u64
        % 250;
    Duration::from_millis(base_ms.min(30_000).saturating_add(jitter))
}

fn sleep_interruptibly(duration: Duration, stopping: &Arc<AtomicBool>) {
    let now = std::time::Instant::now();
    let Some(deadline) = now.checked_add(duration) else {
        return;
    };
    while std::time::Instant::now() < deadline && !stopping.load(Ordering::Relaxed) {
        thread::sleep(
            Duration::from_millis(100)
                .min(deadline.saturating_duration_since(std::time::Instant::now())),
        );
    }
}
