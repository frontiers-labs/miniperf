#![deny(missing_docs)]
//! Shared native symbolization for miniperf.
//!
//! Resolution is deliberately offline by default. Set `MINIPERF_DEBUGINFOD=1`
//! and `DEBUGINFOD_URLS` to permit use of an installed `debuginfod-find` client.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use addr2line::Loader;
use object::{Object, ObjectSegment};

/// A mapped object in one sampled process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessMap {
    /// Process containing the mapping.
    pub pid: u32,
    /// Object path as reported by the operating system.
    pub path: PathBuf,
    /// Inclusive mapping start address.
    pub start: u64,
    /// Exclusive mapping end address.
    pub end: u64,
    /// File offset corresponding to `start`.
    pub offset: u64,
}

/// One logical source frame. Multiple frames may correspond to one machine IP
/// when functions were inlined.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    /// Demangled function or JIT symbol name.
    pub function: String,
    /// Source file, when debug information provides one.
    pub file: Option<String>,
    /// One-based source line, when available.
    pub line: Option<u32>,
    /// Mapped object or perf-map path that supplied the frame.
    pub module: Option<PathBuf>,
}

/// Errors from explicitly mutating the build-id cache.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// Reading or writing a cache file failed.
    #[error("build-id cache I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// The supplied file is not a supported object file.
    #[error("failed to parse object file: {0}")]
    Object(#[from] object::Error),
    /// The supplied object has no build ID.
    #[error("object has no build ID")]
    MissingBuildId,
}

/// Build-id debug-file cache rooted beneath `~/.cache/miniperf` by default.
#[derive(Clone, Debug)]
pub struct BuildIdCache {
    root: PathBuf,
}

impl Default for BuildIdCache {
    fn default() -> Self {
        Self::new(default_cache_root())
    }
}

impl BuildIdCache {
    /// Creates a cache at an explicit root, useful for hermetic tools and tests.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns the cache root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Copies `debug_file` into the cache entry belonging to `object_file`.
    pub fn index_debug_file(
        &self,
        object_file: impl AsRef<Path>,
        debug_file: impl AsRef<Path>,
    ) -> Result<PathBuf, CacheError> {
        let bytes = fs::read(object_file)?;
        let object = object::File::parse(bytes.as_slice())?;
        let build_id = object.build_id()?.ok_or(CacheError::MissingBuildId)?;
        self.store(build_id, debug_file.as_ref())
    }

    fn store(&self, build_id: &[u8], source: &Path) -> Result<PathBuf, CacheError> {
        let target = self.path_for(build_id);
        if !target.is_file() {
            let parent = target.parent().expect("cache entry always has a parent");
            fs::create_dir_all(parent)?;
            let temporary = parent.join("debuginfo.tmp");
            fs::copy(source, &temporary)?;
            fs::rename(temporary, &target)?;
        }
        Ok(target)
    }

    fn path_for(&self, build_id: &[u8]) -> PathBuf {
        self.root
            .join("buildid")
            .join(hex(build_id))
            .join("debuginfo")
    }
}

struct Module {
    map: ProcessMap,
    loader: Option<usize>,
    svma_start: Option<u64>,
}

/// Process-aware symbol resolver backed by native objects and perf JIT maps.
pub struct Resolver {
    modules: HashMap<u32, Vec<Module>>,
    loaders: Vec<Loader>,
    perf_maps: HashMap<u32, PerfMap>,
}

impl Resolver {
    /// Loads mapped objects, separate debug files, and `/tmp/perf-<pid>.map`
    /// files for the supplied process maps.
    pub fn new(maps: impl IntoIterator<Item = ProcessMap>) -> Self {
        Self::with_cache(maps, BuildIdCache::default())
    }

    /// Creates a resolver from the executable mappings of the current process.
    #[cfg(target_os = "linux")]
    pub fn for_current_process() -> Result<Self, std::io::Error> {
        Ok(Self::new(current_process_maps()?))
    }

    /// Creates an empty current-process resolver on hosts without procfs maps.
    #[cfg(not(target_os = "linux"))]
    pub fn for_current_process() -> Result<Self, std::io::Error> {
        Ok(Self::new(Vec::new()))
    }

    /// Creates a resolver using an explicit build-id cache.
    pub fn with_cache(maps: impl IntoIterator<Item = ProcessMap>, cache: BuildIdCache) -> Self {
        let maps = maps.into_iter().collect::<Vec<_>>();
        let mut loaders = Vec::new();
        let mut loader_by_path = HashMap::<PathBuf, Option<usize>>::new();
        let mut modules = HashMap::<u32, Vec<Module>>::new();
        let mut pids = HashSet::new();

        for map in maps {
            pids.insert(map.pid);
            let loader = *loader_by_path.entry(map.path.clone()).or_insert_with(|| {
                let debug_path = find_debug_file(&map.path, &cache);
                Loader::new(debug_path).ok().map(|loader| {
                    let index = loaders.len();
                    loaders.push(loader);
                    index
                })
            });
            let svma_start = mapping_svma_start(&map.path, map.offset);
            modules.entry(map.pid).or_default().push(Module {
                map,
                loader,
                svma_start,
            });
        }
        for process_modules in modules.values_mut() {
            process_modules.sort_unstable_by_key(|module| module.map.start);
        }

        let perf_maps = pids
            .into_iter()
            .filter_map(|pid| {
                let path = PathBuf::from(format!("/tmp/perf-{pid}.map"));
                PerfMap::load(&path).ok().map(|map| (pid, map))
            })
            .collect();

        Self {
            modules,
            loaders,
            perf_maps,
        }
    }

    /// Returns whether any native or JIT mapping is known for `pid`.
    pub fn has_process(&self, pid: u32) -> bool {
        self.modules.contains_key(&pid) || self.perf_maps.contains_key(&pid)
    }

    /// Resolves one runtime instruction pointer to logical inline frames,
    /// ordered from innermost callee to outermost caller.
    pub fn resolve(&self, pid: u32, ip: u64) -> Vec<Frame> {
        if let Some(symbol) = self
            .perf_maps
            .get(&pid)
            .and_then(|perf_map| perf_map.resolve(ip))
        {
            return vec![Frame {
                function: symbol.to_owned(),
                file: None,
                line: None,
                module: Some(PathBuf::from(format!("/tmp/perf-{pid}.map"))),
            }];
        }

        let Some(module) = self.modules.get(&pid).and_then(|modules| {
            modules
                .iter()
                .find(|module| ip >= module.map.start && ip < module.map.end)
        }) else {
            return Vec::new();
        };
        let Some(loader) = module.loader.and_then(|index| self.loaders.get(index)) else {
            return Vec::new();
        };
        let relative = ip
            .saturating_sub(module.map.start)
            .saturating_add(module.svma_start.unwrap_or(module.map.offset));
        for address in [relative, ip] {
            let frames = resolve_loader(loader, address, &module.map.path);
            if !frames.is_empty() {
                return frames;
            }
        }
        Vec::new()
    }

    /// Returns the mapped module containing `ip`.
    pub fn module_path(&self, pid: u32, ip: u64) -> Option<&Path> {
        self.modules.get(&pid)?.iter().find_map(|module| {
            (ip >= module.map.start && ip < module.map.end).then_some(module.map.path.as_path())
        })
    }
}

/// Reads executable mappings for the current Linux process.
#[cfg(target_os = "linux")]
pub fn current_process_maps() -> Result<Vec<ProcessMap>, std::io::Error> {
    let pid = std::process::id();
    let maps = fs::read_to_string("/proc/self/maps")?;
    Ok(maps
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(6, char::is_whitespace);
            let range = fields.next()?;
            let permissions = fields.next()?;
            let offset = fields.next()?;
            let _device = fields.next()?;
            let _inode = fields.next()?;
            let path = fields.next()?.trim();
            if !permissions.contains('x') || path.is_empty() || path.starts_with('[') {
                return None;
            }
            let (start, end) = range.split_once('-')?;
            Some(ProcessMap {
                pid,
                path: PathBuf::from(path),
                start: u64::from_str_radix(start, 16).ok()?,
                end: u64::from_str_radix(end, 16).ok()?,
                offset: u64::from_str_radix(offset, 16).ok()?,
            })
        })
        .collect())
}

/// Returns no mappings on hosts without Linux procfs.
#[cfg(not(target_os = "linux"))]
pub fn current_process_maps() -> Result<Vec<ProcessMap>, std::io::Error> {
    Ok(Vec::new())
}

fn mapping_svma_start(path: &Path, mapping_offset: u64) -> Option<u64> {
    let bytes = fs::read(path).ok()?;
    let object = object::File::parse(bytes.as_slice()).ok()?;
    let segment = object
        .segments()
        .filter(|segment| {
            let (file_offset, file_size) = segment.file_range();
            mapping_offset >= (file_offset & !0xfff)
                && mapping_offset < file_offset.saturating_add(file_size)
        })
        .min_by_key(|segment| segment.file_range().0.abs_diff(mapping_offset))?;
    let (file_offset, _) = segment.file_range();
    // Linux maps PT_LOAD segments from a page-aligned file offset, which may
    // precede p_offset. Preserve the segment's SVMA/file delta.
    Some(
        segment
            .address()
            .saturating_add(mapping_offset)
            .saturating_sub(file_offset),
    )
}

fn resolve_loader(loader: &Loader, address: u64, module: &Path) -> Vec<Frame> {
    let mut resolved = Vec::new();
    if let Ok(mut frames) = loader.find_frames(address) {
        while let Ok(Some(frame)) = frames.next() {
            let function = frame
                .function
                .and_then(|function| function.demangle().ok().map(Cow::into_owned));
            let file = frame
                .location
                .as_ref()
                .and_then(|location| location.file.map(str::to_owned));
            let line = frame.location.and_then(|location| location.line);
            if function.is_some() || file.is_some() || line.is_some() {
                resolved.push(Frame {
                    function: function.unwrap_or_else(|| "[unknown]".to_owned()),
                    file,
                    line,
                    module: Some(module.to_path_buf()),
                });
            }
        }
    }
    if resolved.is_empty() {
        if let Some(symbol) = loader.find_symbol(address) {
            resolved.push(Frame {
                function: addr2line::demangle_auto(Cow::Borrowed(symbol), None).into_owned(),
                file: None,
                line: None,
                module: Some(module.to_path_buf()),
            });
        }
    }
    resolved
}

fn find_debug_file(object_path: &Path, cache: &BuildIdCache) -> PathBuf {
    let Ok(bytes) = fs::read(object_path) else {
        return object_path.to_path_buf();
    };
    let Ok(object) = object::File::parse(bytes.as_slice()) else {
        return object_path.to_path_buf();
    };

    if let Ok(Some((name, expected_crc))) = object.gnu_debuglink() {
        let name = OsStr::new(std::str::from_utf8(name).unwrap_or_default());
        let parent = object_path.parent().unwrap_or_else(|| Path::new("."));
        let absolute_debug = Path::new("/usr/lib/debug")
            .join(object_path.strip_prefix("/").unwrap_or(object_path))
            .with_file_name(name);
        for candidate in [
            parent.join(name),
            parent.join(".debug").join(name),
            absolute_debug,
        ] {
            if debuglink_matches(&candidate, expected_crc) {
                return candidate;
            }
        }
    }

    if let Ok(Some(build_id)) = object.build_id() {
        let cached = cache.path_for(build_id);
        if cached.is_file() {
            return cached;
        }
        let id = hex(build_id);
        if id.len() > 2 {
            let system = Path::new("/usr/lib/debug/.build-id")
                .join(&id[..2])
                .join(format!("{}.debug", &id[2..]));
            if system.is_file() {
                return system;
            }
        }
        if debuginfod_enabled() {
            if let Some(downloaded) = debuginfod_find(&id) {
                if let Ok(cached) = cache.store(build_id, &downloaded) {
                    return cached;
                }
                return downloaded;
            }
        }
    }
    object_path.to_path_buf()
}

fn debuglink_matches(path: &Path, expected_crc: u32) -> bool {
    fs::read(path)
        .map(|bytes| crc32fast::hash(&bytes) == expected_crc)
        .unwrap_or(false)
}

fn debuginfod_enabled() -> bool {
    std::env::var_os("DEBUGINFOD_URLS").is_some()
        && std::env::var_os("MINIPERF_DEBUGINFOD").as_deref() == Some(OsStr::new("1"))
}

fn debuginfod_find(build_id: &str) -> Option<PathBuf> {
    let output = Command::new("debuginfod-find")
        .args(["debuginfo", build_id])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(std::str::from_utf8(&output.stdout).ok()?.trim());
    path.is_file().then_some(path)
}

fn default_cache_root() -> PathBuf {
    if let Some(root) = std::env::var_os("MINIPERF_CACHE_DIR") {
        return PathBuf::from(root);
    }
    if let Some(root) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(root).join("miniperf");
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".cache/miniperf")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Default)]
struct PerfMap {
    entries: Vec<PerfMapEntry>,
}

struct PerfMapEntry {
    start: u64,
    end: u64,
    symbol: String,
}

impl PerfMap {
    fn load(path: &Path) -> Result<Self, std::io::Error> {
        let text = fs::read_to_string(path)?;
        Ok(Self::parse(&text))
    }

    fn parse(text: &str) -> Self {
        let mut entries = text
            .lines()
            .filter_map(|line| {
                let mut fields = line.splitn(3, char::is_whitespace);
                let start = u64::from_str_radix(fields.next()?, 16).ok()?;
                let size = u64::from_str_radix(fields.next()?, 16).ok()?;
                let symbol = fields.next()?.trim();
                (!symbol.is_empty() && size != 0).then(|| PerfMapEntry {
                    start,
                    end: start.saturating_add(size),
                    symbol: symbol.to_owned(),
                })
            })
            .collect::<Vec<_>>();
        entries.sort_unstable_by_key(|entry| entry.start);
        Self { entries }
    }

    fn resolve(&self, ip: u64) -> Option<&str> {
        let index = self.entries.partition_point(|entry| entry.start <= ip);
        let entry = self.entries.get(index.checked_sub(1)?)?;
        (ip < entry.end).then_some(entry.symbol.as_str())
    }
}

/// Best-effort symbol lookup in the current Unix process.
#[cfg(unix)]
pub fn current_process_symbol(ip: u64) -> Option<String> {
    let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
    let found = unsafe { libc::dladdr(ip as *const libc::c_void, info.as_mut_ptr()) };
    if found == 0 {
        return None;
    }
    let info = unsafe { info.assume_init() };
    (!info.dli_sname.is_null()).then(|| {
        unsafe { std::ffi::CStr::from_ptr(info.dli_sname) }
            .to_string_lossy()
            .into_owned()
    })
}

/// Current-process lookup is unavailable on non-Unix hosts.
#[cfg(not(unix))]
pub fn current_process_symbol(_ip: u64) -> Option<String> {
    None
}
