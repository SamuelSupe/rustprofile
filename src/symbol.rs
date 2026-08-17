use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use addr2line::Loader;
use anyhow::{Context, Result};
use object::{Object, ObjectSection, ObjectSegment};

use crate::{
    maps::{MapEntry, read_process_maps},
    process::mapped_module_path,
};

const RESOLVED_CACHE_CAPACITY: usize = 65_536;
const DEBUGINFOD_BUDGET: Duration = Duration::from_secs(30);
const MAX_DEBUGINFO_BYTES: u64 = 512 * 1024 * 1024;
const JIT_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const MAX_JITDUMP_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PERF_MAP_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MappingInfo {
    pub start: u64,
    pub limit: u64,
    pub offset: u64,
    pub relative_address_at_start: u32,
    pub filename: PathBuf,
    pub build_id: Option<String>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SymbolizedLine {
    pub function: String,
    pub system_name: String,
    pub filename: Option<String>,
    pub line: i64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ResolvedLocation {
    pub address: u64,
    pub mapping: Option<MappingInfo>,
    pub lines: Vec<SymbolizedLine>,
}

impl ResolvedLocation {
    pub fn is_symbolized(&self) -> bool {
        !self.lines.is_empty()
    }
}

struct ModuleSymbols {
    path: PathBuf,
    mappings: Vec<MapEntry>,
    load_bias: u64,
    build_id: Option<String>,
    embedded: Option<Loader>,
    external: Option<Loader>,
}

#[derive(Clone)]
struct JitSymbol {
    start: u64,
    end: u64,
    name: String,
}

#[derive(Clone, Copy)]
struct ModuleRange {
    start: u64,
    end: u64,
    module: usize,
    mapping: usize,
}

pub struct Symbolizer {
    modules: Vec<ModuleSymbols>,
    ranges: Vec<ModuleRange>,
    resolved: HashMap<u64, Arc<ResolvedLocation>>,
    resolved_order: VecDeque<u64>,
    _remote_cache: Option<tempfile::TempDir>,
    perf_map_path: Option<PathBuf>,
    perf_map_modified: Option<std::time::SystemTime>,
    perf_map_checked: Instant,
    jit_symbols: Vec<JitSymbol>,
    jitdump_dir: PathBuf,
    jitdump_pid: i32,
    jitdump_fingerprint: Vec<(PathBuf, u64, std::time::SystemTime)>,
    jitdump_symbols: Vec<JitSymbol>,
}

impl Symbolizer {
    pub fn for_process(
        pid: i32,
        symbol_dirs: &[PathBuf],
        debuginfod: Option<&str>,
    ) -> Result<Self> {
        let maps = read_process_maps(pid)?;
        Self::from_maps(pid, &maps, symbol_dirs, debuginfod)
    }

    pub fn from_maps(
        pid: i32,
        maps: &[MapEntry],
        symbol_dirs: &[PathBuf],
        debuginfod: Option<&str>,
    ) -> Result<Self> {
        let mut grouped = BTreeMap::<PathBuf, Vec<MapEntry>>::new();
        for mapping in maps
            .iter()
            .filter(|mapping| mapping.inode != 0 && mapping.is_executable())
        {
            if let Some(path) = &mapping.path {
                grouped
                    .entry(path.clone())
                    .or_default()
                    .push(mapping.clone());
            }
        }

        let remote_cache = debuginfod.map(|_| tempfile::tempdir()).transpose()?;
        let debuginfod_deadline =
            debuginfod.and_then(|_| Instant::now().checked_add(DEBUGINFOD_BUDGET));
        let debuginfod_agent =
            debuginfod.map(|_| ureq::Agent::config_builder().build().new_agent());
        let mut modules = Vec::new();
        for (path, mappings) in grouped {
            let process_path = mapped_module_path(pid, &path, mappings.first());
            let Ok(data) = fs::read(&process_path) else {
                continue;
            };
            let Ok(object) = object::File::parse(data.as_slice()) else {
                continue;
            };
            let Some(load_bias) = elf_load_bias(&object, &mappings) else {
                continue;
            };
            let build_id = object.build_id().ok().flatten().map(hex::encode);
            let debuglink = gnu_debuglink(&object);
            let external_path = find_external_debug_file(
                &path,
                debuglink.as_deref(),
                build_id.as_deref(),
                symbol_dirs,
            )
            .or_else(|| {
                let (Some(base), Some(build_id), Some(cache), Some(agent)) = (
                    debuginfod,
                    build_id.as_deref(),
                    remote_cache.as_ref(),
                    debuginfod_agent.as_ref(),
                ) else {
                    return None;
                };
                let remaining = debuginfod_deadline?.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return None;
                }
                fetch_debuginfo(agent, base, build_id, cache.path(), remaining).ok()
            });

            let embedded = Loader::new(&process_path).ok();
            let external = external_path
                .as_ref()
                .and_then(|path| Loader::new(path).ok());
            modules.push(ModuleSymbols {
                path,
                mappings,
                load_bias,
                build_id,
                embedded,
                external,
            });
        }

        let mut ranges = modules
            .iter()
            .enumerate()
            .flat_map(|(module, symbols)| {
                symbols
                    .mappings
                    .iter()
                    .enumerate()
                    .map(move |(mapping, range)| ModuleRange {
                        start: range.start,
                        end: range.end,
                        module,
                        mapping,
                    })
            })
            .collect::<Vec<_>>();
        ranges.sort_unstable_by_key(|range| range.start);

        let namespace_pid = namespace_pid(pid);
        let perf_map_path = perf_map_path(pid, namespace_pid);
        let (perf_map_modified, jit_symbols) = load_perf_map(&perf_map_path);
        let jitdump_dir = PathBuf::from(format!("/proc/{pid}/root/tmp"));
        let jitdump_fingerprint = jitdump_fingerprint(&jitdump_dir, namespace_pid);
        let jitdump_symbols = load_jitdump_symbols(&jitdump_fingerprint);
        Ok(Self {
            modules,
            ranges,
            resolved: HashMap::new(),
            resolved_order: VecDeque::new(),
            _remote_cache: remote_cache,
            perf_map_path: Some(perf_map_path),
            perf_map_modified,
            perf_map_checked: Instant::now(),
            jit_symbols,
            jitdump_dir,
            jitdump_pid: namespace_pid,
            jitdump_fingerprint,
            jitdump_symbols,
        })
    }

    pub fn resolve(&mut self, address: u64) -> Arc<ResolvedLocation> {
        if let Some(resolved) = self.resolved.get(&address) {
            return Arc::clone(resolved);
        }
        let resolved = Arc::new(self.resolve_uncached(address));
        if self.resolved.len() >= RESOLVED_CACHE_CAPACITY {
            if let Some(expired) = self.resolved_order.pop_front() {
                self.resolved.remove(&expired);
            }
        }
        self.resolved.insert(address, Arc::clone(&resolved));
        self.resolved_order.push_back(address);
        resolved
    }

    pub fn refresh_dynamic_symbols(&mut self) {
        self.refresh_perf_map();
    }

    pub fn mapping_for_address(&mut self, address: u64) -> Option<MappingInfo> {
        if let Some(symbol) = find_jit_symbol(&self.jitdump_symbols, address) {
            return jit_mapping_info(symbol, "[jit:jitdump]");
        }
        if let Some(symbol) = find_jit_symbol(&self.jit_symbols, address) {
            return jit_mapping_info(symbol, "[jit:perf-map]");
        }
        let index = self
            .ranges
            .partition_point(|mapping| mapping.start <= address);
        let range = index
            .checked_sub(1)
            .and_then(|index| self.ranges.get(index))
            .filter(|mapping| address < mapping.end)?;
        let module = &self.modules[range.module];
        let mapping = &module.mappings[range.mapping];
        Some(MappingInfo {
            start: mapping.start,
            limit: mapping.end,
            offset: mapping.offset,
            relative_address_at_start: u32::try_from(
                mapping.start.saturating_sub(module.load_bias),
            )
            .unwrap_or_default(),
            filename: module.path.clone(),
            build_id: module.build_id.clone(),
        })
    }

    pub fn jit_mapping_count(&self) -> u64 {
        self.jit_symbols
            .len()
            .saturating_add(self.jitdump_symbols.len()) as u64
    }

    fn resolve_uncached(&self, address: u64) -> ResolvedLocation {
        if let Some(symbol) = find_jit_symbol(&self.jitdump_symbols, address) {
            return jit_resolved_location(symbol, address, "[jit:jitdump]");
        }
        let jit_index = self
            .jit_symbols
            .partition_point(|symbol| symbol.start <= address);
        if let Some(symbol) = jit_index
            .checked_sub(1)
            .and_then(|index| self.jit_symbols.get(index))
            .filter(|symbol| address < symbol.end)
        {
            return jit_resolved_location(symbol, address, "[jit:perf-map]");
        }
        let index = self
            .ranges
            .partition_point(|mapping| mapping.start <= address);
        let Some(range) = index
            .checked_sub(1)
            .and_then(|index| self.ranges.get(index))
            .filter(|mapping| address < mapping.end)
        else {
            return ResolvedLocation {
                address,
                mapping: None,
                lines: Vec::new(),
            };
        };
        let module = &self.modules[range.module];
        let mapping = &module.mappings[range.mapping];
        let object_address = address.saturating_sub(module.load_bias);
        let mut lines = module
            .embedded
            .as_ref()
            .map(|loader| lookup(loader, object_address))
            .unwrap_or_default();
        if lines.is_empty() {
            lines = module
                .external
                .as_ref()
                .map(|loader| lookup(loader, object_address))
                .unwrap_or_default();
        }

        ResolvedLocation {
            address,
            mapping: Some(MappingInfo {
                start: mapping.start,
                limit: mapping.end,
                offset: mapping.offset,
                relative_address_at_start: u32::try_from(
                    mapping.start.saturating_sub(module.load_bias),
                )
                .unwrap_or_default(),
                filename: module.path.clone(),
                build_id: module.build_id.clone(),
            }),
            lines,
        }
    }

    fn refresh_perf_map(&mut self) {
        if self.perf_map_checked.elapsed() < JIT_REFRESH_INTERVAL {
            return;
        }
        self.perf_map_checked = Instant::now();
        if let Some(path) = self.perf_map_path.as_deref() {
            let modified = fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok();
            if modified.is_some() && modified != self.perf_map_modified {
                let (modified, symbols) = load_perf_map(path);
                self.perf_map_modified = modified;
                self.jit_symbols = symbols;
                self.resolved.clear();
                self.resolved_order.clear();
            }
        }
        let fingerprint = jitdump_fingerprint(&self.jitdump_dir, self.jitdump_pid);
        if fingerprint != self.jitdump_fingerprint {
            let symbols = load_jitdump_symbols(&fingerprint);
            self.jitdump_fingerprint = fingerprint;
            self.jitdump_symbols = symbols;
            self.resolved.clear();
            self.resolved_order.clear();
        }
    }
}

fn find_jit_symbol(symbols: &[JitSymbol], address: u64) -> Option<&JitSymbol> {
    let index = symbols.partition_point(|symbol| symbol.start <= address);
    index
        .checked_sub(1)
        .and_then(|index| symbols.get(index))
        .filter(|symbol| address < symbol.end)
}

fn jit_resolved_location(symbol: &JitSymbol, address: u64, source: &str) -> ResolvedLocation {
    ResolvedLocation {
        address,
        mapping: Some(MappingInfo {
            start: symbol.start,
            limit: symbol.end,
            offset: 0,
            relative_address_at_start: 0,
            filename: PathBuf::from(source),
            build_id: None,
        }),
        lines: vec![SymbolizedLine {
            function: symbol.name.clone(),
            system_name: symbol.name.clone(),
            filename: None,
            line: 0,
        }],
    }
}

fn jit_mapping_info(symbol: &JitSymbol, source: &str) -> Option<MappingInfo> {
    Some(MappingInfo {
        start: symbol.start,
        limit: symbol.end,
        offset: 0,
        relative_address_at_start: 0,
        filename: PathBuf::from(source),
        build_id: None,
    })
}

fn namespace_pid(pid: i32) -> i32 {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok();
    status
        .as_deref()
        .and_then(|status| status.lines().find(|line| line.starts_with("NSpid:")))
        .and_then(|line| line.split_ascii_whitespace().last())
        .and_then(|pid| pid.parse::<i32>().ok())
        .unwrap_or(pid)
}

fn perf_map_path(pid: i32, namespace_pid: i32) -> PathBuf {
    PathBuf::from(format!("/proc/{pid}/root/tmp/perf-{namespace_pid}.map"))
}

fn load_perf_map(path: &Path) -> (Option<std::time::SystemTime>, Vec<JitSymbol>) {
    let modified = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok();
    let Ok(file) = fs::File::open(path) else {
        return (modified, Vec::new());
    };
    let mut contents = String::new();
    if file
        .take(MAX_PERF_MAP_BYTES.saturating_add(1))
        .read_to_string(&mut contents)
        .is_err()
        || contents.len() as u64 > MAX_PERF_MAP_BYTES
    {
        return (modified, Vec::new());
    }
    let mut symbols = contents
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, ' ');
            let start = u64::from_str_radix(fields.next()?, 16).ok()?;
            let size = u64::from_str_radix(fields.next()?, 16).ok()?;
            let name = fields.next()?.trim();
            (!name.is_empty() && size != 0).then(|| JitSymbol {
                start,
                end: start.saturating_add(size),
                name: name.to_owned(),
            })
        })
        .collect::<Vec<_>>();
    symbols.sort_unstable_by_key(|symbol| symbol.start);
    (modified, symbols)
}

fn jitdump_fingerprint(
    directory: &Path,
    namespace_pid: i32,
) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
    let path = directory.join(format!("jit-{namespace_pid}.dump"));
    let Some((size, modified)) = fs::metadata(&path)
        .ok()
        .and_then(|metadata| Some((metadata.len(), metadata.modified().ok()?)))
    else {
        return Vec::new();
    };
    vec![(path, size, modified)]
}

fn load_jitdump_symbols(files: &[(PathBuf, u64, std::time::SystemTime)]) -> Vec<JitSymbol> {
    use linux_perf_data::jitdump::{JitDumpReader, JitDumpRecord};

    let mut by_index = HashMap::<u64, JitSymbol>::new();
    for (path, size, _) in files {
        if *size > MAX_JITDUMP_BYTES {
            continue;
        }
        let Ok(file) = fs::File::open(path) else {
            continue;
        };
        let Ok(mut reader) = JitDumpReader::new(file) else {
            continue;
        };
        while by_index.len() < 65_536 {
            let Ok(Some(raw)) = reader.next_record() else {
                break;
            };
            match raw.parse() {
                Ok(JitDumpRecord::CodeLoad(record)) => {
                    let name = String::from_utf8_lossy(&record.function_name.as_slice())
                        .trim_end_matches('\0')
                        .to_owned();
                    by_index.insert(
                        record.code_index,
                        JitSymbol {
                            start: record.code_addr,
                            end: record
                                .code_addr
                                .saturating_add(record.code_bytes.len() as u64),
                            name,
                        },
                    );
                }
                Ok(JitDumpRecord::CodeMove(record)) => {
                    if let Some(symbol) = by_index.get_mut(&record.code_index) {
                        symbol.start = record.new_code_addr;
                        symbol.end = record.new_code_addr.saturating_add(record.code_size);
                    }
                }
                _ => {}
            }
        }
    }
    let mut symbols = by_index.into_values().collect::<Vec<_>>();
    symbols.sort_unstable_by_key(|symbol| symbol.start);
    symbols
}

pub fn jit_artifact_counts(pid: i32) -> (u64, u64) {
    let namespace_pid = namespace_pid(pid);
    let perf_maps = perf_map_path(pid, namespace_pid).is_file().into();
    let jitdump_files = PathBuf::from(format!("/proc/{pid}/root/tmp/jit-{namespace_pid}.dump"))
        .is_file()
        .into();
    (perf_maps, jitdump_files)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::jitdump_fingerprint;

    #[test]
    fn jitdump_fingerprint_selects_only_the_namespace_pid_file() {
        let directory = tempdir().expect("jitdump fixture directory");
        for name in [
            "jit-123.dump",
            "jit-124.dump",
            "jit-123.dump.tmp",
            "not-a-jitdump.dump",
        ] {
            fs::write(directory.path().join(name), b"fixture").expect("write jitdump fixture");
        }

        let files = jitdump_fingerprint(directory.path(), 123);
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].0.file_name().and_then(|name| name.to_str()),
            Some("jit-123.dump")
        );
    }
}

fn lookup(loader: &Loader, address: u64) -> Vec<SymbolizedLine> {
    let mut lines = Vec::new();
    if let Ok(mut frames) = loader.find_frames(address) {
        while let Ok(Some(frame)) = frames.next() {
            let raw_name = frame
                .function
                .as_ref()
                .and_then(|function| function.raw_name().ok())
                .map(|name| name.into_owned());
            let name = frame
                .function
                .as_ref()
                .and_then(|function| function.demangle().ok())
                .map(|name| name.into_owned())
                .or_else(|| raw_name.as_deref().map(demangle));
            if let Some(function) = name {
                lines.push(SymbolizedLine {
                    function,
                    system_name: raw_name.unwrap_or_default(),
                    filename: frame
                        .location
                        .as_ref()
                        .and_then(|location| location.file)
                        .map(str::to_owned),
                    line: frame
                        .location
                        .as_ref()
                        .and_then(|location| location.line)
                        .map(i64::from)
                        .unwrap_or_default(),
                });
            }
        }
    }
    if lines.is_empty()
        && let Some(raw_name) = loader.find_symbol(address)
    {
        lines.push(SymbolizedLine {
            function: demangle(raw_name),
            system_name: raw_name.to_owned(),
            filename: None,
            line: 0,
        });
    }
    lines
}

fn demangle(name: &str) -> String {
    rustc_demangle::try_demangle(name)
        .map(|name| format!("{name:#}"))
        .unwrap_or_else(|_| name.to_owned())
}

fn elf_load_bias(object: &object::File<'_>, mappings: &[MapEntry]) -> Option<u64> {
    const PAGE_MASK: u64 = !0xfff;
    for mapping in mappings {
        for segment in object.segments() {
            let (file_offset, _) = segment.file_range();
            if (file_offset & PAGE_MASK) == (mapping.offset & PAGE_MASK) {
                return mapping.start.checked_sub(segment.address() & PAGE_MASK);
            }
        }
    }
    None
}

fn gnu_debuglink(object: &object::File<'_>) -> Option<String> {
    let data = object.section_by_name(".gnu_debuglink")?.data().ok()?;
    let end = data.iter().position(|byte| *byte == 0)?;
    std::str::from_utf8(&data[..end]).ok().map(str::to_owned)
}

fn find_external_debug_file(
    module: &Path,
    debuglink: Option<&str>,
    build_id: Option<&str>,
    symbol_dirs: &[PathBuf],
) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(debuglink) = debuglink {
        if let Some(parent) = module.parent() {
            candidates.push(parent.join(debuglink));
            candidates.push(parent.join(".debug").join(debuglink));
        }
        if module.is_absolute() {
            let relative_parent = module
                .parent()
                .and_then(|parent| parent.strip_prefix("/").ok())
                .unwrap_or_else(|| Path::new(""));
            candidates.push(
                Path::new("/usr/lib/debug")
                    .join(relative_parent)
                    .join(debuglink),
            );
        }
    }
    if module.is_absolute()
        && let Ok(relative) = module.strip_prefix("/")
    {
        candidates.push(PathBuf::from(format!(
            "/usr/lib/debug/{}.debug",
            relative.display()
        )));
    }
    if let Some(build_id) = build_id.filter(|id| id.len() > 2) {
        let (prefix, suffix) = build_id.split_at(2);
        candidates.push(
            Path::new("/usr/lib/debug/.build-id")
                .join(prefix)
                .join(format!("{suffix}.debug")),
        );
    }
    for directory in symbol_dirs {
        if let Some(debuglink) = debuglink {
            candidates.push(directory.join(debuglink));
        }
        if let Some(build_id) = build_id.filter(|id| id.len() > 2) {
            let (prefix, suffix) = build_id.split_at(2);
            candidates.push(
                directory
                    .join(".build-id")
                    .join(prefix)
                    .join(format!("{suffix}.debug")),
            );
        }
        if let Some(filename) = module.file_name() {
            candidates.push(directory.join(filename));
        }
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

fn fetch_debuginfo(
    agent: &ureq::Agent,
    base: &str,
    build_id: &str,
    cache: &Path,
    timeout: Duration,
) -> Result<PathBuf> {
    let url = format!(
        "{}/buildid/{build_id}/debuginfo",
        base.trim_end_matches('/')
    );
    let response = agent
        .get(&url)
        .config()
        .timeout_global(Some(timeout))
        .build()
        .call()
        .with_context(|| format!("debuginfod request failed for build ID {build_id}"))?;
    let path = cache.join(format!("{build_id}.debug"));
    let mut temporary = tempfile::NamedTempFile::new_in(cache)?;
    let mut body = response
        .into_body()
        .into_reader()
        .take(MAX_DEBUGINFO_BYTES + 1);
    let copied = io::copy(&mut body, temporary.as_file_mut())
        .context("failed to read debuginfod response")?;
    if copied > MAX_DEBUGINFO_BYTES {
        anyhow::bail!("debuginfod response exceeded {MAX_DEBUGINFO_BYTES} bytes");
    }
    temporary.as_file().sync_all()?;
    temporary
        .persist(&path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to cache debuginfo at {}", path.display()))?;
    Ok(path)
}
