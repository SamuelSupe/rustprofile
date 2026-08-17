use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use json_slabs::{MAGIC, ParsedFile, SlabPlaceholder, SlabType};
use serde::Serialize;
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};

const MAX_PROFILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_DECODED_PROFILE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DIAGNOSTIC_BYTES: u64 = 1024 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 16_384;
const MAX_GALLERY_PROFILES: usize = 4096;
const MAX_EXPANDED_VALUES: usize = 4_000_000;
const MAX_VIEWER_SAMPLES: usize = 65_536;
const MAX_VIEWER_STACKS: usize = 65_536;
const MAX_VIEWER_FUNCTIONS: usize = 262_144;
const MAX_VIEWER_THREADS: usize = 4096;
const MAX_STACK_DEPTH: usize = 512;

pub const VIEWER_HTML: &str = include_str!("flamegraph.html");

#[derive(Clone)]
pub enum Source {
    Profile(PathBuf),
    Directory(PathBuf),
}

#[derive(Clone, Serialize)]
pub struct ProfileEntry {
    pub id: String,
    pub filename: String,
    pub session: String,
    pub window_index: Option<u64>,
    pub started_unix_nanos: Option<i64>,
    pub format: &'static str,
    pub compressed_bytes: u64,
    pub samples: Option<u64>,
    pub dropped_samples: Option<u64>,
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Serialize)]
pub struct ViewerProfile {
    pub profile: ProfileEntry,
    pub start_time_unix_millis: Option<f64>,
    pub interval_millis: Option<f64>,
    pub functions: Vec<String>,
    pub stacks: Vec<Vec<usize>>,
    pub threads: Vec<ViewerThread>,
    pub sample_count: usize,
    pub truncated: bool,
}

#[derive(Serialize)]
pub struct ViewerThread {
    pub pid: String,
    pub tid: String,
    pub name: String,
    pub times_millis: Vec<f64>,
    pub weights: Vec<i64>,
    pub cpu_delta_micros: Vec<f64>,
    pub stack_indices: Vec<Option<usize>>,
}

pub fn entries(source: &Source) -> Result<Vec<ProfileEntry>> {
    let mut entries = match source {
        Source::Profile(path) => vec![entry_for(path)?],
        Source::Directory(directory) => {
            let mut entries = Vec::new();
            for item in fs::read_dir(directory)
                .with_context(|| format!("failed to read {}", directory.display()))?
                .take(MAX_DIRECTORY_ENTRIES)
            {
                let Ok(item) = item else { continue };
                let path = item.path();
                if item.file_type().is_ok_and(|kind| kind.is_file())
                    && is_firefox_profile(&path)
                    && let Ok(entry) = entry_for(&path)
                {
                    entries.push(entry);
                }
            }
            entries
        }
    };
    entries.sort_by(|a, b| {
        b.started_unix_nanos
            .cmp(&a.started_unix_nanos)
            .then_with(|| b.filename.cmp(&a.filename))
    });
    entries.truncate(MAX_GALLERY_PROFILES);
    Ok(entries)
}

pub fn find(source: &Source, id: &str) -> Result<Option<ProfileEntry>> {
    Ok(entries(source)?.into_iter().find(|entry| entry.id == id))
}

pub fn decode(entry: ProfileEntry) -> Result<ViewerProfile> {
    let bytes = read_gzip_bounded(&entry.path)?;
    let root = if bytes.starts_with(&MAGIC) {
        let parsed = ParsedFile::parse(&bytes).context("invalid JSLB profile")?;
        let value: Value =
            serde_json::from_slice(parsed.root_json_bytes()).context("invalid JSLB root JSON")?;
        expand_slabs(
            value,
            &parsed,
            0,
            &mut HashSet::new(),
            &mut ExpansionBudget::new(),
        )?
    } else {
        let value = serde_json::from_slice(&bytes).context("invalid Firefox profile JSON")?;
        validate_value_budget(&value, 0, &mut ExpansionBudget::new())?;
        value
    };
    decode_root(entry, &root)
}

fn decode_root(entry: ProfileEntry, root: &Value) -> Result<ViewerProfile> {
    let root = root.as_object().context("profile root is not an object")?;
    let shared = object_map(root, "shared")?;
    let strings = string_array(field(shared, "stringArray")?)?;
    let stack_table = object_map(shared, "stackTable")?;
    let frame_table = object_map(shared, "frameTable")?;
    let func_table = object_map(shared, "funcTable")?;
    let prefix_offsets = integer_array(field(stack_table, "prefixOffset")?)?;
    let stack_frames = integer_array(field(stack_table, "frame")?)?;
    let frame_funcs = integer_array(field(frame_table, "func")?)?;
    let func_names = integer_array(field(func_table, "name")?)?;
    if prefix_offsets.len() > MAX_VIEWER_STACKS {
        bail!("profile contains too many stacks for the viewer");
    }
    if func_names.len() > MAX_VIEWER_FUNCTIONS {
        bail!("profile contains too many functions for the viewer");
    }

    let mut functions = Vec::with_capacity(func_names.len());
    for name in func_names {
        functions.push(
            usize::try_from(name)
                .ok()
                .and_then(|index| strings.get(index))
                .cloned()
                .unwrap_or_else(|| "<unknown>".to_owned()),
        );
    }
    let mut stacks = Vec::with_capacity(prefix_offsets.len().min(stack_frames.len()));
    for stack_index in 0..prefix_offsets.len().min(stack_frames.len()) {
        let mut reversed = Vec::new();
        let mut current = Some(stack_index);
        let mut seen = HashSet::new();
        while let Some(index) = current {
            if reversed.len() == MAX_STACK_DEPTH || !seen.insert(index) {
                break;
            }
            let Some(frame_index) = stack_frames
                .get(index)
                .and_then(|value| usize::try_from(*value).ok())
            else {
                break;
            };
            if let Some(function_index) = frame_funcs
                .get(frame_index)
                .and_then(|value| usize::try_from(*value).ok())
            {
                reversed.push(function_index);
            }
            let offset = prefix_offsets[index];
            current = if offset > 0 {
                usize::try_from(offset)
                    .ok()
                    .and_then(|offset| index.checked_sub(offset))
            } else {
                None
            };
        }
        reversed.reverse();
        stacks.push(reversed);
    }

    let mut threads = Vec::new();
    let mut sample_count = 0usize;
    let mut truncated = false;
    let profile_threads = field(root, "threads")?
        .as_array()
        .context("threads is not an array")?;
    if profile_threads.len() > MAX_VIEWER_THREADS {
        bail!("profile contains too many threads for the viewer");
    }
    for thread in profile_threads {
        let thread = thread.as_object().context("thread is not an object")?;
        let samples = object_map(thread, "samples")?;
        let stack_indices = optional_integer_array(field(samples, "stack")?)?;
        let weights = integer_array(field(samples, "weight")?)?;
        let time_deltas = float_array(field(samples, "timeDeltas")?)?;
        let cpu_deltas = samples
            .get("threadCPUDelta")
            .map(float_array)
            .transpose()?
            .unwrap_or_default();
        let available = stack_indices
            .len()
            .min(weights.len())
            .min(time_deltas.len());
        let take = available.min(MAX_VIEWER_SAMPLES.saturating_sub(sample_count));
        truncated |= take < available;
        let mut elapsed = 0.0;
        let mut times = Vec::with_capacity(take);
        for delta in time_deltas.into_iter().take(take) {
            if delta < 0.0 {
                bail!("profile contains a negative sample time delta");
            }
            elapsed += delta;
            if !elapsed.is_finite() {
                bail!("profile sample time exceeds the supported range");
            }
            times.push(elapsed);
        }
        threads.push(ViewerThread {
            pid: display_id(thread.get("pid")),
            tid: display_id(thread.get("tid")),
            name: thread
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Unnamed thread")
                .to_owned(),
            times_millis: times,
            weights: weights.into_iter().take(take).collect(),
            cpu_delta_micros: cpu_deltas.into_iter().take(take).collect(),
            stack_indices: stack_indices
                .into_iter()
                .take(take)
                .map(|index| index.and_then(|value| usize::try_from(value).ok()))
                .collect(),
        });
        sample_count += take;
        if sample_count == MAX_VIEWER_SAMPLES {
            truncated |= threads.len() < profile_threads.len();
            break;
        }
    }
    let meta = root.get("meta").and_then(Value::as_object);
    Ok(ViewerProfile {
        profile: entry,
        start_time_unix_millis: meta
            .and_then(|meta| meta.get("startTime"))
            .and_then(Value::as_f64),
        interval_millis: meta
            .and_then(|meta| meta.get("interval"))
            .and_then(Value::as_f64),
        functions,
        stacks,
        threads,
        sample_count,
        truncated,
    })
}

fn entry_for(path: &Path) -> Result<ProfileEntry> {
    let metadata = path
        .metadata()
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.len() > MAX_PROFILE_BYTES {
        bail!(
            "profile {} is {} bytes, exceeding the {} byte serve limit",
            path.display(),
            metadata.len(),
            MAX_PROFILE_BYTES
        );
    }
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("profile filename is not valid UTF-8")?
        .to_owned();
    let stem = filename
        .strip_prefix("firefox-")
        .unwrap_or(&filename)
        .strip_suffix(".json.gz")
        .or_else(|| {
            filename
                .strip_prefix("firefox-")
                .unwrap_or(&filename)
                .strip_suffix(".jslb.gz")
        })
        .unwrap_or(&filename);
    let mut parts = stem.rsplitn(3, '-');
    let started_unix_nanos = parts.next().and_then(|value| value.parse().ok());
    let window_index = parts.next().and_then(|value| value.parse().ok());
    let session = parts.next().unwrap_or(stem).to_owned();
    let (samples, dropped_samples) = diagnostics(path, stem);
    Ok(ProfileEntry {
        id: hex::encode(Sha256::digest(filename.as_bytes())),
        filename,
        session,
        window_index,
        started_unix_nanos,
        format: if path.to_string_lossy().ends_with(".jslb.gz") {
            "jslb"
        } else {
            "json"
        },
        compressed_bytes: metadata.len(),
        samples,
        dropped_samples,
        path: path.to_owned(),
    })
}

fn diagnostics(path: &Path, stem: &str) -> (Option<u64>, Option<u64>) {
    let Some(parent) = path.parent() else {
        return (None, None);
    };
    let Ok(file) = fs::File::open(parent.join(format!("diagnostics-{stem}.json"))) else {
        return (None, None);
    };
    let mut bytes = Vec::new();
    if file
        .take(MAX_DIAGNOSTIC_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_DIAGNOSTIC_BYTES
    {
        return (None, None);
    }
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return (None, None);
    };
    let firefox = value.get("firefox");
    (
        firefox
            .and_then(|value| value.get("samples"))
            .and_then(Value::as_u64),
        firefox
            .and_then(|value| value.get("dropped_samples"))
            .and_then(Value::as_u64),
    )
}

fn is_firefox_profile(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with("firefox-")
                && (name.ends_with(".json.gz") || name.ends_with(".jslb.gz"))
        })
}

fn read_gzip_bounded(path: &Path) -> Result<Vec<u8>> {
    let file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut decoder = GzDecoder::new(file).take(MAX_DECODED_PROFILE_BYTES + 1);
    let mut bytes = Vec::new();
    decoder
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to decompress {}", path.display()))?;
    if bytes.len() as u64 > MAX_DECODED_PROFILE_BYTES {
        bail!(
            "decompressed profile exceeds the {} byte viewer limit",
            MAX_DECODED_PROFILE_BYTES
        );
    }
    Ok(bytes)
}

fn expand_slabs(
    value: Value,
    parsed: &ParsedFile<'_>,
    depth: usize,
    active: &mut HashSet<usize>,
    budget: &mut ExpansionBudget,
) -> Result<Value> {
    if depth > 64 {
        bail!("JSLB nesting exceeds the viewer limit");
    }
    match value {
        Value::Object(mut object) if object.len() == 1 && object.contains_key("$s") => {
            let index = object
                .remove("$s")
                .and_then(|value| value.as_u64())
                .and_then(|value| usize::try_from(value).ok())
                .context("invalid JSLB placeholder")?;
            if !active.insert(index) {
                bail!("cyclic JSLB placeholder {index}");
            }
            let slab = parsed.slab_at(SlabPlaceholder(index))?;
            let expanded = match slab.slab_type {
                SlabType::Json => {
                    let value =
                        serde_json::from_slice(slab.bytes).context("invalid JSLB sub-JSON")?;
                    expand_slabs(value, parsed, depth + 1, active, budget)?
                }
                _ => {
                    budget.consume(slab.element_count())?;
                    Value::Array(slab_values(slab.slab_type, slab.bytes)?)
                }
            };
            active.remove(&index);
            Ok(expanded)
        }
        Value::Object(object) => {
            budget.consume(object.len())?;
            Ok(Value::Object(
                object
                    .into_iter()
                    .map(|(key, value)| {
                        Ok((key, expand_slabs(value, parsed, depth + 1, active, budget)?))
                    })
                    .collect::<Result<Map<String, Value>>>()?,
            ))
        }
        Value::Array(array) => {
            budget.consume(array.len())?;
            Ok(Value::Array(
                array
                    .into_iter()
                    .map(|value| expand_slabs(value, parsed, depth + 1, active, budget))
                    .collect::<Result<Vec<_>>>()?,
            ))
        }
        value => {
            budget.consume(1)?;
            Ok(value)
        }
    }
}

struct ExpansionBudget {
    remaining: usize,
}

impl ExpansionBudget {
    fn new() -> Self {
        Self {
            remaining: MAX_EXPANDED_VALUES,
        }
    }

    fn consume(&mut self, count: usize) -> Result<()> {
        self.remaining = self
            .remaining
            .checked_sub(count)
            .context("profile expands beyond the viewer value limit")?;
        Ok(())
    }
}

fn validate_value_budget(value: &Value, depth: usize, budget: &mut ExpansionBudget) -> Result<()> {
    if depth > 64 {
        bail!("JSON nesting exceeds the viewer limit");
    }
    match value {
        Value::Object(object) => {
            budget.consume(object.len())?;
            for value in object.values() {
                validate_value_budget(value, depth + 1, budget)?;
            }
        }
        Value::Array(array) => {
            budget.consume(array.len())?;
            for value in array {
                validate_value_budget(value, depth + 1, budget)?;
            }
        }
        _ => budget.consume(1)?,
    }
    Ok(())
}

fn slab_values(kind: SlabType, bytes: &[u8]) -> Result<Vec<Value>> {
    let values = match kind {
        SlabType::Int8 => bytes
            .iter()
            .map(|value| Value::from(*value as i8))
            .collect(),
        SlabType::Uint8 => bytes.iter().map(|value| Value::from(*value)).collect(),
        SlabType::Int16 => chunks(bytes, i16::from_le_bytes, Value::from),
        SlabType::Uint16 => chunks(bytes, u16::from_le_bytes, Value::from),
        SlabType::Int32 => chunks(bytes, i32::from_le_bytes, Value::from),
        SlabType::Uint32 => chunks(bytes, u32::from_le_bytes, Value::from),
        SlabType::Int64 => chunks(bytes, i64::from_le_bytes, Value::from),
        SlabType::Uint64 => chunks(bytes, u64::from_le_bytes, Value::from),
        SlabType::Float32 => float_chunks(bytes, f32::from_le_bytes, |value| value as f64)?,
        SlabType::Float64 => float_chunks(bytes, f64::from_le_bytes, |value| value)?,
        SlabType::Json => unreachable!(),
    };
    Ok(values)
}

fn chunks<const N: usize, T: Copy>(
    bytes: &[u8],
    decode: fn([u8; N]) -> T,
    into: fn(T) -> Value,
) -> Vec<Value> {
    bytes
        .chunks_exact(N)
        .map(|chunk| into(decode(chunk.try_into().expect("exact chunk"))))
        .collect()
}

fn float_chunks<const N: usize, T: Copy>(
    bytes: &[u8],
    decode: fn([u8; N]) -> T,
    into: fn(T) -> f64,
) -> Result<Vec<Value>> {
    bytes
        .chunks_exact(N)
        .map(|chunk| {
            let value = into(decode(chunk.try_into().expect("exact chunk")));
            Number::from_f64(value)
                .map(Value::Number)
                .context("JSLB contains a non-finite float")
        })
        .collect()
}

fn field<'a>(object: &'a Map<String, Value>, name: &str) -> Result<&'a Value> {
    object.get(name).with_context(|| format!("missing {name}"))
}

fn object_map<'a>(object: &'a Map<String, Value>, name: &str) -> Result<&'a Map<String, Value>> {
    field(object, name)?
        .as_object()
        .with_context(|| format!("{name} is not an object"))
}

fn string_array(value: &Value) -> Result<Vec<String>> {
    value
        .as_array()
        .context("stringArray is not an array")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .context("invalid stringArray value")
        })
        .collect()
}

fn integer_array(value: &Value) -> Result<Vec<i64>> {
    value
        .as_array()
        .context("column is not an array")?
        .iter()
        .map(|value| value.as_i64().context("column contains a non-integer"))
        .collect()
}

fn optional_integer_array(value: &Value) -> Result<Vec<Option<i64>>> {
    value
        .as_array()
        .context("column is not an array")?
        .iter()
        .map(|value| {
            if value.is_null() {
                Ok(None)
            } else {
                value
                    .as_i64()
                    .map(Some)
                    .context("column contains a non-integer")
            }
        })
        .collect()
}

fn float_array(value: &Value) -> Result<Vec<f64>> {
    value
        .as_array()
        .context("column is not an array")?
        .iter()
        .map(|value| value.as_f64().context("column contains a non-number"))
        .collect()
}

fn display_id(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(value)) => value.to_string(),
        _ => "?".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, File},
        io::Write,
    };

    use flate2::{Compression, write::GzEncoder};
    use serde_json::json;
    use tempfile::tempdir;

    use super::{Source, decode, entries, find};
    use crate::{
        FirefoxProfileFormat,
        firefox::write_firefox_profile,
        profile::{Frame, Stack, TimedStackSample},
    };

    fn sample(pid: u32, tid: u32) -> TimedStackSample {
        TimedStackSample {
            stack: Stack::from(vec![Frame { address: 0x401000 }]),
            pid,
            tid,
            thread_name: Some("gallery-worker".to_owned()),
            timestamp: 1_000,
            cpu_delta: 10,
        }
    }

    fn write_json_gzip(path: &std::path::Path, value: serde_json::Value) {
        let file = File::create(path).expect("create JSON fixture");
        let mut encoder = GzEncoder::new(file, Compression::fast());
        encoder
            .write_all(&serde_json::to_vec(&value).expect("serialize JSON fixture"))
            .expect("gzip JSON fixture");
        encoder.finish().expect("finish JSON fixture");
    }

    #[test]
    fn directory_entries_decode_json_and_jslb_and_reject_path_ids() {
        let directory = tempdir().expect("gallery directory");
        let json_path = directory.path().join("firefox-gallery-000001-100.json.gz");
        let jslb_path = directory.path().join("firefox-gallery-000002-200.jslb.gz");
        write_firefox_profile(
            &json_path,
            &[sample(41, 42)],
            FirefoxProfileFormat::Json,
            1_000_000_000,
            49,
            "gallery-target",
            None,
        )
        .expect("write JSON Firefox profile");
        write_firefox_profile(
            &jslb_path,
            &[sample(51, 52)],
            FirefoxProfileFormat::Jslb,
            2_000_000_000,
            49,
            "gallery-target",
            None,
        )
        .expect("write JSLB Firefox profile");
        fs::write(
            directory.path().join("diagnostics-gallery-000002-200.json"),
            br#"{"firefox":{"samples":7,"dropped_samples":3}}"#,
        )
        .expect("write diagnostics");
        fs::write(directory.path().join("not-a-profile.json.gz"), b"ignored")
            .expect("write unrelated file");

        let source = Source::Directory(directory.path().to_owned());
        let listed = entries(&source).expect("scan gallery directory");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].filename, "firefox-gallery-000002-200.jslb.gz");
        assert_eq!(listed[0].format, "jslb");
        assert_eq!(listed[0].samples, Some(7));
        assert_eq!(listed[0].dropped_samples, Some(3));
        assert_eq!(listed[1].format, "json");

        for entry in listed.clone() {
            let profile = decode(entry).expect("decode gallery profile");
            assert_eq!(profile.sample_count, 1);
            assert_eq!(profile.threads.len(), 1);
            assert_eq!(profile.threads[0].name, "gallery-worker");
            assert_eq!(profile.threads[0].times_millis.len(), 1);
        }
        assert!(
            find(&source, &listed[0].id)
                .expect("find listed profile")
                .is_some()
        );
        assert!(
            find(&source, "../../etc/passwd")
                .expect("find traversal id")
                .is_none()
        );
    }

    #[test]
    fn directory_scan_ignores_oversized_profiles_but_single_source_rejects_them() {
        let directory = tempdir().expect("gallery directory");
        let path = directory.path().join("firefox-gallery-000001-100.json.gz");
        let file = File::create(&path).expect("create sparse profile");
        file.set_len(super::MAX_PROFILE_BYTES + 1)
            .expect("make sparse profile exceed limit");

        assert!(
            entries(&Source::Directory(directory.path().to_owned()))
                .expect("scan gallery directory")
                .is_empty()
        );
        let error = match entries(&Source::Profile(path)) {
            Ok(_) => panic!("single oversized profile must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("exceeding the"));
    }

    #[test]
    fn oversized_diagnostics_are_ignored_without_blocking_gallery_entries() {
        let directory = tempdir().expect("gallery directory");
        let profile = directory.path().join("firefox-gallery-000001-100.json.gz");
        write_firefox_profile(
            &profile,
            &[sample(41, 42)],
            FirefoxProfileFormat::Json,
            1_000_000_000,
            49,
            "gallery-target",
            None,
        )
        .expect("write Firefox profile");
        let diagnostic = directory.path().join("diagnostics-gallery-000001-100.json");
        let file = File::create(diagnostic).expect("create oversized diagnostics");
        file.set_len(super::MAX_DIAGNOSTIC_BYTES + 1)
            .expect("make diagnostics exceed limit");

        let listed = entries(&Source::Directory(directory.path().to_owned()))
            .expect("scan gallery directory");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].samples, None);
        assert_eq!(listed[0].dropped_samples, None);
    }

    #[test]
    fn negative_sample_delta_is_rejected_before_viewer_output() {
        let directory = tempdir().expect("gallery directory");
        let path = directory.path().join("firefox-gallery-000001-100.json.gz");
        write_json_gzip(
            &path,
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
                        "timeDeltas": [-1.0],
                        "threadCPUDelta": [2.0]
                    }
                }]
            }),
        );
        let entry = entries(&Source::Profile(path))
            .expect("list single profile")
            .into_iter()
            .next()
            .expect("profile entry");
        let error = match decode(entry) {
            Ok(_) => panic!("negative sample delta must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("negative sample time delta"));
    }
}
