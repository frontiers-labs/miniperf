pub const MAX_FRAMES: usize = 64;

#[repr(C)]
struct UnwindContext {
    _private: [u8; 0],
}
unsafe extern "C" {
    fn _Unwind_Backtrace(
        callback: extern "C" fn(*mut UnwindContext, *mut std::ffi::c_void) -> i32,
        argument: *mut std::ffi::c_void,
    ) -> i32;
    fn _Unwind_GetIP(context: *mut UnwindContext) -> usize;
}
struct Capture<'a> {
    frames: &'a mut [u64; MAX_FRAMES],
    count: usize,
}
extern "C" fn frame(context: *mut UnwindContext, argument: *mut std::ffi::c_void) -> i32 {
    let capture = unsafe { &mut *argument.cast::<Capture<'_>>() };
    let pc = unsafe { _Unwind_GetIP(context) } as u64;
    if pc == 0 {
        return 0;
    }
    if capture.count == MAX_FRAMES {
        return 5;
    }
    capture.frames[capture.count] = pc.saturating_sub(1);
    capture.count += 1;
    0
}

/// Capture raw PCs into caller-owned storage. Symbolization runs offline.
#[inline(never)]
pub fn capture(frames: &mut [u64; MAX_FRAMES]) -> usize {
    capture_status(frames).frames
}

pub struct CaptureStatus {
    pub frames: usize,
    pub exhausted: bool,
}
pub fn capture_status(frames: &mut [u64; MAX_FRAMES]) -> CaptureStatus {
    let mut capture = Capture { frames, count: 0 };
    let reason = unsafe { _Unwind_Backtrace(frame, (&mut capture as *mut Capture<'_>).cast()) };
    CaptureStatus {
        frames: capture.count,
        exhausted: reason == 5,
    }
}
