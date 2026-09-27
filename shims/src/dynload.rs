//! Small platform loader used by shim cdylibs without a link-time collector dependency.
use std::ffi::{CStr, c_void};

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryA(name: *const u8) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
}

pub unsafe fn load(name: &CStr) -> *mut c_void {
    #[cfg(windows)]
    {
        unsafe { LoadLibraryA(name.as_ptr().cast()) }
    }
    #[cfg(unix)]
    {
        unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) }
    }
}

pub unsafe fn symbol(module: *mut c_void, name: &CStr) -> *mut c_void {
    #[cfg(windows)]
    {
        unsafe { GetProcAddress(module, name.as_ptr().cast()) }
    }
    #[cfg(unix)]
    {
        unsafe { libc::dlsym(module, name.as_ptr()) }
    }
}

pub const CORE_LIBRARY: &str = if cfg!(windows) {
    "mperf_collector.dll"
} else if cfg!(target_os = "macos") {
    "libmperf_collector.dylib"
} else {
    "libmperf_collector.so"
};
