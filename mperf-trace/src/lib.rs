//! Thin Rust wrapper over the miniperf trace C ABI. Like the C static stub,
//! it dlopens the collector core on first use when `MPERF_SESSION_DIR` is
//! set; otherwise every operation is a cheap no-op.

use std::ffi::{CString, c_char, c_void};
use std::sync::OnceLock;

#[repr(C)]
struct RawPayload {
    name: *const c_char,
    function: *const c_char,
    file: *const c_char,
    line: u32,
    column: u32,
    flags: u32,
}

struct Vtable {
    register_: unsafe extern "C" fn(*const RawPayload) -> *mut c_void,
    begin: unsafe extern "C" fn(*mut c_void, u64) -> u64,
    end: unsafe extern "C" fn(*mut c_void, u64),
    instant: unsafe extern "C" fn(*mut c_void, i64),
    counter: unsafe extern "C" fn(*mut c_void, i64),
}

unsafe impl Send for Vtable {}
unsafe impl Sync for Vtable {}

static VTABLE: OnceLock<Option<Vtable>> = OnceLock::new();

fn vtable() -> Option<&'static Vtable> {
    VTABLE
        .get_or_init(|| {
            std::env::var_os("MPERF_SESSION_DIR")?;
            let library = std::env::var("MPERF_COLLECTOR_LIBRARY").unwrap_or_else(|_| {
                if cfg!(windows) {
                    "mperf_collector.dll".to_string()
                } else {
                    "libmperf_collector.so".to_string()
                }
            });
            let library = CString::new(library).ok()?;
            #[cfg(unix)]
            let core = unsafe { libc::dlopen(library.as_ptr(), libc::RTLD_NOW) };
            #[cfg(windows)]
            let core = {
                let wide = library
                    .to_string_lossy()
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect::<Vec<_>>();
                (unsafe { windows_sys::Win32::System::LibraryLoader::LoadLibraryW(wide.as_ptr()) })
                    as *mut c_void
            };
            #[cfg(not(any(unix, windows)))]
            let core = std::ptr::null_mut();
            if core.is_null() {
                return None;
            }
            let resolve = |name: &str| {
                let name = CString::new(name).unwrap();
                #[cfg(unix)]
                let symbol = unsafe { libc::dlsym(core, name.as_ptr()) };
                #[cfg(windows)]
                let symbol = unsafe {
                    windows_sys::Win32::System::LibraryLoader::GetProcAddress(
                        core as windows_sys::Win32::Foundation::HMODULE,
                        name.as_ptr().cast(),
                    )
                    .map(|function| function as *const () as *mut c_void)
                    .unwrap_or(std::ptr::null_mut())
                };
                #[cfg(not(any(unix, windows)))]
                let symbol = std::ptr::null_mut();
                (!symbol.is_null()).then_some(symbol)
            };
            unsafe {
                Some(Vtable {
                    register_: std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(*const RawPayload) -> *mut c_void,
                    >(resolve("mperf_trace_register")?),
                    begin: std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(*mut c_void, u64) -> u64,
                    >(resolve("mperf_trace_begin")?),
                    end: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut c_void, u64)>(
                        resolve("mperf_trace_end")?,
                    ),
                    instant: std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(*mut c_void, i64),
                    >(resolve("mperf_trace_instant")?),
                    counter: std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(*mut c_void, i64),
                    >(resolve("mperf_trace_counter")?),
                })
            }
        })
        .as_ref()
}

/// A registered trace point. Obtain once (e.g. in a `OnceLock`) and reuse.
#[derive(Clone, Copy)]
pub struct TracePoint {
    handle: *mut c_void,
}

unsafe impl Send for TracePoint {}
unsafe impl Sync for TracePoint {}

impl TracePoint {
    pub fn register(name: &str, function: &str, file: &str, line: u32, stack: bool) -> TracePoint {
        let Some(vtable) = vtable() else {
            return TracePoint {
                handle: std::ptr::null_mut(),
            };
        };
        let name = CString::new(name).unwrap_or_default();
        let function = CString::new(function).unwrap_or_default();
        let file = CString::new(file).unwrap_or_default();
        let payload = RawPayload {
            name: name.as_ptr(),
            function: function.as_ptr(),
            file: file.as_ptr(),
            line,
            column: 0,
            flags: stack as u32,
        };
        TracePoint {
            handle: unsafe { (vtable.register_)(&payload) },
        }
    }

    pub fn is_active(&self) -> bool {
        !self.handle.is_null()
    }

    pub fn begin(&self, parent: u64) -> u64 {
        match (self.is_active(), vtable()) {
            (true, Some(vtable)) => unsafe { (vtable.begin)(self.handle, parent) },
            _ => 0,
        }
    }

    pub fn end(&self, instance: u64) {
        if let (true, Some(vtable)) = (self.is_active(), vtable()) {
            unsafe { (vtable.end)(self.handle, instance) }
        }
    }

    pub fn instant(&self, value: i64) {
        if let (true, Some(vtable)) = (self.is_active(), vtable()) {
            unsafe { (vtable.instant)(self.handle, value) }
        }
    }

    pub fn counter(&self, value: i64) {
        if let (true, Some(vtable)) = (self.is_active(), vtable()) {
            unsafe { (vtable.counter)(self.handle, value) }
        }
    }

    /// Begin a span ended when the guard drops.
    pub fn scope(&self) -> ScopeGuard {
        ScopeGuard {
            point: *self,
            instance: self.begin(0),
        }
    }
}

pub struct ScopeGuard {
    point: TracePoint,
    instance: u64,
}

impl ScopeGuard {
    pub fn instance(&self) -> u64 {
        self.instance
    }
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        self.point.end(self.instance);
    }
}

/// Trace a lexical scope: `let _guard = trace_scope!("phase");`
#[macro_export]
macro_rules! trace_scope {
    ($name:expr) => {{
        static POINT: std::sync::OnceLock<$crate::TracePoint> = std::sync::OnceLock::new();
        POINT
            .get_or_init(|| {
                $crate::TracePoint::register($name, module_path!(), file!(), line!(), false)
            })
            .scope()
    }};
}
