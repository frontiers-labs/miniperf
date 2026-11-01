use std::ffi::{c_void, CString};
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicUsize, Ordering};

use libc::{
    close, ftruncate, mmap, munmap, shm_open, shm_unlink, MAP_FAILED, MAP_SHARED, O_CREAT, O_RDWR,
    PROT_WRITE, S_IRUSR, S_IWUSR,
};

pub struct Shmem {
    ptr: *mut libc::c_void,
    fd: i32,
    is_owning: bool,
    size: usize,
    name: String,
}

pub struct Semaphore {
    sem: *mut libc::sem_t,
    #[cfg(not(target_os = "macos"))]
    is_owning: bool,
    #[cfg(target_os = "macos")]
    name: Option<CString>,
    #[cfg(target_os = "macos")]
    counter: *mut AtomicUsize,
}

impl Shmem {
    pub fn create(name: &str, size: usize) -> Result<Self, std::io::Error> {
        let (fd, ptr) = unsafe { Self::raw_parts(name, size, O_RDWR | O_CREAT)? };

        Ok(Self {
            ptr,
            fd,
            is_owning: true,
            size,
            name: name.to_owned(),
        })
    }

    pub fn open(name: &str, size: usize) -> Result<Self, std::io::Error> {
        let (fd, ptr) = unsafe { Self::raw_parts(name, size, O_RDWR)? };

        Ok(Self {
            ptr,
            fd,
            is_owning: false,
            size,
            name: name.to_owned(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn as_ptr(&self) -> *const () {
        self.ptr as *const ()
    }

    pub fn as_mut_ptr(&self) -> *mut () {
        self.ptr as *mut ()
    }

    unsafe fn raw_parts(
        name: &str,
        size: usize,
        flags: i32,
    ) -> Result<(i32, *mut c_void), std::io::Error> {
        let name = CString::new(name)?;
        let fd = shm_open(name.as_ptr(), flags, (S_IRUSR | S_IWUSR) as libc::c_uint);

        if fd == -1 {
            eprintln!("fd == -1\n");
            return Err(std::io::Error::last_os_error());
        }

        if flags & O_CREAT != 0 && ftruncate(fd, size as i64) == -1 {
            eprintln!("ftruncate fail\n");
            return Err(std::io::Error::last_os_error());
        }

        let addr = mmap(std::ptr::null_mut(), size, PROT_WRITE, MAP_SHARED, fd, 0);

        if addr == MAP_FAILED {
            eprintln!("mmap failed");
            return Err(std::io::Error::last_os_error());
        }

        Ok((fd, addr))
    }
}

impl Drop for Shmem {
    fn drop(&mut self) {
        unsafe {
            munmap(self.ptr, self.size);
            close(self.fd);
        }
        if self.is_owning {
            unsafe {
                if let Ok(name) = CString::new(self.name.as_str()) {
                    shm_unlink(name.as_ptr());
                }
            }
        }
    }
}

impl Semaphore {
    #[cfg(not(target_os = "macos"))]
    pub fn create(ptr: *mut (), _name: &str) -> Result<Self, std::io::Error> {
        if unsafe { libc::sem_init(ptr as *mut libc::sem_t, 1, 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(Self {
            sem: ptr as *mut libc::sem_t,
            is_owning: true,
        })
    }

    #[cfg(target_os = "macos")]
    pub fn create(ptr: *mut (), name: &str) -> Result<Self, std::io::Error> {
        let name = CString::new(name)?;
        let sem = unsafe {
            libc::sem_open(
                name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL,
                (S_IRUSR | S_IWUSR) as libc::c_uint,
                0,
            )
        };
        if sem == libc::SEM_FAILED {
            return Err(std::io::Error::last_os_error());
        }

        let counter = ptr.cast::<AtomicUsize>();
        unsafe { &*counter }.store(0, Ordering::Relaxed);

        Ok(Self {
            sem,
            name: Some(name),
            counter,
        })
    }

    #[cfg(not(target_os = "macos"))]
    pub fn open(ptr: *mut (), _name: &str) -> Result<Self, std::io::Error> {
        Ok(Self {
            sem: ptr as *mut libc::sem_t,
            is_owning: false,
        })
    }

    #[cfg(target_os = "macos")]
    pub fn open(ptr: *mut (), name: &str) -> Result<Self, std::io::Error> {
        let name = CString::new(name)?;
        let sem = unsafe { libc::sem_open(name.as_ptr(), 0) };
        if sem == libc::SEM_FAILED {
            return Err(std::io::Error::last_os_error());
        }

        Ok(Self {
            sem,
            name: None,
            counter: ptr.cast::<AtomicUsize>(),
        })
    }

    #[cfg(not(target_os = "macos"))]
    pub fn required_size() -> usize {
        std::mem::size_of::<libc::sem_t>()
    }

    #[cfg(target_os = "macos")]
    pub fn required_size() -> usize {
        std::mem::size_of::<AtomicUsize>()
    }

    pub fn wait(&self) -> Result<(), std::io::Error> {
        unsafe {
            if libc::sem_wait(self.sem) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }

        #[cfg(target_os = "macos")]
        unsafe {
            (&*self.counter).fetch_sub(1, Ordering::Acquire);
        }

        Ok(())
    }

    pub fn try_wait(&self) -> Result<(), std::io::Error> {
        unsafe {
            if libc::sem_trywait(self.sem) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }

        #[cfg(target_os = "macos")]
        unsafe {
            (&*self.counter).fetch_sub(1, Ordering::Acquire);
        }

        Ok(())
    }

    pub fn post(&self) -> Result<(), std::io::Error> {
        #[cfg(target_os = "macos")]
        unsafe {
            (&*self.counter).fetch_add(1, Ordering::Release);
        }

        unsafe {
            if libc::sem_post(self.sem) != 0 {
                #[cfg(target_os = "macos")]
                (&*self.counter).fetch_sub(1, Ordering::Relaxed);
                return Err(std::io::Error::last_os_error());
            }
        }

        Ok(())
    }

    pub fn counter(&self) -> Result<i32, std::io::Error> {
        #[cfg(target_os = "macos")]
        return unsafe { Ok((&*self.counter).load(Ordering::Acquire) as i32) };

        #[cfg(not(target_os = "macos"))]
        let mut res = 0;
        #[cfg(not(target_os = "macos"))]
        unsafe {
            if libc::sem_getvalue(self.sem, &mut res as *mut i32) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }

        #[cfg(not(target_os = "macos"))]
        Ok(res)
    }
}

impl Drop for Semaphore {
    fn drop(&mut self) {
        #[cfg(not(target_os = "macos"))]
        if self.is_owning {
            unsafe {
                libc::sem_destroy(self.sem);
            }
        }

        #[cfg(target_os = "macos")]
        unsafe {
            libc::sem_close(self.sem);
            if let Some(name) = &self.name {
                libc::sem_unlink(name.as_ptr());
            }
        }
    }
}

unsafe impl Send for Shmem {}
unsafe impl Send for Semaphore {}

#[cfg(test)]
mod tests {
    use super::{Semaphore, Shmem};

    #[test]
    fn shmem_updates() {
        let name = format!("/shmem_updates_{}", std::process::id());
        let shmem1 = Shmem::create(&name, 32).expect("failed to create shmem");
        let shmem2 = Shmem::open(&name, 32).expect("failed to open shmem");

        unsafe {
            *(shmem1.as_mut_ptr() as *mut i32) = 42;
        };

        let result = unsafe { *(shmem2.as_ptr() as *const i32) };

        assert_eq!(result, 42);
    }

    #[test]
    fn shared_semaphores() {
        let name = format!("/shared_semaphores_{}", std::process::id());
        let sem_name = format!("{name}_sem");
        let shmem1 = Shmem::create(&name, 32).expect("failed to create shmem");
        let shmem2 = Shmem::open(&name, 32).expect("failed to open shmem");

        let semaphore1 = Semaphore::create(shmem1.as_mut_ptr(), &sem_name)
            .expect("failed to create a semaphore");

        semaphore1.post().expect("failed to post a semaphore");

        assert_eq!(semaphore1.counter().unwrap(), 1);

        let semaphore2 =
            Semaphore::open(shmem2.as_mut_ptr(), &sem_name).expect("failed to open a semaphore");

        assert_eq!(semaphore2.counter().unwrap(), 1);

        assert!(semaphore2.wait().is_ok());
        assert_eq!(semaphore2.counter().unwrap(), 0);
        assert_eq!(semaphore1.counter().unwrap(), 0);
    }
}
