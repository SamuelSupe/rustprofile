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

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MappingInfo {
    pub start: u64,
    pub limit: u64,
    pub offset: u64,
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
}

impl Symbolizer {
    pub fn for_process(
        pid: i32,
        symbol_dirs: &[PathBuf],
        debuginfod: Option<&str>,
    ) -> Result<Self> {
        let maps = read_process_maps(pid)?;
        let mut grouped = BTreeMap::<PathBuf, Vec<MapEntry>>::new();
        for mapping in maps
            .into_iter()
            .filter(|mapping| mapping.inode != 0 && mapping.is_executable())
        {
            if let Some(path) = &mapping.path {
                grouped.entry(path.clone()).or_default().push(mapping);
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

        Ok(Self {
            modules,
            ranges,
            resolved: HashMap::new(),
            resolved_order: VecDeque::new(),
            _remote_cache: remote_cache,
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

    fn resolve_uncached(&self, address: u64) -> ResolvedLocation {
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
                filename: module.path.clone(),
                build_id: module.build_id.clone(),
            }),
            lines,
        }
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
