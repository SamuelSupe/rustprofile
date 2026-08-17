#![cfg(target_os = "linux")]

//! User-space coverage for the perf.data import boundary.
//!
//! The fixture below is the smallest regular perf.data file that carries one
//! sampled instruction pointer, PID/TID, timestamp, and two-frame callchain.
//! It avoids perf/eBPF privileges while exercising the same parser and output
//! path used for real perf and simpleperf captures.

use std::{fs, io::Read, process::Command};

use flate2::read::GzDecoder;
use serde_json::Value;
use tempfile::TempDir;

fn profiler() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rustprofile"))
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn minimal_perf_data() -> Vec<u8> {
    minimal_perf_data_with_samples(1)
}

fn minimal_perf_data_with_samples(sample_count: usize) -> Vec<u8> {
    const HEADER_SIZE: u64 = 104;
    const ATTR_SIZE: u64 = 128;
    const ATTR_OFFSET: u64 = HEADER_SIZE;
    const DATA_OFFSET: u64 = HEADER_SIZE + ATTR_SIZE;

    // PERF_SAMPLE_IP | PERF_SAMPLE_TID | PERF_SAMPLE_TIME | PERF_SAMPLE_CALLCHAIN.
    const SAMPLE_TYPE: u64 = 1 | 2 | 4 | 1 << 5;
    const PID: u32 = 4242;
    let record_size: u16 = 56;
    let data_size = u64::from(record_size).saturating_mul(sample_count as u64);

    let mut bytes = Vec::with_capacity((DATA_OFFSET + data_size) as usize);
    bytes.extend_from_slice(b"PERFILE2");
    push_u64(&mut bytes, HEADER_SIZE);
    push_u64(&mut bytes, ATTR_SIZE);
    push_u64(&mut bytes, ATTR_OFFSET);
    push_u64(&mut bytes, ATTR_SIZE);
    push_u64(&mut bytes, DATA_OFFSET);
    push_u64(&mut bytes, data_size);
    push_u64(&mut bytes, 0);
    push_u64(&mut bytes, 0);
    for _ in 0..4 {
        push_u64(&mut bytes, 0);
    }
    assert_eq!(bytes.len(), HEADER_SIZE as usize);

    // A hardware CPU-cycles event with the fields consumed by the parser.
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, ATTR_SIZE as u32);
    push_u64(&mut bytes, 0);
    push_u64(&mut bytes, 1);
    push_u64(&mut bytes, SAMPLE_TYPE);
    push_u64(&mut bytes, 0);
    push_u64(&mut bytes, 0);
    push_u32(&mut bytes, 1);
    push_u32(&mut bytes, 0);
    push_u64(&mut bytes, 0);
    push_u64(&mut bytes, 0);
    push_u64(&mut bytes, 0);
    push_u64(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u64(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u64(&mut bytes, 0);
    assert_eq!(bytes.len(), DATA_OFFSET as usize);

    for sample_index in 0..sample_count {
        // PERF_RECORD_SAMPLE: IP, PID/TID, TIME, CALLCHAIN length and two frames.
        push_u32(&mut bytes, 9);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, record_size);
        push_u64(&mut bytes, 0x401000 + (sample_index as u64 * 0x1000));
        push_u32(&mut bytes, PID);
        push_u32(&mut bytes, PID);
        push_u64(
            &mut bytes,
            1_000_000_000 + (sample_index as u64 * 1_000_000),
        );
        push_u64(&mut bytes, 2);
        push_u64(&mut bytes, 0x401000 + (sample_index as u64 * 0x1000));
        push_u64(&mut bytes, 0x402000 + (sample_index as u64 * 0x1000));
    }
    assert_eq!(bytes.len(), (DATA_OFFSET + data_size) as usize);
    bytes
}

fn gunzip(path: &std::path::Path) -> Vec<u8> {
    let compressed = fs::read(path).expect("read gzip output");
    assert_eq!(&compressed[..2], &[0x1f, 0x8b]);
    let mut decoder = GzDecoder::new(compressed.as_slice());
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes).expect("decompress output");
    bytes
}

#[test]
fn import_emits_bounded_pprof_diagnostics_and_firefox_formats() {
    let fixture = TempDir::new().expect("fixture tempdir");
    let input = fixture.path().join("perf.data");
    fs::write(&input, minimal_perf_data()).expect("write perf.data fixture");

    for (format, extension, magic) in [
        ("json", "json.gz", None),
        (
            "jslb",
            "jslb.gz",
            Some([0xdc, 0xdf, b'J', b'S', b'L', b'B', 1, 0]),
        ),
    ] {
        let output = TempDir::new().expect("output tempdir");
        let result = profiler()
            .args([
                "import",
                "--input",
                input.to_str().expect("input path is UTF-8"),
                "--format",
                "perf-data",
                "--window",
                "1s",
                "--firefox-profile",
                format,
                "--output",
                output.path().to_str().expect("output path is UTF-8"),
            ])
            .output()
            .expect("import command should be runnable");
        assert!(
            result.status.success(),
            "import {format} failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );

        let entries = fs::read_dir(output.path())
            .expect("read import output")
            .map(|entry| entry.expect("import output entry").path())
            .collect::<Vec<_>>();
        let cpu = entries
            .iter()
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("cpu-"))
            })
            .expect("import should write a CPU pprof");
        assert_eq!(&gunzip(cpu)[..1], &[0x0a]);
        let firefox = entries
            .iter()
            .find(|path| {
                path.extension().is_some_and(|extension| extension == "gz")
                    && path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().contains("firefox-"))
            })
            .expect("import should write a Firefox profile");
        let firefox_bytes = gunzip(firefox);
        if let Some(expected_magic) = magic {
            assert_eq!(
                firefox_bytes.get(..expected_magic.len()),
                Some(expected_magic.as_slice())
            );
        } else {
            let profile: Value = serde_json::from_slice(&firefox_bytes).expect("Firefox JSON");
            assert_eq!(profile["meta"]["product"], "rustprofile");
        }

        let diagnostics = entries
            .iter()
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .expect("import should write diagnostics");
        let diagnostics: Value =
            serde_json::from_slice(&fs::read(diagnostics).expect("read diagnostics"))
                .expect("diagnostics JSON");
        assert_eq!(diagnostics["schema_version"], 3);
        assert_eq!(diagnostics["samples"], 1);
        assert_eq!(diagnostics["format"], "perf_data");
        assert_eq!(diagnostics["outputs"].as_array().map(Vec::len), Some(3));
    }
}

#[test]
fn import_rejects_an_empty_perf_file_without_publishing_outputs() {
    let input_dir = TempDir::new().expect("input tempdir");
    let output_dir = TempDir::new().expect("output tempdir");
    let input = input_dir.path().join("empty.perf.data");
    fs::write(&input, []).expect("write empty input");

    let result = profiler()
        .args([
            "import",
            "--input",
            input.to_str().expect("input path is UTF-8"),
            "--output",
            output_dir.path().to_str().expect("output path is UTF-8"),
        ])
        .output()
        .expect("import command should be runnable");
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("failed to parse perf.data"));
    assert!(
        fs::read_dir(output_dir.path())
            .expect("read output directory")
            .next()
            .is_none()
    );
}

#[test]
fn import_reports_dropped_firefox_timeline_samples_when_budget_is_one() {
    let fixture = TempDir::new().expect("fixture tempdir");
    let input = fixture.path().join("perf.data");
    fs::write(&input, minimal_perf_data_with_samples(2)).expect("write perf.data fixture");
    let output = TempDir::new().expect("output tempdir");

    let result = profiler()
        .args([
            "import",
            "--input",
            input.to_str().expect("input path is UTF-8"),
            "--format",
            "perf-data",
            "--window",
            "1s",
            "--firefox-profile",
            "json",
            "--max-timeline-samples",
            "1",
            "--output",
            output.path().to_str().expect("output path is UTF-8"),
        ])
        .output()
        .expect("import command should be runnable");
    assert!(
        result.status.success(),
        "import failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    let diagnostics = fs::read_dir(output.path())
        .expect("read import output")
        .map(|entry| entry.expect("import output entry").path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .expect("import should write diagnostics");
    let diagnostics: Value =
        serde_json::from_slice(&fs::read(diagnostics).expect("read diagnostics"))
            .expect("diagnostics JSON");
    assert_eq!(diagnostics["samples"], 2);
    assert_eq!(diagnostics["timeline_dropped_samples"], 1);
}
