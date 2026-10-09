//! Windows implementation of the shared-memory transport.
//!
//! Shared regions use named page-file mappings. The wakeup primitive is a
//! named kernel semaphore; its approximate count is mirrored in the first
//! word of the region so `counter()` keeps the same API as the POSIX backend.

use std::{
    ffi::c_void,
    io::{Error, ErrorKind},
    ptr,
    sync::atomic::{AtomicI32, Ordering},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS, HANDLE,
        INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    System::{
        Memory::{
            CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile,
            FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
        },
        Threading::{
            CreateSemaphoreW, OpenSemaphoreW, ReleaseSemaphore, WaitForSingleObject, INFINITE,
            SEMAPHORE_ALL_ACCESS,
        },
    },
};

pub struct Shmem {
    ptr: *mut c_void,
    mapping: HANDLE,
    is_owning: bool,
    size: usize,
    name: String,
}

pub struct Semaphore {
    handle: HANDLE,
    counter: *mut AtomicI32,
}

fn object_name(name: &str) -> Result<Vec<u16>, Error> {
    // Give the objects a private, stable namespace and normalize POSIX-style
    // names (including the leading slash commonly used by callers).
    let normalized = name.replace(['/', '\\'], "_");
    let name = format!("Local\\miniperf_{normalized}");
    Ok(name.encode_utf16().chain(std::iter::once(0)).collect())
}

fn last_error() -> Error {
    Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

impl Shmem {
    pub fn create(name: &str, size: usize) -> Result<Self, Error> {
        Self::open_or_create(name, size, true)
    }

    pub fn open(name: &str, size: usize) -> Result<Self, Error> {
        Self::open_or_create(name, size, false)
    }

    fn open_or_create(name: &str, size: usize, create: bool) -> Result<Self, Error> {
        if size == 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "shared-memory size must be nonzero",
            ));
        }
        let wide_name = object_name(name)?;
        let size64 = size as u64;
        let handle = unsafe {
            if create {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    ptr::null(),
                    PAGE_READWRITE,
                    (size64 >> 32) as u32,
                    size64 as u32,
                    wide_name.as_ptr(),
                )
            } else {
                OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide_name.as_ptr())
            }
        };
        if handle.is_null() {
            return Err(last_error());
        }
        if create && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(handle) };
            return Err(Error::new(
                ErrorKind::AlreadyExists,
                "named shared-memory mapping already exists",
            ));
        }
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size) };
        let ptr = view.Value;
        if ptr.is_null() {
            let error = last_error();
            unsafe { CloseHandle(handle) };
            return Err(error);
        }
        Ok(Self {
            ptr,
            mapping: handle,
            is_owning: create,
            size,
            name: name.to_owned(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn as_ptr(&self) -> *const () {
        self.ptr.cast()
    }
    pub fn as_mut_ptr(&self) -> *mut () {
        self.ptr.cast()
    }
}

impl Drop for Shmem {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.ptr });
            CloseHandle(self.mapping);
        }
        // Windows removes named mappings automatically after the last handle
        // closes. `is_owning` is retained to match the POSIX API semantics.
        let _ = self.is_owning;
        let _ = self.size;
    }
}

impl Semaphore {
    pub fn create(ptr: *mut (), name: &str) -> Result<Self, Error> {
        let wide_name = object_name(name)?;
        unsafe { SetLastError(0) };
        let handle = unsafe { CreateSemaphoreW(ptr::null(), 0, i32::MAX, wide_name.as_ptr()) };
        if handle.is_null() {
            return Err(last_error());
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(handle) };
            return Err(Error::new(
                ErrorKind::AlreadyExists,
                "named semaphore already exists",
            ));
        }
        let counter = ptr.cast::<AtomicI32>();
        unsafe { (*counter).store(0, Ordering::Relaxed) };
        Ok(Self { handle, counter })
    }

    pub fn open(ptr: *mut (), name: &str) -> Result<Self, Error> {
        let wide_name = object_name(name)?;
        let handle = unsafe { OpenSemaphoreW(SEMAPHORE_ALL_ACCESS, 0, wide_name.as_ptr()) };
        if handle.is_null() {
            return Err(last_error());
        }
        Ok(Self {
            handle,
            counter: ptr.cast(),
        })
    }

    pub fn required_size() -> usize {
        std::mem::size_of::<AtomicI32>()
    }

    pub fn wait(&self) -> Result<(), Error> {
        match unsafe { WaitForSingleObject(self.handle, INFINITE) } {
            WAIT_OBJECT_0 => {
                unsafe { (*self.counter).fetch_sub(1, Ordering::Acquire) };
                Ok(())
            }
            _ => Err(last_error()),
        }
    }

    pub fn try_wait(&self) -> Result<(), Error> {
        match unsafe { WaitForSingleObject(self.handle, 0) } {
            WAIT_OBJECT_0 => {
                unsafe { (*self.counter).fetch_sub(1, Ordering::Acquire) };
                Ok(())
            }
            WAIT_TIMEOUT => Err(Error::from(ErrorKind::WouldBlock)),
            _ => Err(last_error()),
        }
    }

    pub fn post(&self) -> Result<(), Error> {
        unsafe { (*self.counter).fetch_add(1, Ordering::Release) };
        if unsafe { ReleaseSemaphore(self.handle, 1, ptr::null_mut()) } == 0 {
            unsafe { (*self.counter).fetch_sub(1, Ordering::Relaxed) };
            return Err(last_error());
        }
        Ok(())
    }

    pub fn counter(&self) -> Result<i32, Error> {
        Ok(unsafe { (*self.counter).load(Ordering::Acquire) })
    }
}

impl Drop for Semaphore {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

unsafe impl Send for Shmem {}
unsafe impl Send for Semaphore {}
