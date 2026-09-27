pub const MAX_FRAMES: usize = 64;

/// Best-effort frame-pointer walk of the calling thread's stack. Requires
/// frame pointers; stops at the first implausible frame. Returns the number
/// of return addresses stored in `frames`, most recent call first.
#[inline(never)]
#[cfg(windows)]
pub fn capture(frames: &mut [u64; MAX_FRAMES]) -> usize {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn RtlCaptureStackBackTrace(
            frames_to_skip: u32,
            frames_to_capture: u32,
            back_trace: *mut *mut core::ffi::c_void,
            back_trace_hash: *mut u32,
        ) -> u16;
    }
    let mut addresses = [core::ptr::null_mut(); MAX_FRAMES];
    let count = unsafe {
        RtlCaptureStackBackTrace(
            1,
            MAX_FRAMES.min(62) as u32,
            addresses.as_mut_ptr(),
            core::ptr::null_mut(),
        )
    } as usize;
    for (frame, address) in frames.iter_mut().zip(addresses.iter()).take(count) {
        *frame = *address as usize as u64;
    }
    count
}

#[inline(never)]
#[cfg(not(windows))]
pub fn capture(frames: &mut [u64; MAX_FRAMES]) -> usize {
    let mut fp = current_frame_pointer();
    let mut sp = fp;
    let mut count = 0;
    while count < MAX_FRAMES {
        if fp == 0 || fp & 0x7 != 0 || fp < sp || fp - sp > 8 * 1024 * 1024 {
            break;
        }
        let (next_fp, return_address) = unsafe {
            let fp = fp as *const u64;
            (fp.read_volatile(), fp.add(1).read_volatile())
        };
        if return_address < 0x1000 {
            break;
        }
        frames[count] = return_address;
        count += 1;
        sp = fp;
        fp = next_fp;
    }
    count
}

#[cfg(all(not(windows), target_arch = "x86_64"))]
fn current_frame_pointer() -> u64 {
    let fp: u64;
    unsafe { core::arch::asm!("mov {}, rbp", out(reg) fp) };
    fp
}

#[cfg(all(not(windows), target_arch = "aarch64"))]
fn current_frame_pointer() -> u64 {
    let fp: u64;
    unsafe { core::arch::asm!("mov {}, x29", out(reg) fp) };
    fp
}

#[cfg(all(
    not(windows),
    not(any(target_arch = "x86_64", target_arch = "aarch64"))
))]
fn current_frame_pointer() -> u64 {
    0
}
