//! PDB and PE symbol lookup for recorded Windows module mappings.
//!
//! DbgHelp treats its process handle as a symbol-session key. We give each
//! sampled PID a distinct, owned key so overlapping virtual addresses in
//! different recorded processes cannot collide. DbgHelp is single threaded;
//! every call made here is serialized with one global lock.

use std::{
    collections::HashMap,
    ffi::{c_void, OsStr},
    mem::size_of,
    os::windows::ffi::{OsStrExt, OsStringExt},
    ptr,
    sync::Mutex,
};

use windows_sys::Win32::System::Diagnostics::Debug::{
    SymCleanup, SymFromAddrW, SymGetLineFromAddrW64, SymGetOptions, SymInitializeW,
    SymLoadModuleExW, SymSetOptions, IMAGEHLP_LINEW64, SYMBOL_INFOW, SYMOPT_FAIL_CRITICAL_ERRORS,
    SYMOPT_LOAD_LINES, SYMOPT_NO_PROMPTS, SYMOPT_UNDNAME,
};
use windows_sys::Win32::System::{
    ProcessStatus::{EnumProcessModules, GetModuleFileNameExW, GetModuleInformation, MODULEINFO},
    Threading::GetCurrentProcess,
};

use super::{Frame, Module, ProcessMap};

static DBGHELP_LOCK: Mutex<()> = Mutex::new(());

pub(super) struct NativeResolver {
    processes: HashMap<u32, SymbolSession>,
}

struct SymbolSession {
    // The allocation's address is a stable, unique DbgHelp session key.
    key: Box<u8>,
    initialized: bool,
}

impl SymbolSession {
    fn handle(&self) -> *mut c_void {
        (&*self.key as *const u8).cast_mut().cast()
    }
}

impl Drop for SymbolSession {
    fn drop(&mut self) {
        if !self.initialized {
            return;
        }
        let _guard = DBGHELP_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        // SAFETY: this handle was initialized successfully in `new` and is
        // kept alive by `key` until after cleanup.
        unsafe { SymCleanup(self.handle()) };
    }
}

impl NativeResolver {
    pub(super) fn new(modules: &HashMap<u32, Vec<Module>>) -> Self {
        let mut processes = HashMap::new();
        let _guard = DBGHELP_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        for (&pid, maps) in modules {
            let mut session = SymbolSession {
                key: Box::new(0),
                initialized: false,
            };
            let handle = session.handle();
            // Preserve options installed by other DbgHelp users. Source lines
            // and undecorated names are needed for PDB-backed frames.
            // SAFETY: all pointers passed to DbgHelp below remain valid for
            // the duration of each call. Calls are serialized by the lock.
            unsafe {
                SymSetOptions(
                    SymGetOptions()
                        | SYMOPT_LOAD_LINES
                        | SYMOPT_UNDNAME
                        | SYMOPT_FAIL_CRITICAL_ERRORS
                        | SYMOPT_NO_PROMPTS,
                );
                if SymInitializeW(handle, ptr::null(), 0) == 0 {
                    continue;
                }
            }
            session.initialized = true;
            for module in maps {
                let map = &module.map;
                let Some(size) = map
                    .end
                    .checked_sub(map.start)
                    .and_then(|n| u32::try_from(n).ok())
                else {
                    continue;
                };
                if size == 0 || map.start == 0 {
                    continue;
                }
                let image = wide_null(map.path.as_os_str());
                // A zero return leaves the object/exports fallback available.
                // The module path lets DbgHelp locate its matching local PDB.
                unsafe {
                    SymLoadModuleExW(
                        handle,
                        ptr::null_mut(),
                        image.as_ptr(),
                        ptr::null(),
                        map.start,
                        size,
                        ptr::null(),
                        0,
                    );
                }
            }
            processes.insert(pid, session);
        }
        Self { processes }
    }

    pub(super) fn resolve(&self, pid: u32, ip: u64, map: &ProcessMap) -> Option<Frame> {
        let session = self.processes.get(&pid)?;
        let _guard = DBGHELP_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        // SYMBOL_INFOW ends with a one-element flexible array. The following
        // storage gives DbgHelp room for a long undecorated function name.
        #[repr(C)]
        struct SymbolBuffer {
            info: SYMBOL_INFOW,
            extra_name: [u16; 1024],
        }
        let mut symbol: SymbolBuffer = unsafe { std::mem::zeroed() };
        symbol.info.SizeOfStruct = size_of::<SYMBOL_INFOW>() as u32;
        symbol.info.MaxNameLen = 1025;
        let mut displacement = 0;
        // SAFETY: `symbol` is correctly aligned and has capacity for
        // MaxNameLen UTF-16 units, including SYMBOL_INFOW::Name[0].
        let found_symbol =
            unsafe { SymFromAddrW(session.handle(), ip, &mut displacement, &mut symbol.info) != 0 };
        let function = if found_symbol && symbol.info.ModBase == map.start {
            let len = (symbol.info.NameLen as usize).min(1025);
            let name = unsafe { std::slice::from_raw_parts(symbol.info.Name.as_ptr(), len) };
            (!name.is_empty()).then(|| String::from_utf16_lossy(name))
        } else {
            None
        };

        let mut line = IMAGEHLP_LINEW64 {
            SizeOfStruct: size_of::<IMAGEHLP_LINEW64>() as u32,
            ..Default::default()
        };
        let mut line_displacement = 0;
        let found_line = unsafe {
            SymGetLineFromAddrW64(session.handle(), ip, &mut line_displacement, &mut line) != 0
        };
        let found_line = found_line && line.Address >= map.start && line.Address < map.end;
        let file = if found_line && !line.FileName.is_null() {
            // DbgHelp owns the NUL-terminated string until the next call.
            // Convert it while the global lock is held.
            let len = (0..32768).find(|&i| unsafe { *line.FileName.add(i) == 0 });
            len.map(|len| {
                String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(line.FileName, len) })
            })
        } else {
            None
        };
        if function.is_none() && file.is_none() {
            return None;
        }
        Some(Frame {
            function: function.unwrap_or_else(|| "[unknown]".to_owned()),
            file,
            line: (found_line && line.LineNumber != 0).then_some(line.LineNumber),
            module: Some(map.path.clone()),
        })
    }
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

pub(super) fn current_process_maps() -> Result<Vec<ProcessMap>, std::io::Error> {
    let process = unsafe { GetCurrentProcess() };
    let mut modules = vec![ptr::null_mut(); 128];
    loop {
        let mut bytes_needed = 0;
        let bytes_available = (modules.len() * size_of::<*mut c_void>()) as u32;
        let ok = unsafe {
            EnumProcessModules(
                process,
                modules.as_mut_ptr(),
                bytes_available,
                &mut bytes_needed,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if bytes_needed > bytes_available {
            modules.resize(
                bytes_needed as usize / size_of::<*mut c_void>() + 16,
                ptr::null_mut(),
            );
            continue;
        }
        modules.truncate(bytes_needed as usize / size_of::<*mut c_void>());
        break;
    }

    let pid = std::process::id();
    let mut maps = Vec::with_capacity(modules.len());
    for module in modules {
        let mut info = MODULEINFO::default();
        if unsafe {
            GetModuleInformation(process, module, &mut info, size_of::<MODULEINFO>() as u32)
        } == 0
        {
            continue;
        }
        let mut name = vec![0u16; 32768];
        let len =
            unsafe { GetModuleFileNameExW(process, module, name.as_mut_ptr(), name.len() as u32) };
        if len == 0 || len as usize >= name.len() {
            continue;
        }
        let start = info.lpBaseOfDll as u64;
        maps.push(ProcessMap {
            pid,
            path: std::ffi::OsString::from_wide(&name[..len as usize]).into(),
            start,
            end: start.saturating_add(info.SizeOfImage as u64),
            offset: 0,
        });
    }
    Ok(maps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Resolver;

    #[inline(never)]
    fn pdb_fixture() -> u64 {
        std::hint::black_box(42)
    }

    #[test]
    fn resolves_current_executable_pdb_symbol() {
        assert_eq!(pdb_fixture(), 42);
        let ip = pdb_fixture as *const () as usize as u64;
        let mut maps = current_process_maps().expect("current process modules");
        assert!(maps.iter().any(|map| ip >= map.start && ip < map.end));
        let other_pid = std::process::id().wrapping_add(1);
        maps.extend(maps.clone().into_iter().map(|mut map| {
            map.pid = other_pid;
            map
        }));
        let resolver = Resolver::new(maps);
        for pid in [std::process::id(), other_pid] {
            let frames = resolver.resolve(pid, ip);
            assert!(
                frames
                    .iter()
                    .any(|frame| frame.function.contains("pdb_fixture")),
                "expected PDB function for {ip:#x}, got {frames:?}"
            );
            assert!(
                frames
                    .iter()
                    .any(|frame| frame.file.is_some() && frame.line.is_some()),
                "expected PDB source location for {ip:#x}, got {frames:?}"
            );
        }
    }
}
