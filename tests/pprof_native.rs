#![cfg(target_os = "linux")]

use std::{
    collections::HashMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::Mutex,
    time::Duration,
};

use flate2::read::GzDecoder;
use prost::Message;
use rustprofile::{CheckReport, ProfileKind, WindowDiagnostics};
use tempfile::TempDir;

static NATIVE_TEST_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, PartialEq, Message)]
struct Profile {
    #[prost(message, repeated, tag = "1")]
    sample_type: Vec<ValueType>,
    #[prost(message, repeated, tag = "2")]
    sample: Vec<Sample>,
    #[prost(message, repeated, tag = "5")]
    function: Vec<Function>,
    #[prost(string, repeated, tag = "6")]
    string_table: Vec<String>,
    #[prost(int64, tag = "9")]
    time_nanos: i64,
    #[prost(int64, tag = "10")]
    duration_nanos: i64,
    #[prost(int64, tag = "12")]
    period: i64,
    #[prost(int64, repeated, tag = "13")]
    comment: Vec<i64>,
    #[prost(int64, tag = "14")]
    default_sample_type: i64,
}

#[derive(Clone, PartialEq, Message)]
struct Function {
    #[prost(uint64, tag = "1")]
    id: u64,
    #[prost(int64, tag = "2")]
    name: i64,
    #[prost(int64, tag = "3")]
    system_name: i64,
    #[prost(int64, tag = "4")]
    filename: i64,
    #[prost(int64, tag = "5")]
    start_line: i64,
}

#[derive(Clone, PartialEq, Message)]
struct ValueType {
    #[prost(int64, tag = "1")]
    r#type: i64,
    #[prost(int64, tag = "2")]
    unit: i64,
}

#[derive(Clone, PartialEq, Message)]
struct Sample {
    #[prost(uint64, repeated, packed = "true", tag = "1")]
    location_id: Vec<u64>,
    #[prost(int64, repeated, packed = "true", tag = "2")]
    value: Vec<i64>,
}

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn spawn(path: &Path) -> Self {
        let child = Command::new(path)
            .spawn()
            .unwrap_or_else(|error| panic!("failed to start {}: {error}", path.display()));
        Self { child }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn profiler() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rustprofile"))
}

fn build_cpu_fixture(output_dir: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cpu_target.rs");
    let output = output_dir.join("cpu-target-fp");
    let status = Command::new("rustc")
        .args([
            "--edition=2024",
            source.to_str().expect("fixture path is valid UTF-8"),
            "-C",
            "debuginfo=2",
            "-C",
            "force-frame-pointers=yes",
            "-o",
            output.to_str().expect("output path is valid UTF-8"),
        ])
        .status()
        .expect("rustc should be available in the Linux test image");
    assert!(status.success(), "failed to compile CPU fixture");
    output
}

fn build_system_fixture(output_dir: &Path) -> Option<PathBuf> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/system_target.c");
    let output = output_dir.join("system-target");
    let cc = Command::new("cc")
        .args([
            "-O0",
            "-g",
            "-fno-omit-frame-pointer",
            source.to_str().expect("fixture path is valid UTF-8"),
            "-o",
            output.to_str().expect("output path is valid UTF-8"),
        ])
        .status();
    match cc {
        Ok(status) if status.success() => Some(output),
        Ok(status) => {
            eprintln!("skipping native system allocator test: cc exited with {status}");
            None
        }
        Err(error) => {
            eprintln!("skipping native system allocator test: cc unavailable ({error})");
            None
        }
    }
}

fn build_fixture_variants(output_dir: &Path) {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/build_variants.sh");
    let status = Command::new(&script)
        .arg(output_dir)
        .status()
        .expect("fixture build script should be runnable");
    assert!(status.success(), "fixture build script failed");
}

fn skip_if_preflight_unavailable(output: &std::process::Output) -> Option<CheckReport> {
    let report: CheckReport = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "check --json did not emit a CheckReport (status {:?}): {} / {} ({error})",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    if !report.errors.is_empty() {
        eprintln!(
            "skipping native pprof test because the Linux preflight is unavailable: {}",
            report.errors.join("; ")
        );
        None
    } else {
        Some(report)
    }
}

fn record_preflight_unavailable(output: &std::process::Output) -> bool {
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let unavailable = [
        "failed to determine tracepoint",
        "failed to create tracepoint",
        "failed to attach tracepoint",
    ];
    if unavailable.iter().any(|needle| text.contains(needle)) {
        eprintln!(
            "skipping native pprof test because the Linux tracepoint backend is unavailable: {text}"
        );
        true
    } else {
        false
    }
}

fn decode_profile(path: &Path) -> Profile {
    let compressed =
        fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    assert_eq!(
        compressed.get(0..2),
        Some(&[0x1f, 0x8b][..]),
        "profile must be gzip"
    );
    let mut decoder = GzDecoder::new(compressed.as_slice());
    let mut bytes = Vec::new();
    decoder
        .read_to_end(&mut bytes)
        .unwrap_or_else(|error| panic!("decompress {}: {error}", path.display()));
    Profile::decode(bytes.as_slice())
        .unwrap_or_else(|error| panic!("decode pprof protobuf {}: {error}", path.display()))
}

fn assert_go_pprof_usable(path: &Path) {
    let available = Command::new("go")
        .arg("version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !available {
        eprintln!("go tool pprof unavailable; protobuf decode was still verified");
        return;
    }
    let pprof = Command::new("go")
        .args(["tool", "pprof", "-top", "-nodecount=1"])
        .arg(path)
        .output()
        .expect("go tool pprof should run");
    assert!(
        pprof.status.success(),
        "go tool pprof rejected {}: {}",
        path.display(),
        String::from_utf8_lossy(&pprof.stderr)
    );
    eprintln!("go tool pprof accepted {}", path.display());
}

fn profile_string(profile: &Profile, index: i64) -> &str {
    let index = usize::try_from(index).expect("pprof string index should be non-negative");
    profile
        .string_table
        .get(index)
        .map(String::as_str)
        .unwrap_or_else(|| panic!("pprof string index {index} is out of bounds"))
}

fn artifact_key(path: &Path, prefix: &str, suffix: &str) -> String {
    let name = path
        .file_name()
        .expect("artifact should have a filename")
        .to_string_lossy();
    name.strip_prefix(prefix)
        .and_then(|name| name.strip_suffix(suffix))
        .unwrap_or_else(|| panic!("unexpected artifact name {name:?}"))
        .to_owned()
}

#[test]
fn native_cpu_windows_are_pprof_decodable_and_diagnostics_reconcile() {
    let _lock = NATIVE_TEST_LOCK.lock().expect("native test lock");
    let fixtures = TempDir::new().expect("fixture tempdir");
    let fixture = build_cpu_fixture(fixtures.path());
    let target = ChildGuard::spawn(&fixture);
    std::thread::sleep(Duration::from_millis(100));

    let check = profiler()
        .args(["check", "--pid", &target.pid().to_string(), "--json"])
        .output()
        .expect("check command should be runnable");
    let Some(report) = skip_if_preflight_unavailable(&check) else {
        return;
    };
    assert!(matches!(
        report.target.kind,
        rustprofile::TargetKind::Process
    ));
    assert_eq!(
        report.target.pid,
        i32::try_from(target.pid()).expect("pid fits i32")
    );
    assert_eq!(report.target.container_id, None);

    let output_dir = TempDir::new().expect("profile output tempdir");
    let record = profiler()
        .args([
            "record",
            "--pid",
            &target.pid().to_string(),
            "--profiles",
            "cpu",
            "--duration",
            "900ms",
            "--window",
            "300ms",
            "--unwind",
            "fp",
            "--svg",
            "--output",
            output_dir
                .path()
                .to_str()
                .expect("output path is valid UTF-8"),
        ])
        .output()
        .expect("record command should be runnable");
    if !record.status.success() && record_preflight_unavailable(&record) {
        return;
    }
    assert!(
        record.status.success(),
        "native record failed after a successful preflight: stdout={} stderr={}",
        String::from_utf8_lossy(&record.stdout),
        String::from_utf8_lossy(&record.stderr)
    );

    let mut diagnostics_paths = Vec::new();
    let mut profile_paths = Vec::new();
    let mut svg_paths = Vec::new();
    for entry in fs::read_dir(output_dir.path()).expect("read profile output directory") {
        let path = entry.expect("output directory entry").path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            diagnostics_paths.push(path);
        } else if path.extension().is_some_and(|extension| extension == "gz") {
            profile_paths.push(path);
        } else if path.extension().is_some_and(|extension| extension == "svg") {
            svg_paths.push(path);
        }
    }
    assert!(
        !diagnostics_paths.is_empty(),
        "record should write diagnostics"
    );
    assert!(
        !profile_paths.is_empty(),
        "record should write CPU profiles"
    );
    assert!(!svg_paths.is_empty(), "--svg should write CPU flame graphs");

    let mut diagnostics_by_key = HashMap::new();
    for path in diagnostics_paths {
        let bytes = fs::read(&path).expect("read diagnostics");
        let diagnostics: WindowDiagnostics =
            serde_json::from_slice(&bytes).expect("diagnostics JSON schema");
        assert_eq!(diagnostics.schema_version, 2);
        assert_eq!(
            diagnostics.pid,
            i32::try_from(target.pid()).expect("pid fits i32")
        );
        assert!(matches!(
            diagnostics.target.kind,
            rustprofile::TargetKind::Process
        ));
        assert_eq!(diagnostics.target.pid, diagnostics.pid);
        assert_eq!(diagnostics.target.container_id, None);
        assert_eq!(diagnostics.profiles_requested, vec![ProfileKind::Cpu]);
        assert_eq!(diagnostics.profiles_written, vec![ProfileKind::Cpu]);
        assert!(diagnostics.ended_unix_nanos >= diagnostics.started_unix_nanos);
        assert!(diagnostics.cpu.usable_samples <= diagnostics.cpu.samples);
        assert!(diagnostics.cpu.symbolized_locations <= diagnostics.cpu.total_locations);
        let output_paths = diagnostics
            .outputs
            .iter()
            .map(PathBuf::as_path)
            .collect::<Vec<_>>();
        assert!(
            output_paths.contains(&path.as_path()),
            "diagnostics must list itself"
        );
        for output in output_paths {
            assert!(
                output.exists(),
                "diagnostics listed missing output {}",
                output.display()
            );
        }
        let key = artifact_key(&path, "diagnostics-", ".json");
        assert!(diagnostics_by_key.insert(key, diagnostics).is_none());
    }
    assert!(
        diagnostics_by_key
            .values()
            .any(|window: &WindowDiagnostics| window.cpu.samples > 0),
        "busy target should produce at least one CPU sample"
    );

    let mut profiles_by_key = HashMap::new();
    for path in &profile_paths {
        let profile = decode_profile(path);
        assert_eq!(profile.sample_type.len(), 2);
        let sample_type_names = profile
            .sample_type
            .iter()
            .map(|sample_type| profile_string(&profile, sample_type.r#type))
            .collect::<Vec<_>>();
        assert_eq!(sample_type_names, ["samples", "cpu"]);
        assert!(profile.period > 0);
        assert_eq!(profile_string(&profile, profile.default_sample_type), "cpu");
        for sample in &profile.sample {
            assert_eq!(sample.value.len(), 2);
            assert!(!sample.location_id.is_empty());
        }
        assert!(profile.time_nanos > 0);
        assert!(profile.duration_nanos >= 0);
        let key = artifact_key(path, "cpu-", ".pb.gz");
        assert!(profiles_by_key.insert(key, profile).is_none());
    }
    assert_eq!(diagnostics_by_key.len(), profiles_by_key.len());
    for (key, diagnostics) in &diagnostics_by_key {
        let profile = profiles_by_key
            .get(key)
            .unwrap_or_else(|| panic!("missing CPU profile for diagnostics {key}"));
        let (sample_count, cpu_nanoseconds) = profile.sample.iter().fold(
            (0_i64, 0_i64),
            |(sample_count, cpu_nanoseconds), sample| {
                (
                    sample_count.saturating_add(sample.value[0]),
                    cpu_nanoseconds.saturating_add(sample.value[1]),
                )
            },
        );
        assert_eq!(
            sample_count,
            i64::try_from(diagnostics.cpu.usable_samples).expect("sample count fits i64"),
            "CPU sample count mismatch for {key}"
        );
        assert_eq!(
            cpu_nanoseconds, diagnostics.cpu.cpu_nanoseconds,
            "CPU nanoseconds mismatch for {key}"
        );
    }

    let mut svgs_by_key = HashMap::new();
    for path in &svg_paths {
        let svg = fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read self-contained SVG {}: {error}", path.display()));
        assert!(svg.starts_with("<svg"), "SVG must be a document root");
        assert!(svg.contains("CPU flame graph"));
        assert!(svg.contains("<rect"));
        assert!(!svg.contains("<script"));
        let key = artifact_key(path, "cpu-", ".svg");
        assert!(svgs_by_key.insert(key, path.clone()).is_none());
    }
    assert_eq!(diagnostics_by_key.len(), svgs_by_key.len());
    assert_eq!(profiles_by_key.len(), svgs_by_key.len());
    for (key, diagnostics) in &diagnostics_by_key {
        let svg_path = svgs_by_key
            .get(key)
            .unwrap_or_else(|| panic!("missing CPU SVG for diagnostics {key}"));
        assert!(
            diagnostics.outputs.iter().any(|output| output == svg_path),
            "diagnostics must list CPU SVG for {key}"
        );
    }

    for path in &profile_paths {
        assert_go_pprof_usable(path);
    }
}

#[test]
fn native_system_allocator_windows_report_sampled_heap_semantics() {
    let _lock = NATIVE_TEST_LOCK.lock().expect("native test lock");
    let fixtures = TempDir::new().expect("fixture tempdir");
    let Some(fixture) = build_system_fixture(fixtures.path()) else {
        return;
    };
    let target = ChildGuard::spawn(&fixture);
    std::thread::sleep(Duration::from_millis(100));
    let pid = target.pid().to_string();

    let check = profiler()
        .args(["check", "--pid", &pid, "--json"])
        .output()
        .expect("check command should be runnable");
    let Some(report) = skip_if_preflight_unavailable(&check) else {
        return;
    };
    assert_eq!(
        report.allocator.detected.as_deref(),
        Some("system"),
        "a glibc fixture should select the system allocator"
    );
    assert!(report.allocator.complete);

    let output_dir = TempDir::new().expect("profile output tempdir");
    let record = profiler()
        .args([
            "record",
            "--pid",
            &pid,
            "--profiles",
            "heap",
            "--allocator",
            "system",
            "--alloc-interval",
            "4096",
            "--duration",
            "900ms",
            "--window",
            "300ms",
            "--unwind",
            "fp",
            "--output",
            output_dir
                .path()
                .to_str()
                .expect("output path is valid UTF-8"),
        ])
        .output()
        .expect("record command should be runnable");
    if !record.status.success() && record_preflight_unavailable(&record) {
        return;
    }
    assert!(
        record.status.success(),
        "system heap record failed after a successful preflight: stdout={} stderr={}",
        String::from_utf8_lossy(&record.stdout),
        String::from_utf8_lossy(&record.stderr)
    );

    let mut diagnostics_paths = Vec::new();
    let mut profile_paths = Vec::new();
    for entry in fs::read_dir(output_dir.path()).expect("read profile output directory") {
        let path = entry.expect("output directory entry").path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            diagnostics_paths.push(path);
        } else if path.extension().is_some_and(|extension| extension == "gz") {
            profile_paths.push(path);
        }
    }
    assert!(
        !diagnostics_paths.is_empty(),
        "record should write diagnostics"
    );
    assert!(
        !profile_paths.is_empty(),
        "record should write heap profiles"
    );
    let mut sampled_allocations = 0_u64;
    let mut diagnostics_by_key = HashMap::new();
    for path in diagnostics_paths {
        let diagnostics: WindowDiagnostics =
            serde_json::from_slice(&fs::read(&path).expect("read heap diagnostics"))
                .expect("heap diagnostics JSON schema");
        assert_eq!(diagnostics.schema_version, 2);
        assert!(matches!(
            diagnostics.target.kind,
            rustprofile::TargetKind::Process
        ));
        assert_eq!(diagnostics.target.pid, diagnostics.pid);
        assert_eq!(diagnostics.profiles_requested, vec![ProfileKind::Heap]);
        assert_eq!(diagnostics.profiles_written, vec![ProfileKind::Heap]);
        assert_eq!(diagnostics.heap.allocator.as_deref(), Some("system"));
        assert!(diagnostics.heap.since_attach);
        sampled_allocations =
            sampled_allocations.saturating_add(diagnostics.heap.sampled_allocations);
        assert!(diagnostics.heap.sampled_allocations <= diagnostics.heap.allocation_events);
        assert!(diagnostics.heap.usable_stacks <= diagnostics.heap.stack_samples);
        for output in &diagnostics.outputs {
            assert!(
                output.exists(),
                "diagnostics listed missing output {}",
                output.display()
            );
        }
        let key = artifact_key(&path, "diagnostics-", ".json");
        assert!(diagnostics_by_key.insert(key, diagnostics).is_none());
    }
    assert!(
        sampled_allocations > 0,
        "system allocator fixture should produce sampled allocation events"
    );

    let mut profiles_by_key = HashMap::new();
    for path in &profile_paths {
        let profile = decode_profile(path);
        let names = profile
            .sample_type
            .iter()
            .map(|sample_type| profile_string(&profile, sample_type.r#type))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "alloc_objects",
                "alloc_space",
                "inuse_objects",
                "inuse_space"
            ]
        );
        assert_eq!(
            profile_string(&profile, profile.default_sample_type),
            "inuse_space"
        );
        assert!(
            profile
                .comment
                .iter()
                .map(|comment| profile_string(&profile, *comment))
                .any(|comment| comment.contains("since rustprofile attached")),
            "heap pprof should disclose since-attach inuse semantics"
        );
        for sample in &profile.sample {
            assert_eq!(sample.value.len(), 4);
            assert!(!sample.location_id.is_empty());
        }
        let totals = profile
            .sample
            .iter()
            .fold([0_i64; 4], |mut totals, sample| {
                for (total, value) in totals.iter_mut().zip(&sample.value) {
                    *total = total.saturating_add(*value);
                }
                totals
            });
        let key = artifact_key(path, "heap-", ".pb.gz");
        assert!(profiles_by_key.insert(key, (profile, totals)).is_none());
    }
    assert_eq!(diagnostics_by_key.len(), profiles_by_key.len());
    for (key, diagnostics) in &diagnostics_by_key {
        let (_, totals) = profiles_by_key
            .get(key)
            .unwrap_or_else(|| panic!("missing heap profile for diagnostics {key}"));
        assert_eq!(
            *totals,
            [
                diagnostics.heap.alloc_objects,
                diagnostics.heap.alloc_space,
                diagnostics.heap.inuse_objects,
                diagnostics.heap.inuse_space,
            ],
            "heap sample totals mismatch for {key}"
        );
    }
    for path in &profile_paths {
        assert_go_pprof_usable(path);
    }
}

#[test]
fn native_unwind_acceptance_matrix_covers_fp_dwarf_stripped_and_no_unwind() {
    let _lock = NATIVE_TEST_LOCK.lock().expect("native test lock");
    let fixtures = TempDir::new().expect("fixture tempdir");
    build_fixture_variants(fixtures.path());

    let fp_target = ChildGuard::spawn(&fixtures.path().join("fp-debug"));
    std::thread::sleep(Duration::from_millis(100));
    let fp_pid = fp_target.pid().to_string();
    let fp_check = profiler()
        .args(["check", "--pid", &fp_pid, "--json"])
        .output()
        .expect("check command should be runnable");
    let Some(fp_report) = skip_if_preflight_unavailable(&fp_check) else {
        return;
    };
    assert!(fp_report.has_unwind_info);
    drop(fp_target);

    struct Case {
        label: &'static str,
        file: &'static str,
        unwind_mode: &'static str,
        expected_mode: Option<&'static str>,
        expect_unwind: bool,
        expect_record_success: bool,
        check_functions: bool,
    }
    let cases = [
        Case {
            label: "rust-fp-debug",
            file: "fp-debug",
            unwind_mode: "auto",
            expected_mode: Some("fp"),
            expect_unwind: true,
            expect_record_success: true,
            check_functions: false,
        },
        Case {
            label: if cfg!(target_arch = "aarch64") {
                "rust-default-release-auto-fp-on-aarch64"
            } else {
                "rust-default-release-auto-dwarf"
            },
            file: "default-release",
            unwind_mode: "auto",
            expected_mode: if cfg!(target_arch = "aarch64") {
                Some("fp")
            } else {
                Some("dwarf")
            },
            expect_unwind: true,
            expect_record_success: true,
            check_functions: false,
        },
        Case {
            label: if cfg!(target_arch = "aarch64") {
                "rust-stripped-auto-fp-on-aarch64"
            } else {
                "rust-stripped-auto-dwarf"
            },
            file: "stripped",
            unwind_mode: "auto",
            expected_mode: if cfg!(target_arch = "aarch64") {
                Some("fp")
            } else {
                Some("dwarf")
            },
            expect_unwind: true,
            expect_record_success: true,
            check_functions: true,
        },
        Case {
            label: if cfg!(target_arch = "aarch64") {
                "rust-no-unwind-but-fp-available"
            } else {
                "rust-no-unwind-negative"
            },
            file: "no-unwind",
            unwind_mode: "auto",
            expected_mode: None,
            expect_unwind: false,
            expect_record_success: cfg!(target_arch = "aarch64"),
            check_functions: false,
        },
        Case {
            label: "c-synthetic-cleared-fp-auto-dwarf",
            file: "dwarf-c",
            unwind_mode: "auto",
            expected_mode: Some("dwarf"),
            expect_unwind: true,
            expect_record_success: true,
            check_functions: false,
        },
        Case {
            label: "c-synthetic-cleared-fp-no-unwind-negative",
            file: "no-unwind-c",
            unwind_mode: "auto",
            expected_mode: None,
            expect_unwind: false,
            expect_record_success: false,
            check_functions: false,
        },
    ];

    for case in cases {
        let target = ChildGuard::spawn(&fixtures.path().join(case.file));
        std::thread::sleep(Duration::from_millis(100));
        let pid = target.pid().to_string();
        let check = profiler()
            .args(["check", "--pid", &pid, "--json"])
            .output()
            .expect("check command should be runnable");
        let report: CheckReport = serde_json::from_slice(&check.stdout).unwrap_or_else(|error| {
            panic!(
                "{} check did not emit JSON: {} / {} ({error})",
                case.label,
                String::from_utf8_lossy(&check.stdout),
                String::from_utf8_lossy(&check.stderr)
            )
        });
        assert!(
            report.errors.is_empty(),
            "{} preflight errors: {}",
            case.label,
            report.errors.join("; ")
        );
        assert_eq!(
            report.has_unwind_info, case.expect_unwind,
            "{} unwind report",
            case.label
        );

        let output_dir = TempDir::new().expect("profile output tempdir");
        let record = profiler()
            .args([
                "record",
                "--pid",
                &pid,
                "--profiles",
                "cpu",
                "--duration",
                "3s",
                "--window",
                "700ms",
                "--unwind",
                case.unwind_mode,
                "--output",
                output_dir
                    .path()
                    .to_str()
                    .expect("output path is valid UTF-8"),
            ])
            .output()
            .expect("record command should be runnable");
        if !record.status.success() && record_preflight_unavailable(&record) {
            return;
        }
        assert_eq!(
            record.status.success(),
            case.expect_record_success,
            "{} record status; stdout={} stderr={}",
            case.label,
            String::from_utf8_lossy(&record.stdout),
            String::from_utf8_lossy(&record.stderr)
        );
        if !case.expect_record_success {
            let stderr = String::from_utf8_lossy(&record.stderr);
            assert!(
                stderr.contains("no usable unwind table")
                    || stderr.contains("no .eh_frame or .debug_frame"),
                "{} should explain missing unwind support: {}",
                case.label,
                stderr
            );
            continue;
        }

        let mut diagnostics = Vec::new();
        let mut profiles = Vec::new();
        for entry in fs::read_dir(output_dir.path()).expect("read matrix output directory") {
            let path = entry.expect("matrix output entry").path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                diagnostics.push(
                    serde_json::from_slice::<WindowDiagnostics>(
                        &fs::read(&path).expect("read matrix diagnostics"),
                    )
                    .expect("matrix diagnostics JSON schema"),
                );
            } else if path.extension().is_some_and(|extension| extension == "gz") {
                profiles.push(path);
            }
        }
        assert!(
            !diagnostics.is_empty(),
            "{} should emit diagnostics",
            case.label
        );
        assert!(
            !profiles.is_empty(),
            "{} should emit a CPU profile",
            case.label
        );
        assert!(
            diagnostics.iter().any(|window| window.cpu.samples > 0),
            "{} should collect parsed CPU samples",
            case.label
        );
        assert!(
            diagnostics
                .iter()
                .any(|window| window.cpu.usable_samples > 0),
            "{} should collect usable stacks",
            case.label
        );
        if let Some(expected_mode) = case.expected_mode {
            assert!(
                diagnostics.iter().all(|window| {
                    window.cpu.selected_mode.map(|mode| mode.to_string())
                        == Some(expected_mode.to_owned())
                }),
                "{} should select {}: {:?}",
                case.label,
                expected_mode,
                diagnostics
                    .iter()
                    .map(|window| window.cpu.selected_mode)
                    .collect::<Vec<_>>()
            );
        }
        eprintln!(
            "unwind matrix {}: has_unwind={} selected={:?} samples={:?} usable={:?}",
            case.label,
            report.has_unwind_info,
            diagnostics
                .iter()
                .map(|window| window.cpu.selected_mode)
                .collect::<Vec<_>>(),
            diagnostics
                .iter()
                .map(|window| window.cpu.samples)
                .collect::<Vec<_>>(),
            diagnostics
                .iter()
                .map(|window| window.cpu.usable_samples)
                .collect::<Vec<_>>()
        );
        if case.check_functions {
            for path in profiles {
                let profile = decode_profile(&path);
                assert!(
                    profile.function.iter().all(|function| {
                        let name = profile_string(&profile, function.name);
                        !name.contains("burn") && !name.contains("cpu_target")
                    }),
                    "stripped profile must not invent the fixture's function names"
                );
            }
        }
    }
}
