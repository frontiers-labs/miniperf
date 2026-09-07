use std::cell::Cell;
use std::ffi::CString;

#[derive(Debug)]
/// A child process suspended before `execve` so counters can be attached.
pub struct Process {
    pid: i32,
    #[cfg(not(target_os = "macos"))]
    write_fd: i32,
    /// Set once the child has been observed to exit (via `wait`). Until the
    /// process is reaped it lingers as a zombie, which keeps its accounting
    /// (e.g. `proc_pid_rusage` cycles/instructions) queryable by a counting
    /// driver's `stop()` even though the child has already finished.
    exited: Cell<bool>,
    reaped: Cell<bool>,
    exit_code: Cell<Option<i32>>,
}

impl Process {
    /// Creates a child process suspended until [`Process::cont`] is called.
    pub fn new(args: &[String], env: &[(String, String)]) -> Result<Self, std::io::Error> {
        #[cfg(target_os = "macos")]
        {
            Self::new_macos_suspended(args, env)
        }

        #[cfg(not(target_os = "macos"))]
        {
            Self::new_fork_gated(args, env)
        }
    }

    #[cfg(target_os = "macos")]
    fn new_macos_suspended(
        args: &[String],
        env: &[(String, String)],
    ) -> Result<Self, std::io::Error> {
        if args.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "process command is empty",
            ));
        }

        let prog = CString::new(args[0].as_str())?;
        let c_args: Vec<CString> = args
            .iter()
            .map(|arg| CString::new(arg.as_str()))
            .collect::<Result<_, _>>()?;
        let mut c_arg_ptrs: Vec<*mut libc::c_char> = c_args
            .iter()
            .map(|arg| arg.as_ptr() as *mut libc::c_char)
            .collect();
        c_arg_ptrs.push(std::ptr::null_mut());

        let c_env: Vec<CString> = std::env::vars()
            .filter(|(key, _)| !env.iter().any(|(override_key, _)| override_key == key))
            .chain(env.iter().cloned())
            .map(|(key, val)| CString::new(format!("{key}={val}")))
            .collect::<Result<_, _>>()?;
        let mut c_env_ptrs: Vec<*mut libc::c_char> = c_env
            .iter()
            .map(|entry| entry.as_ptr() as *mut libc::c_char)
            .collect();
        c_env_ptrs.push(std::ptr::null_mut());

        let mut attr: libc::posix_spawnattr_t = std::ptr::null_mut();
        let init_rc = unsafe { libc::posix_spawnattr_init(&mut attr) };
        if init_rc != 0 {
            return Err(std::io::Error::from_raw_os_error(init_rc));
        }

        let flags = libc::POSIX_SPAWN_START_SUSPENDED as libc::c_short;
        let flags_rc = unsafe { libc::posix_spawnattr_setflags(&mut attr, flags) };
        if flags_rc != 0 {
            unsafe { libc::posix_spawnattr_destroy(&mut attr) };
            return Err(std::io::Error::from_raw_os_error(flags_rc));
        }

        let mut pid = 0;
        let spawn_rc = unsafe {
            libc::posix_spawn(
                &mut pid,
                prog.as_ptr(),
                std::ptr::null(),
                &attr,
                c_arg_ptrs.as_ptr(),
                c_env_ptrs.as_ptr(),
            )
        };
        unsafe { libc::posix_spawnattr_destroy(&mut attr) };
        if spawn_rc != 0 {
            return Err(std::io::Error::from_raw_os_error(spawn_rc));
        }

        Ok(Process {
            pid,
            exited: Cell::new(false),
            reaped: Cell::new(false),
            exit_code: Cell::new(None),
        })
    }

    #[cfg(not(target_os = "macos"))]
    fn new_fork_gated(args: &[String], env: &[(String, String)]) -> Result<Self, std::io::Error> {
        let mut pipe_fds: [libc::c_int; 2] = [-1; 2];
        if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } == -1 {
            return Err(std::io::Error::last_os_error());
        }

        let child_pid = unsafe { libc::fork() };
        if child_pid == -1 {
            panic!()
        }

        if child_pid == 0 {
            let prog = CString::new(args[0].clone())?;
            let c_args: Vec<CString> = args
                .iter()
                .map(|arg| CString::new(arg.as_str()).unwrap())
                .collect();
            let mut c_arg_ptrs: Vec<*const libc::c_char> =
                c_args.iter().map(|arg| arg.as_ptr()).collect();
            c_arg_ptrs.push(std::ptr::null());

            let mut c_env: Vec<CString> = std::env::vars()
                .filter(|(key, _)| !env.iter().any(|(override_key, _)| override_key == key))
                .chain(env.iter().cloned())
                .map(|(key, val)| CString::new(format!("{}={}", key, val)).unwrap())
                .collect();
            c_env.push(
                CString::new(format!("MPERF_PROFILE_ROOT_PID={}", unsafe {
                    libc::getpid()
                }))
                .unwrap(),
            );
            let mut c_env_ptrs: Vec<*const libc::c_char> =
                c_env.iter().map(|env| env.as_ptr()).collect();
            c_env_ptrs.push(std::ptr::null());

            // Wait for parent signal
            let mut buf = [0u8; 1];
            unsafe { libc::read(pipe_fds[0], buf.as_mut_ptr() as *mut libc::c_void, 1) };
            unsafe { libc::close(pipe_fds[0]) };

            unsafe {
                if libc::execve(prog.as_ptr(), c_arg_ptrs.as_ptr(), c_env_ptrs.as_ptr()) == -1 {
                    // If we get here, exec failed
                    let err = std::io::Error::last_os_error();
                    eprintln!("execve failed: {err}");
                    libc::_exit(1);
                }
            }
        }

        unsafe { libc::close(pipe_fds[0]) };
        Ok(Process {
            pid: child_pid,
            write_fd: pipe_fds[1],
            exited: Cell::new(false),
            reaped: Cell::new(false),
            exit_code: Cell::new(None),
        })
    }

    /// Returns the child process identifier.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Releases the child to execute its configured command.
    pub fn cont(&self) {
        #[cfg(target_os = "macos")]
        unsafe {
            libc::kill(self.pid, libc::SIGCONT);
        }

        #[cfg(not(target_os = "macos"))]
        unsafe {
            libc::write(self.write_fd, &[1u8] as *const u8 as *const libc::c_void, 1);
            libc::close(self.write_fd);
        }
    }

    /// Block until the child exits, but leave it unreaped (a zombie) so that its
    /// final resource accounting stays queryable. Reaping happens on drop.
    pub fn wait(&self) -> Result<(), std::io::Error> {
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            if libc::waitid(
                libc::P_PID,
                self.pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            ) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            let status = info.si_status();
            self.exit_code
                .set(Some(if info.si_code == libc::CLD_EXITED {
                    status
                } else {
                    128_i32.saturating_add(status)
                }));
        }
        self.exited.set(true);
        Ok(())
    }

    /// Whether the child has already exited, without blocking. Like
    /// [`Process::wait`], it leaves the zombie in place so the child's final
    /// resource accounting stays queryable.
    pub fn try_wait(&self) -> Result<bool, std::io::Error> {
        if self.exited.get() {
            return Ok(true);
        }
        unsafe {
            // waitid leaves the struct untouched when nothing has exited, so
            // a zeroed si_pid is what "still running" looks like.
            let mut info: libc::siginfo_t = std::mem::zeroed();
            if libc::waitid(
                libc::P_PID,
                self.pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            ) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            if info.si_pid() == 0 {
                return Ok(false);
            }
            let status = info.si_status();
            self.exit_code
                .set(Some(if info.si_code == libc::CLD_EXITED {
                    status
                } else {
                    128_i32.saturating_add(status)
                }));
        }
        self.exited.set(true);
        Ok(true)
    }

    /// Returns the conventional process exit code after [`Process::wait`].
    /// Signal termination is represented as `128 + signal`.
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code.get()
    }

    /// Reap the child if it has exited, releasing the zombie. Idempotent.
    fn reap(&self) {
        if self.reaped.get() {
            return;
        }
        // Process owns the spawned child. If setup fails before `cont()` (for
        // example a denied kperf session), leaving a suspended/pre-exec child
        // behind is worse than terminating it during cleanup.
        if !self.exited.get() {
            unsafe {
                libc::kill(self.pid, libc::SIGKILL);
            }
        }
        let mut status: libc::c_int = 0;
        let rc = unsafe { libc::waitpid(self.pid, &mut status, 0) };
        if rc == self.pid || rc == -1 {
            self.reaped.set(true);
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.reap();
    }
}
