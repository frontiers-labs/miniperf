#![cfg(unix)]
//! miniperf MPI proxy: preloaded into MPI applications. Wraps MPI_Init /
//! MPI_Init_thread / MPI_Finalize via PMPI to (1) record the rank into
//! process metadata, and (2) upgrade cross-node time alignment from
//! NTP-grade to ~10 µs with a ping-pong offset exchange between rank 0 and
//! every other rank at Init and again at Finalize (drift captured by the
//! second pair).
//!
//! Built without MPI headers: PMPI entry points are resolved with dlsym and
//! the two ABI families are told apart at runtime — Open MPI exports its
//! predefined handles as symbols (`ompi_mpi_comm_world`, `ompi_mpi_byte`),
//! everything MPICH-derived uses the well-known integer constants. The rank
//! itself prefers launcher environment variables so it works even when the
//! ABI is unrecognized.

use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicPtr, Ordering};

const CLOCK_TAG: c_int = 0x4d50;
const PING_ROUNDS: usize = 16;

const MPICH_COMM_WORLD: usize = 0x4400_0000;
const MPICH_BYTE: usize = 0x4c00_010d;
const MPICH_STATUS_IGNORE: usize = 1;

type SendFn = unsafe extern "C" fn(*const c_void, c_int, usize, c_int, c_int, usize) -> c_int;
type RecvFn = unsafe extern "C" fn(*mut c_void, c_int, usize, c_int, c_int, usize, usize) -> c_int;
type CommRankFn = unsafe extern "C" fn(usize, *mut c_int) -> c_int;
type InitFn = unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char) -> c_int;
type InitThreadFn =
    unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char, c_int, *mut c_int) -> c_int;
type FinalizeFn = unsafe extern "C" fn() -> c_int;

type SetRankFn = unsafe extern "C" fn(i64);
type ClockSyncFn = unsafe extern "C" fn(u32, *const c_char, i64, i64, i64);
type TimestampFn = unsafe extern "C" fn() -> i64;

static CORE_SET_RANK: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static CORE_CLOCK_SYNC: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static CORE_TIMESTAMP: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

fn core_resolve() -> bool {
    if !CORE_SET_RANK.load(Ordering::Acquire).is_null() {
        return true;
    }
    if std::env::var_os("MPERF_SESSION_DIR").is_none() {
        return false;
    }
    let library = std::env::var("MPERF_COLLECTOR_LIBRARY")
        .unwrap_or_else(|_| "libmperf_collector.so".to_string());
    let Ok(library) = std::ffi::CString::new(library) else {
        return false;
    };
    let core = unsafe { libc::dlopen(library.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if core.is_null() {
        return false;
    }
    let set_rank = unsafe { libc::dlsym(core, c"mperf_trace_set_rank".as_ptr()) };
    let clock_sync = unsafe { libc::dlsym(core, c"mperf_trace_clock_sync".as_ptr()) };
    let timestamp = unsafe { libc::dlsym(core, c"mperf_trace_timestamp".as_ptr()) };
    if set_rank.is_null() || clock_sync.is_null() || timestamp.is_null() {
        return false;
    }
    CORE_CLOCK_SYNC.store(clock_sync, Ordering::Release);
    CORE_TIMESTAMP.store(timestamp, Ordering::Release);
    CORE_SET_RANK.store(set_rank, Ordering::Release);
    true
}

fn timestamp() -> i64 {
    let f: TimestampFn = unsafe { std::mem::transmute(CORE_TIMESTAMP.load(Ordering::Acquire)) };
    unsafe { f() }
}

fn next_symbol(name: &std::ffi::CStr) -> *mut c_void {
    let symbol = unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr()) };
    if !symbol.is_null() {
        return symbol;
    }
    unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) }
}

/// Predefined MPI handles for the runtime found in this process.
struct MpiAbi {
    comm_world: usize,
    byte: usize,
    status_ignore: usize,
}

fn detect_abi() -> Option<MpiAbi> {
    let comm_world = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"ompi_mpi_comm_world".as_ptr()) };
    if !comm_world.is_null() {
        let byte = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"ompi_mpi_byte".as_ptr()) };
        if byte.is_null() {
            return None;
        }
        return Some(MpiAbi {
            comm_world: comm_world as usize,
            byte: byte as usize,
            status_ignore: 0,
        });
    }
    // Every MPICH derivative (MPICH, Intel MPI, MVAPICH, Cray) keeps these
    // constants; anything else is unknown and we skip the exchange.
    if !unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"MPID_Init".as_ptr()) }.is_null()
        || !unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"MPIR_Err_return_comm".as_ptr()) }.is_null()
    {
        return Some(MpiAbi {
            comm_world: MPICH_COMM_WORLD,
            byte: MPICH_BYTE,
            status_ignore: MPICH_STATUS_IGNORE,
        });
    }
    None
}

fn env_number(names: &[&str]) -> Option<i64> {
    names
        .iter()
        .find_map(|name| std::env::var(name).ok()?.trim().parse::<i64>().ok())
}

fn rank_and_size(abi: Option<&MpiAbi>) -> (Option<i64>, Option<i64>) {
    let mut rank = env_number(&[
        "OMPI_COMM_WORLD_RANK",
        "PMIX_RANK",
        "PMI_RANK",
        "SLURM_PROCID",
    ]);
    let size = env_number(&[
        "OMPI_COMM_WORLD_SIZE",
        "PMI_SIZE",
        "SLURM_NTASKS",
        "MPI_LOCALNRANKS",
    ]);
    if rank.is_none()
        && let Some(abi) = abi
    {
        let comm_rank = next_symbol(c"PMPI_Comm_rank");
        if !comm_rank.is_null() {
            let comm_rank: CommRankFn = unsafe { std::mem::transmute(comm_rank) };
            let mut value: c_int = -1;
            if unsafe { comm_rank(abi.comm_world, &mut value) } == 0 {
                rank = Some(value as i64);
            }
        }
    }
    (rank, size)
}

/// Rank 0 answers each peer's pings with its own clock; every other rank
/// estimates the offset via midpoint pairing and keeps the round with the
/// smallest RTT as its uncertainty.
fn clock_exchange(abi: &MpiAbi, rank: i64, size: i64, phase: &std::ffi::CStr) {
    let send = next_symbol(c"PMPI_Send");
    let recv = next_symbol(c"PMPI_Recv");
    if send.is_null() || recv.is_null() || size < 2 {
        return;
    }
    let send: SendFn = unsafe { std::mem::transmute(send) };
    let recv: RecvFn = unsafe { std::mem::transmute(recv) };
    let payload_bytes = 8 as c_int;
    if rank == 0 {
        for peer in 1..size {
            for _ in 0..PING_ROUNDS {
                let mut ping = 0i64;
                unsafe {
                    recv(
                        &mut ping as *mut i64 as *mut c_void,
                        payload_bytes,
                        abi.byte,
                        peer as c_int,
                        CLOCK_TAG,
                        abi.comm_world,
                        abi.status_ignore,
                    );
                }
                let now = timestamp();
                unsafe {
                    send(
                        &now as *const i64 as *const c_void,
                        payload_bytes,
                        abi.byte,
                        peer as c_int,
                        CLOCK_TAG,
                        abi.comm_world,
                    );
                }
            }
        }
    } else {
        let mut best_rtt = i64::MAX;
        let mut best_local = 0i64;
        let mut best_peer = 0i64;
        for _ in 0..PING_ROUNDS {
            let sent = timestamp();
            unsafe {
                send(
                    &sent as *const i64 as *const c_void,
                    payload_bytes,
                    abi.byte,
                    0,
                    CLOCK_TAG,
                    abi.comm_world,
                );
            }
            let mut remote = 0i64;
            unsafe {
                recv(
                    &mut remote as *mut i64 as *mut c_void,
                    payload_bytes,
                    abi.byte,
                    0,
                    CLOCK_TAG,
                    abi.comm_world,
                    abi.status_ignore,
                );
            }
            let received = timestamp();
            let rtt = received - sent;
            if rtt < best_rtt {
                best_rtt = rtt;
                best_local = sent + rtt / 2;
                best_peer = remote;
            }
        }
        let clock_sync: ClockSyncFn =
            unsafe { std::mem::transmute(CORE_CLOCK_SYNC.load(Ordering::Acquire)) };
        unsafe { clock_sync(0, phase.as_ptr(), best_local, best_peer, best_rtt / 2) };
    }
}

fn after_init(phase: &std::ffi::CStr) {
    if !core_resolve() {
        return;
    }
    let abi = detect_abi();
    let (rank, size) = rank_and_size(abi.as_ref());
    if let Some(rank) = rank {
        let set_rank: SetRankFn =
            unsafe { std::mem::transmute(CORE_SET_RANK.load(Ordering::Acquire)) };
        unsafe { set_rank(rank) };
        if let (Some(abi), Some(size)) = (abi, size) {
            clock_exchange(&abi, rank, size, phase);
        }
    }
}

/// # Safety
/// Standard MPI ABI; forwarded verbatim to PMPI_Init.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn MPI_Init(argc: *mut c_int, argv: *mut *mut *mut c_char) -> c_int {
    let next = next_symbol(c"PMPI_Init");
    if next.is_null() {
        return 1;
    }
    let next: InitFn = unsafe { std::mem::transmute(next) };
    let result = unsafe { next(argc, argv) };
    if result == 0 {
        after_init(c"init");
    }
    result
}

/// # Safety
/// Standard MPI ABI; forwarded verbatim to PMPI_Init_thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn MPI_Init_thread(
    argc: *mut c_int,
    argv: *mut *mut *mut c_char,
    required: c_int,
    provided: *mut c_int,
) -> c_int {
    let next = next_symbol(c"PMPI_Init_thread");
    if next.is_null() {
        return 1;
    }
    let next: InitThreadFn = unsafe { std::mem::transmute(next) };
    let result = unsafe { next(argc, argv, required, provided) };
    if result == 0 {
        after_init(c"init");
    }
    result
}

/// # Safety
/// Standard MPI ABI; forwarded verbatim to PMPI_Finalize.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn MPI_Finalize() -> c_int {
    if core_resolve()
        && let Some(abi) = detect_abi()
    {
        let (rank, size) = rank_and_size(Some(&abi));
        if let (Some(rank), Some(size)) = (rank, size) {
            clock_exchange(&abi, rank, size, c"finalize");
        }
    }
    let next = next_symbol(c"PMPI_Finalize");
    if next.is_null() {
        return 1;
    }
    let next: FinalizeFn = unsafe { std::mem::transmute(next) };
    unsafe { next() }
}
