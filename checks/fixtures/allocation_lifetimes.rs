//! Real allocator operations with a pointer-level oracle printed after capture.
use std::ffi::{c_char, c_void};
use std::hint::black_box;
use std::time::Instant;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn realloc(pointer: *mut c_void, size: usize) -> *mut c_void;
    fn free(pointer: *mut c_void);
    fn fork() -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn exit(status: i32) -> !;
    fn execl(path: *const c_char, first: *const c_char, ...) -> i32;
    fn getpid() -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
}

#[derive(Clone, Copy)]
struct Operation {
    case: &'static str,
    kind: &'static str,
    old: usize,
    pointer: usize,
    size: usize,
}

#[inline(never)]
unsafe fn allocation_fixture_allocate(size: usize) -> *mut c_void {
    let pointer = unsafe { malloc(black_box(size)) };
    assert!(!pointer.is_null());
    unsafe { pointer.cast::<u8>().write_volatile(43) };
    black_box(pointer)
}

#[inline(never)]
unsafe fn allocation_fixture_resize(pointer: *mut c_void, size: usize) -> *mut c_void {
    black_box(unsafe { realloc(black_box(pointer), black_box(size)) })
}

fn lifetimes() {
    let mut operations = Vec::with_capacity(256);
    let mut push = |case, kind, old: *mut c_void, pointer: *mut c_void, size| {
        operations.push(Operation {
            case,
            kind,
            old: old as usize,
            pointer: pointer as usize,
            size,
        });
    };
    unsafe {
        let retained = allocation_fixture_allocate(120_001);
        push(
            "retained",
            "allocated",
            std::ptr::null_mut(),
            retained,
            120_001,
        );
        for _ in 0..32 {
            let pointer = allocation_fixture_allocate(1237);
            push("reuse", "allocated", std::ptr::null_mut(), pointer, 1237);
            free(pointer);
            push("reuse", "freed", pointer, std::ptr::null_mut(), 0);
        }
        let pointer = allocation_fixture_resize(std::ptr::null_mut(), 1801);
        assert!(!pointer.is_null());
        pointer.cast::<u8>().write_volatile(71);
        push(
            "realloc_null",
            "reallocated",
            std::ptr::null_mut(),
            pointer,
            1801,
        );
        let same = allocation_fixture_resize(pointer, 1789);
        assert_eq!(same, pointer, "shrinking glibc realloc must stay in place");
        push("in_place", "reallocated", pointer, same, 1789);
        let failed = allocation_fixture_resize(same, usize::MAX - 4095);
        assert!(failed.is_null(), "impossible allocation must fail");
        push("failure", "reallocated", same, failed, usize::MAX - 4095);
        assert_eq!(
            same.cast::<u8>().read_volatile(),
            71,
            "failed realloc changed the old block"
        );
        free(same);
        push("after_failure", "freed", same, std::ptr::null_mut(), 0);

        let pointer = allocation_fixture_allocate(2053);
        push("moving", "allocated", std::ptr::null_mut(), pointer, 2053);
        let barrier = allocation_fixture_allocate(2063);
        push("barrier", "allocated", std::ptr::null_mut(), barrier, 2063);
        let moved = allocation_fixture_resize(pointer, 4_000_003);
        assert!(!moved.is_null());
        assert_ne!(moved, pointer, "growth must exercise a moving realloc");
        assert_eq!(moved.cast::<u8>().read_volatile(), 43);
        push("moving", "reallocated", pointer, moved, 4_000_003);
        free(barrier);
        push("barrier", "freed", barrier, std::ptr::null_mut(), 0);
        free(moved);
        push("moving", "freed", moved, std::ptr::null_mut(), 0);

        let zero = allocation_fixture_allocate(3251);
        push("zero", "allocated", std::ptr::null_mut(), zero, 3251);
        let result = allocation_fixture_resize(zero, 0);
        push("zero", "reallocated", zero, result, 0);
        if !result.is_null() {
            free(result);
            push("zero", "freed", result, std::ptr::null_mut(), 0);
        }

        for index in 0..8 {
            let pointer = allocation_fixture_allocate(4153 + index * 16);
            push(
                "cross_thread",
                "allocated",
                std::ptr::null_mut(),
                pointer,
                4153 + index * 16,
            );
            let address = pointer as usize;
            let short = std::thread::spawn(move || {
                free(address as *mut c_void);
                let short = allocation_fixture_allocate(6211 + index * 16);
                free(short);
                short as usize
            })
            .join()
            .unwrap();
            push("cross_thread", "freed", pointer, std::ptr::null_mut(), 0);
            push(
                "short_thread",
                "allocated",
                std::ptr::null_mut(),
                short as *mut c_void,
                6211 + index * 16,
            );
            push(
                "short_thread",
                "freed",
                short as *mut c_void,
                std::ptr::null_mut(),
                0,
            );
        }
    }
    for (sequence, op) in operations.iter().enumerate() {
        println!(
            "{{\"sequence\":{sequence},\"case\":\"{}\",\"kind\":\"{}\",\"old_address\":{},\"address\":{},\"requested_bytes\":{}}}",
            op.case, op.kind, op.old, op.pointer, op.size
        );
    }
}

fn bench(threads: usize, iterations: usize, work: usize) {
    let started = Instant::now();
    let workers: Vec<_> = (0..threads)
        .map(|_| {
            std::thread::spawn(move || {
                let mut checksum = 0_u64;
                let mut bytes = 0_u64;
                let mut cpu_checksum = 0_u64;
                for index in 0..iterations {
                    let mut value = index as u64;
                    for _ in 0..work {
                        value = black_box(value.wrapping_mul(6364136223846793005).wrapping_add(1));
                    }
                    cpu_checksum = cpu_checksum.wrapping_add(value);
                    let size = 8191 + (index % 32) * 16;
                    unsafe {
                        let pointer = allocation_fixture_allocate(size);
                        pointer
                            .cast::<u8>()
                            .add(size - 1)
                            .write_volatile(index as u8);
                        checksum += pointer.cast::<u8>().read_volatile() as u64
                            + pointer.cast::<u8>().add(size - 1).read_volatile() as u64;
                        free(pointer);
                    }
                    bytes += size as u64;
                }
                (checksum, bytes, cpu_checksum)
            })
        })
        .collect();
    let (checksum, bytes, cpu_checksum) = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .fold((0_u64, 0_u64, 0_u64), |(a, b, c), (d, e, f)| {
            (a + d, b + e, c.wrapping_add(f))
        });
    println!(
        "{{\"iterations\":{iterations},\"threads\":{threads},\"allocations\":{},\"requested_bytes\":{bytes},\"checksum\":{checksum},\"cpu_checksum\":{cpu_checksum},\"cpu_work\":{work},\"failures\":0,\"producer_ns\":{}}}",
        iterations * threads,
        started.elapsed().as_nanos()
    );
}

fn fork_scope(exec: bool) {
    unsafe {
        let parent = allocation_fixture_allocate(13579);
        let pid = fork();
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            if exec {
                execl(
                    c"/proc/self/exe".as_ptr(),
                    c"allocation-lifetimes".as_ptr(),
                    c"fork-child".as_ptr(),
                    std::ptr::null::<c_char>(),
                );
                exit(127);
            }
            let child = allocation_fixture_allocate(24681);
            free(child);
            exit(0);
        }
        let mut status = 0;
        assert_eq!(waitpid(pid, &mut status, 0), pid);
        assert_eq!(status, 0, "fork child did not exit normally");
        free(parent);
        println!(
            "{{\"fork_child\":{pid},\"parent_address\":{},\"child_status\":{status}}}",
            parent as usize
        );
    }
}

fn main() {
    let arguments: Vec<_> = std::env::args().collect();
    if arguments.get(1).map(String::as_str) == Some("bench") {
        bench(
            arguments[2].parse().unwrap(),
            arguments[3].parse().unwrap(),
            arguments
                .get(4)
                .map(|value| value.parse().unwrap())
                .unwrap_or(0),
        );
    } else if arguments.get(1).map(String::as_str) == Some("fork-exit") {
        fork_scope(false);
    } else if arguments.get(1).map(String::as_str) == Some("fork-exec") {
        fork_scope(true);
    } else if arguments.get(1).map(String::as_str) == Some("fork-child") {
        unsafe {
            let child = allocation_fixture_allocate(24681);
            free(child);
        }
    } else if arguments.get(1).map(String::as_str) == Some("killed") {
        unsafe {
            let pointer = allocation_fixture_allocate(35111);
            black_box(pointer);
            kill(getpid(), 9);
        }
        panic!("SIGKILL did not terminate the workload");
    } else {
        lifetimes();
    }
}
