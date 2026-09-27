use std::cell::Cell;

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, GENERIC_READ, GENERIC_WRITE, HANDLE,
        INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    },
    System::{
        Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE},
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Threading::{
            CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess, GetExitCodeProcess,
            InitializeProcThreadAttributeList, ResumeThread, TerminateProcess,
            UpdateProcThreadAttribute, WaitForSingleObject, CREATE_SUSPENDED,
            CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, INFINITE,
            PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES,
            STARTUPINFOEXW,
        },
    },
};

#[derive(Debug)]
pub(super) struct WindowsProcess {
    pid: i32,
    process_handle: HANDLE,
    thread_handle: HANDLE,
    job_handle: HANDLE,
    continued: Cell<bool>,
}

impl WindowsProcess {
    pub(super) fn new(args: &[String], env: &[(String, String)]) -> Result<Self, std::io::Error> {
        use std::collections::BTreeMap;
        use std::os::windows::ffi::OsStrExt;

        if args.is_empty() || args[0].is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "process command is empty",
            ));
        }
        if args.iter().any(|arg| arg.contains('\0')) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "process argument contains a NUL byte",
            ));
        }

        let mut command: Vec<u16> = args
            .iter()
            .map(|arg| quote_windows_arg(arg))
            .collect::<Vec<_>>()
            .join(" ")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        // Windows environment names are case insensitive. Preserve inherited
        // values, then apply caller overrides with the same semantics.
        let mut variables = BTreeMap::<String, (std::ffi::OsString, std::ffi::OsString)>::new();
        for (key, value) in std::env::vars_os() {
            if let Some(name) = key.to_str() {
                variables.insert(name.to_ascii_uppercase(), (key, value));
            }
        }
        for (key, value) in env {
            if key.is_empty() || key.contains(['\0', '=']) || value.contains('\0') {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "invalid process environment entry",
                ));
            }
            variables.insert(key.to_ascii_uppercase(), (key.into(), value.into()));
        }
        let mut environment = Vec::<u16>::new();
        for (_, (key, value)) in variables {
            environment.extend(key.encode_wide());
            environment.push('=' as u16);
            environment.extend(value.encode_wide());
            environment.push(0);
        }
        environment.push(0);
        if environment.len() == 1 {
            environment.push(0);
        }

        let stdio = InheritedStdio::new()?;
        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        let inherited = stdio.initialized;
        startup.StartupInfo.cb = if inherited {
            std::mem::size_of::<STARTUPINFOEXW>() as u32
        } else {
            std::mem::size_of_val(&startup.StartupInfo) as u32
        };
        if inherited {
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdInput = stdio.handles[0];
            startup.StartupInfo.hStdOutput = stdio.handles[1];
            startup.StartupInfo.hStdError = stdio.handles[2];
            startup.lpAttributeList = stdio.attributes.as_ptr() as *mut _;
        }
        let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let created = unsafe {
            CreateProcessW(
                std::ptr::null(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                inherited as i32,
                CREATE_SUSPENDED
                    | CREATE_UNICODE_ENVIRONMENT
                    | if inherited {
                        EXTENDED_STARTUPINFO_PRESENT
                    } else {
                        0
                    },
                environment.as_ptr().cast(),
                std::ptr::null(),
                &startup.StartupInfo,
                &mut info,
            )
        };
        if created == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // A job keeps any descendants in the same lifetime as the root.
        // Assignment may be disallowed by a host-imposed job; in that case
        // root process control still works.
        let mut job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if !job.is_null() {
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            } != 0;
            if !configured || unsafe { AssignProcessToJobObject(job, info.hProcess) } == 0 {
                unsafe { CloseHandle(job) };
                job = std::ptr::null_mut();
            }
        }
        Ok(Self {
            pid: info.dwProcessId as i32,
            process_handle: info.hProcess,
            thread_handle: info.hThread,
            job_handle: job,
            continued: Cell::new(false),
        })
    }

    pub(super) fn pid(&self) -> i32 {
        self.pid
    }

    pub(super) fn cont(&self) {
        if !self.continued.replace(true) {
            unsafe { ResumeThread(self.thread_handle) };
        }
    }

    pub(super) fn wait(&self) -> Result<i32, std::io::Error> {
        if unsafe { WaitForSingleObject(self.process_handle, INFINITE) } != WAIT_OBJECT_0 {
            return Err(std::io::Error::last_os_error());
        }
        self.exit_code()
    }

    pub(super) fn try_wait(&self) -> Result<Option<i32>, std::io::Error> {
        let result = unsafe { WaitForSingleObject(self.process_handle, 0) };
        if result == windows_sys::Win32::Foundation::WAIT_TIMEOUT {
            return Ok(None);
        }
        if result != WAIT_OBJECT_0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Some(self.exit_code()?))
    }

    fn exit_code(&self) -> Result<i32, std::io::Error> {
        let mut code = 259;
        if unsafe { GetExitCodeProcess(self.process_handle, &mut code) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(code as i32)
    }

    pub(super) fn terminate(&self) -> Result<i32, std::io::Error> {
        if !self.job_handle.is_null() {
            if unsafe { TerminateJobObject(self.job_handle, 1) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
        } else if self.try_wait()?.is_none()
            && unsafe { TerminateProcess(self.process_handle, 1) } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        self.wait()
    }

    pub(super) fn reap(&self, exited: bool) {
        if !exited {
            if self.job_handle.is_null() {
                unsafe { TerminateProcess(self.process_handle, 1) };
            } else {
                unsafe { TerminateJobObject(self.job_handle, 1) };
            }
            unsafe { WaitForSingleObject(self.process_handle, INFINITE) };
        }
        unsafe {
            CloseHandle(self.thread_handle);
            CloseHandle(self.process_handle);
            if !self.job_handle.is_null() {
                CloseHandle(self.job_handle);
            }
        }
    }
}

struct InheritedStdio {
    handles: [HANDLE; 3],
    attributes: Vec<usize>,
    initialized: bool,
}

impl InheritedStdio {
    fn new() -> Result<Self, std::io::Error> {
        let mut stdio = Self {
            handles: [std::ptr::null_mut(); 3],
            attributes: Vec::new(),
            initialized: false,
        };
        let process = unsafe { GetCurrentProcess() };
        for (index, kind) in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .into_iter()
            .enumerate()
        {
            let mut parent_handle = unsafe { GetStdHandle(kind) };
            let fallback = if parent_handle.is_null() || parent_handle == INVALID_HANDLE_VALUE {
                // A GUI or service parent may have no console handles. Pass valid
                // NUL handles instead of copying missing handles into the child.
                let nul = ['N' as u16, 'U' as u16, 'L' as u16, 0];
                let access = if index == 0 {
                    GENERIC_READ
                } else {
                    GENERIC_WRITE
                };
                parent_handle = unsafe {
                    CreateFileW(
                        nul.as_ptr(),
                        access,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        std::ptr::null(),
                        OPEN_EXISTING,
                        FILE_ATTRIBUTE_NORMAL,
                        std::ptr::null_mut(),
                    )
                };
                if parent_handle == INVALID_HANDLE_VALUE {
                    return Err(std::io::Error::last_os_error());
                }
                true
            } else {
                false
            };
            let duplicated = unsafe {
                DuplicateHandle(
                    process,
                    parent_handle,
                    process,
                    &mut stdio.handles[index],
                    0,
                    1,
                    DUPLICATE_SAME_ACCESS,
                )
            };
            let duplicate_error = (duplicated == 0).then(std::io::Error::last_os_error);
            if fallback {
                unsafe { CloseHandle(parent_handle) };
            }
            if let Some(error) = duplicate_error {
                return Err(error);
            }
        }
        let inherited = stdio
            .handles
            .iter()
            .copied()
            .filter(|handle| !handle.is_null())
            .collect::<Vec<_>>();
        if inherited.is_empty() {
            return Ok(stdio);
        }

        let mut size = 0;
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
        if size == 0 {
            return Err(std::io::Error::last_os_error());
        }
        stdio.attributes = vec![0; size.div_ceil(std::mem::size_of::<usize>())];
        let attributes = stdio.attributes.as_mut_ptr().cast();
        if unsafe { InitializeProcThreadAttributeList(attributes, 1, 0, &mut size) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        stdio.initialized = true;
        if unsafe {
            UpdateProcThreadAttribute(
                attributes,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                inherited.as_ptr().cast(),
                inherited.len() * std::mem::size_of::<HANDLE>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(stdio)
    }
}

impl Drop for InheritedStdio {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { DeleteProcThreadAttributeList(self.attributes.as_mut_ptr().cast()) };
        }
        for handle in self.handles {
            if !handle.is_null() {
                unsafe { CloseHandle(handle) };
            }
        }
    }
}

fn quote_windows_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(char::is_whitespace) && !arg.contains('"') {
        return arg.to_owned();
    }
    let mut result = String::from("\"");
    let mut slashes = 0;
    for ch in arg.chars() {
        match ch {
            '\\' => slashes += 1,
            '"' => {
                result.extend(std::iter::repeat('\\').take(slashes * 2 + 1));
                result.push('"');
                slashes = 0;
            }
            _ => {
                result.extend(std::iter::repeat('\\').take(slashes));
                result.push(ch);
                slashes = 0;
            }
        }
    }
    result.extend(std::iter::repeat('\\').take(slashes * 2));
    result.push('"');
    result
}
