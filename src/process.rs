use std::{collections::BTreeSet, fs, os::unix::fs::MetadataExt, path::PathBuf, str::FromStr};

use anyhow::{Context, Result, bail};
use object::{Object, ObjectSection, ObjectSymbol};

use crate::{
    config::{AllocatorChoice, DEFAULT_MAX_THREADS, KernelVersion},
    diagnostics::{AllocatorReport, CheckReport, ModuleReport},
    maps::{MapEntry, mapped_files, read_process_maps},
};

const RUST_ALLOCATOR_SYMBOLS: [&str; 4] = [
    "__rust_alloc",
    "__rust_alloc_zeroed",
    "__rust_realloc",
    "__rust_dealloc",
];

const SYSTEM_ALLOCATOR_SYMBOLS: [&str; 4] = ["malloc", "calloc", "realloc", "free"];

pub fn inspect(
    pid: i32,
    allocator: AllocatorChoice,
    target: crate::TargetMetadata,
) -> Result<CheckReport> {
    let executable_link = PathBuf::from(format!("/proc/{pid}/exe"));
    let executable = fs::read_link(&executable_link)
        .with_context(|| format!("cannot inspect process {pid}; failed to resolve executable"))?;
    let executable_data = fs::read(&executable_link)
        .with_context(|| format!("failed to read executable for process {pid}"))?;
    let executable_object = object::File::parse(executable_data.as_slice())
        .context("target executable is not a supported object file")?;
    let architecture = architecture_name(executable_object.architecture())?;

    let kernel_release = kernel_release()?;
    let kernel_version = KernelVersion::from_str(&kernel_release)?;
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    let running_as_root = unsafe { libc::geteuid() } == 0;
    let thread_count = read_threads(pid)?.len();
    let maps = read_process_maps(pid)?;
    let mut files = mapped_files(&maps);
    if !files.contains(&executable) {
        files.insert(0, executable.clone());
    }

    let mut modules = Vec::new();
    let mut rust_candidate = None;
    let mut system_candidate = None::<(bool, PathBuf)>;
    for path in &files {
        let mapping = maps
            .iter()
            .find(|mapping| mapping.path.as_deref() == Some(path.as_path()));
        let scanned = if path == &executable {
            scan_module(path, &executable_object)
        } else {
            let Some(scanned) = inspect_module(pid, path, mapping)? else {
                continue;
            };
            scanned
        };
        if scanned.has_rust_allocator {
            rust_candidate = Some(path.clone());
        }
        if scanned.has_system_allocator {
            let dedicated_libc = path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.contains("libc") || name.contains("ld-musl")
            });
            if dedicated_libc || path == &executable {
                let replace = system_candidate
                    .as_ref()
                    .is_none_or(|(current_is_libc, _)| dedicated_libc && !current_is_libc);
                if replace {
                    system_candidate = Some((dedicated_libc, path.clone()));
                }
            }
        }
        modules.push(scanned.report);
    }
    let has_unwind_info = has_nonempty_section(&executable_object, ".eh_frame")
        || has_nonempty_section(&executable_object, ".debug_frame");
    let allocator_report = select_allocator(allocator, rust_candidate, system_candidate);

    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    if !kernel_version.is_supported() {
        errors.push(format!(
            "kernel {kernel_release} is below the required Linux 5.8 baseline"
        ));
    }
    if !running_as_root {
        errors.push("rustprofile MVP must run as root".to_owned());
    }
    if architecture != std::env::consts::ARCH {
        errors.push(format!(
            "target architecture {architecture} does not match profiler architecture {}",
            std::env::consts::ARCH
        ));
    }
    if thread_count > DEFAULT_MAX_THREADS {
        errors.push(format!(
            "target has {thread_count} threads, exceeding the default limit of {DEFAULT_MAX_THREADS}"
        ));
    }
    if !has_unwind_info {
        warnings.push(
            "the target executable has no .eh_frame or .debug_frame; automatic fallback may only produce leaf frames"
                .to_owned(),
        );
    }
    if !allocator_report.complete {
        warnings.push(
            allocator_report
                .reason
                .clone()
                .unwrap_or_else(|| "heap allocator probes are unavailable".to_owned()),
        );
    }

    Ok(CheckReport {
        schema_version: 2,
        pid,
        target,
        executable,
        architecture: architecture.to_owned(),
        kernel_release,
        kernel_supported: kernel_version.is_supported(),
        running_as_root,
        thread_count,
        modules,
        has_unwind_info,
        allocator: allocator_report,
        warnings,
        errors,
    })
}

pub fn read_threads(pid: i32) -> Result<BTreeSet<i32>> {
    let path = format!("/proc/{pid}/task");
    let entries = fs::read_dir(&path).with_context(|| format!("failed to read {path}"))?;
    entries
        .map(|entry| {
            let entry = entry?;
            entry
                .file_name()
                .to_string_lossy()
                .parse::<i32>()
                .context("invalid thread id in procfs")
        })
        .collect()
}

struct ScannedModule {
    report: ModuleReport,
    has_rust_allocator: bool,
    has_system_allocator: bool,
}

fn inspect_module(
    pid: i32,
    path: &std::path::Path,
    mapping: Option<&MapEntry>,
) -> Result<Option<ScannedModule>> {
    let process_path = mapped_module_path(pid, path, mapping);
    let Ok(data) = fs::read(&process_path) else {
        return Ok(None);
    };
    let Ok(object) = object::File::parse(data.as_slice()) else {
        return Ok(None);
    };
    Ok(Some(scan_module(path, &object)))
}

fn scan_module(path: &std::path::Path, object: &object::File<'_>) -> ScannedModule {
    let build_id = object.build_id().ok().flatten().map(hex::encode);
    let mut symbol_count = 0;
    let mut rust_symbols = 0_u8;
    let mut system_symbols = 0_u8;
    for symbol in object.symbols().chain(object.dynamic_symbols()) {
        if symbol.address() == 0 {
            continue;
        }
        let Ok(name) = symbol.name() else {
            continue;
        };
        symbol_count += 1;
        if symbol.is_undefined() {
            continue;
        }
        if let Some(index) = RUST_ALLOCATOR_SYMBOLS
            .iter()
            .position(|candidate| *candidate == name)
        {
            rust_symbols |= 1 << index;
        }
        if let Some(index) = SYSTEM_ALLOCATOR_SYMBOLS
            .iter()
            .position(|candidate| *candidate == name)
        {
            system_symbols |= 1 << index;
        }
    }
    ScannedModule {
        report: ModuleReport {
            path: path.to_path_buf(),
            build_id,
            has_eh_frame: has_nonempty_section(&object, ".eh_frame"),
            has_debug_frame: has_nonempty_section(&object, ".debug_frame"),
            symbol_count,
        },
        has_rust_allocator: rust_symbols == (1_u8 << RUST_ALLOCATOR_SYMBOLS.len()) - 1,
        has_system_allocator: system_symbols == (1_u8 << SYSTEM_ALLOCATOR_SYMBOLS.len()) - 1,
    }
}

fn has_nonempty_section<'data>(object: &object::File<'data>, name: &str) -> bool {
    object
        .section_by_name(name)
        .is_some_and(|section| section.size() > 0)
}

fn select_allocator(
    requested: AllocatorChoice,
    rust_candidate: Option<PathBuf>,
    system_candidate: Option<(bool, PathBuf)>,
) -> AllocatorReport {
    let selected = match requested {
        AllocatorChoice::Auto => rust_candidate
            .map(|path| ("rust", path))
            .or_else(|| system_candidate.map(|(_, path)| ("system", path))),
        AllocatorChoice::Rust => rust_candidate.map(|path| ("rust", path)),
        AllocatorChoice::System => system_candidate.map(|(_, path)| ("system", path)),
    };

    match selected {
        Some((family, path)) => AllocatorReport {
            requested,
            detected: Some(family.to_owned()),
            module: Some(path),
            complete: true,
            reason: None,
        },
        None => AllocatorReport {
            requested,
            detected: None,
            module: None,
            complete: false,
            reason: Some(match requested {
                AllocatorChoice::Auto => {
                    "neither a complete Rust allocator shim nor a supported mapped libc was found"
                }
                AllocatorChoice::Rust => "the complete __rust_alloc symbol family was not found",
                AllocatorChoice::System => "a supported mapped libc allocator was not found",
            }
            .to_owned()),
        },
    }
}

pub fn process_root_path(pid: i32, path: &std::path::Path) -> PathBuf {
    if path.is_absolute() {
        PathBuf::from(format!("/proc/{pid}/root")).join(
            path.strip_prefix("/")
                .expect("absolute paths always have a root prefix"),
        )
    } else {
        path.to_owned()
    }
}

pub fn mapped_module_path(pid: i32, path: &std::path::Path, mapping: Option<&MapEntry>) -> PathBuf {
    if let Some(mapping) = mapping {
        let map_file = PathBuf::from(format!(
            "/proc/{pid}/map_files/{:x}-{:x}",
            mapping.start, mapping.end
        ));
        if fs::File::open(&map_file).is_ok() {
            return map_file;
        }
    }
    process_root_path(pid, path)
}

pub fn file_identity(path: &std::path::Path) -> Result<(u64, u64, i64, u64)> {
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to stat module {}", path.display()))?;
    Ok((
        metadata.dev(),
        metadata.ino(),
        metadata.mtime(),
        metadata.size(),
    ))
}

fn architecture_name(architecture: object::Architecture) -> Result<&'static str> {
    match architecture {
        object::Architecture::X86_64 => Ok("x86_64"),
        object::Architecture::Aarch64 => Ok("aarch64"),
        other => bail!("unsupported target architecture {other:?}"),
    }
}

#[cfg(target_os = "linux")]
fn kernel_release() -> Result<String> {
    let mut uts = std::mem::MaybeUninit::<libc::utsname>::uninit();
    // SAFETY: uname initializes the provided utsname on success.
    let result = unsafe { libc::uname(uts.as_mut_ptr()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("uname failed");
    }
    // SAFETY: uname returned success, so every field in uts is initialized.
    let uts = unsafe { uts.assume_init() };
    // SAFETY: uname fields are fixed-size NUL-terminated C strings on success.
    let release = unsafe { std::ffi::CStr::from_ptr(uts.release.as_ptr()) };
    Ok(release.to_string_lossy().into_owned())
}

#[cfg(not(target_os = "linux"))]
fn kernel_release() -> Result<String> {
    bail!("rustprofile only supports Linux")
}
