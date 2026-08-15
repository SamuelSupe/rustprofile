use std::process::Command;

fn profiler() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rustprofile"))
}

fn assert_rejected(arguments: &[&str], expected: &str) {
    let output = profiler()
        .args(arguments)
        .output()
        .expect("rustprofile binary should be runnable");
    assert!(
        !output.status.success(),
        "expected arguments {arguments:?} to be rejected, stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("tracefs is not mounted") {
        eprintln!("skipping OTLP protocol assertion because tracefs is unavailable: {stderr}");
        return;
    }
    assert!(
        stderr.contains(expected),
        "stderr for {arguments:?} did not contain {expected:?}: {stderr}"
    );
}

#[test]
fn check_rejects_non_positive_pid_at_the_cli_boundary() {
    assert_rejected(
        &["check", "--pid", "0"],
        "invalid value '0' for '--pid <PID>'",
    );
}

#[test]
fn record_rejects_invalid_parser_ranges_before_touching_a_process() {
    assert_rejected(
        &["record", "--pid", "1", "--cpu-frequency", "1000"],
        "invalid value '1000' for '--cpu-frequency <CPU_FREQUENCY>'",
    );
    assert_rejected(
        &["record", "--pid", "1", "--keep-windows", "0"],
        "value must be greater than zero",
    );
    assert_rejected(
        &["record", "--pid", "1", "--max-stacks", "0"],
        "value must be greater than zero",
    );
}

#[test]
#[cfg(target_os = "linux")]
fn record_rejects_zero_window_after_duration_parsing() {
    let output = profiler()
        .args(["record", "--pid", "1", "--window", "0"])
        .output()
        .expect("rustprofile binary should be runnable");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("tracefs is not mounted") {
        eprintln!("skipping zero-window assertion because tracefs is unavailable: {stderr}");
        return;
    }
    assert!(
        stderr.contains("--window must be greater than zero"),
        "stderr did not explain zero window: {stderr}"
    );
}

#[test]
fn record_rejects_unknown_profile_kind() {
    assert_rejected(
        &["record", "--pid", "1", "--profiles", "cpu,unknown"],
        "invalid value 'unknown'",
    );
}

#[test]
#[cfg(target_os = "linux")]
fn target_selector_is_required_and_mutually_exclusive() {
    assert_rejected(&["check", "--json"], "required");

    assert_rejected(
        &["check", "--pid", "1", "--docker-container", "container-id"],
        "cannot be used",
    );
    assert_rejected(
        &[
            "record",
            "--docker-container",
            "container-id",
            "--k8s-pod",
            "default/api",
        ],
        "cannot be used",
    );
    assert_rejected(
        &["check", "--pid", "1", "--container", "app"],
        "cannot be used",
    );
}

#[test]
fn otlp_compression_is_a_closed_cli_enum() {
    assert_rejected(
        &["record", "--pid", "1", "--otlp-compression", "br"],
        "invalid value 'br'",
    );
}

#[test]
#[cfg(target_os = "linux")]
fn unsupported_otlp_protocol_is_rejected_before_target_access() {
    let output = profiler()
        .env("OTEL_EXPORTER_OTLP_PROFILES_PROTOCOL", "grpc")
        .args([
            "record",
            "--pid",
            "1",
            "--profiles",
            "cpu",
            "--duration",
            "1ms",
            "--otlp-endpoint",
            "http://127.0.0.1:4318/v1development/profiles",
        ])
        .output()
        .expect("rustprofile binary should be runnable");
    assert!(
        !output.status.success(),
        "unsupported OTLP protocol should fail before process access"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("http/protobuf") || stderr.contains("protocol"),
        "stderr did not explain unsupported OTLP protocol: {stderr}"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn invalid_otlp_timeout_environment_is_rejected_explicitly() {
    let output = profiler()
        .env(
            "OTEL_EXPORTER_OTLP_PROFILES_TIMEOUT",
            "not-a-millisecond-value",
        )
        .env_remove("OTEL_EXPORTER_OTLP_TIMEOUT")
        .args([
            "record",
            "--pid",
            "1",
            "--profiles",
            "cpu",
            "--duration",
            "1ms",
            "--window",
            "1ms",
            "--unwind",
            "fp",
            "--allow-partial",
            "--otlp-endpoint",
            "http://127.0.0.1:4318/v1development/profiles",
        ])
        .output()
        .expect("rustprofile binary should be runnable");
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("tracefs is not mounted") {
        eprintln!("skipping OTLP timeout assertion because tracefs is unavailable: {stderr}");
        return;
    }
    assert!(!output.status.success(), "invalid OTLP timeout must fail");
    assert!(
        stderr.contains("OTLP timeout environment value must be milliseconds"),
        "stderr did not explain invalid OTLP timeout: {stderr}"
    );
}
