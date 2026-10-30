//! Turning a captured stack dump into frames.
//!
//! A sampling driver can only copy raw registers and a slice of the user stack
//! out of the interrupted thread; walking that into a call stack needs the
//! binaries' unwind tables and happens after the recording. That walk is still
//! part of measuring, so it lives next to the driver that produced the bytes.
//!
//! Where the host has no DWARF unwinder for its architecture, [`PostHocUnwinder`]
//! still builds and still returns the kernel's own callchain — callers never
//! branch on the platform.

use smallvec::SmallVec;

use crate::sink::ProcAddr;

/// Frames of one resolved call stack, innermost first.
pub type Frames = SmallVec<[u64; 32]>;

/// One sample's raw stack capture, as the driver recorded it.
pub struct StackSample<'a> {
    /// Process the sample came from; unwind tables are per-process.
    pub pid: u32,
    /// Identifier shared by every sample of one counter group. Samples in a
    /// group are captured together, so one stack covers all of them.
    pub group_id: u64,
    /// `PERF_SAMPLE_REGS_USER` mask describing `regs`.
    pub regs_mask: u64,
    /// Register values, ordered by increasing set-bit index in `regs_mask`.
    pub regs: &'a [u64],
    /// User stack bytes beginning at the sampled stack pointer.
    pub user_stack: &'a [u8],
    /// The kernel's own frame-pointer callchain, used when unwinding fails.
    pub callchain: &'a [u64],
}

/// Per-process module tables and caches, used after recording has completed.
pub struct PostHocUnwinder {
    dwarf: dwarf::Unwinder,
    last_stack: Option<(u64, Frames)>,
}

impl PostHocUnwinder {
    /// Load unwind tables for every module in a recording.
    pub fn new(modules: &[ProcAddr]) -> Self {
        PostHocUnwinder {
            dwarf: dwarf::Unwinder::new(modules),
            last_stack: None,
        }
    }

    /// Resolve the deepest stack available: the kernel callchain or the DWARF
    /// unwind of the user stack dump, whichever walked further, then raw IP.
    ///
    /// Samples without their own register capture inherit the stack of the
    /// group they were captured with.
    pub fn resolve(&mut self, sample: &StackSample<'_>) -> Frames {
        let mut frames = Frames::from_slice(sample.callchain);
        if sample.regs_mask != 0 {
            let unwound = self
                .dwarf
                .unwind(sample)
                .filter(|stack| stack.len() > frames.len());
            match unwound {
                Some(stack) => frames = stack.into_iter().collect(),
                None if frames.is_empty() => {
                    frames.extend(dwarf::instruction_pointer(sample));
                }
                None => {}
            }
            self.last_stack = Some((sample.group_id, frames.clone()));
        } else if let Some((group_id, stack)) = &self.last_stack {
            if *group_id == sample.group_id {
                frames.clone_from(stack);
            }
        }
        frames
    }
}

impl StackSample<'_> {
    /// One architecture register out of the capture, by its perf register
    /// index. `None` when the sample did not include it.
    pub fn register(&self, index: u32) -> Option<u64> {
        let bit = 1_u64.checked_shl(index)?;
        if self.regs_mask & bit == 0 {
            return None;
        }
        let value_index = (self.regs_mask & bit.wrapping_sub(1)).count_ones() as usize;
        self.regs.get(value_index).copied()
    }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod dwarf {
    use std::{collections::BTreeMap, fs, path::Path};

    use framehop::{CacheNative, MayAllocateDuringUnwind, Module, Unwinder as _, UnwinderNative};
    use framehop_object::ObjectSectionInfo;
    use object::{Object, ObjectSegment};

    use super::{ProcAddr, StackSample};

    type NativeUnwinder = UnwinderNative<Vec<u8>, MayAllocateDuringUnwind>;
    type NativeCache = CacheNative<MayAllocateDuringUnwind>;

    pub(super) struct Unwinder {
        unwinders: BTreeMap<u32, NativeUnwinder>,
        caches: BTreeMap<u32, NativeCache>,
    }

    impl Unwinder {
        pub(super) fn new(modules: &[ProcAddr]) -> Self {
            let mut mappings = BTreeMap::<(u32, &str), Vec<&ProcAddr>>::new();
            for map in modules {
                if map.filename.is_empty() || map.filename.starts_with('[') || map.len == 0 {
                    continue;
                }
                mappings
                    .entry((map.pid, map.filename.as_str()))
                    .or_default()
                    .push(map);
            }

            let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
            let mut unwinders = BTreeMap::<u32, NativeUnwinder>::new();
            for ((pid, filename), maps) in mappings {
                let Ok(bytes) = fs::read(Path::new(filename)) else {
                    continue;
                };
                let Ok(object) = object::File::parse(bytes.as_slice()) else {
                    continue;
                };
                // Coalesce executable mappings by the object's true load bias.
                // Keep insertion sorted even for old recordings with conflicting
                // overlapping mappings.
                let mut ranges = BTreeMap::<u64, (u64, u64)>::new();
                for map in maps {
                    let segment = object
                        .segments()
                        .filter(|segment| {
                            let (offset, size) = segment.file_range();
                            map.pgoff >= offset / page_size * page_size
                                && map.pgoff < offset.saturating_add(size)
                        })
                        .min_by_key(|segment| segment.file_range().0.abs_diff(map.pgoff));
                    let Some(segment) = segment else {
                        continue;
                    };
                    let (offset, _) = segment.file_range();
                    // Linux maps from a page-aligned file offset. The segment's
                    // virtual address can differ from its file offset.
                    let svma = segment
                        .address()
                        .saturating_add(map.pgoff)
                        .saturating_sub(offset);
                    let base = map.addr.saturating_sub(svma);
                    let end = map.addr.saturating_add(map.len);
                    ranges
                        .entry(base)
                        .and_modify(|range| {
                            range.0 = range.0.min(map.addr);
                            range.1 = range.1.max(end);
                        })
                        .or_insert((map.addr, end));
                }
                for (base, (start, end)) in ranges {
                    let module = Module::<Vec<u8>>::new(
                        filename.to_owned(),
                        start..end,
                        base,
                        ObjectSectionInfo::from_ref(&object),
                    );
                    unwinders.entry(pid).or_default().add_module(module);
                }
            }

            Unwinder {
                unwinders,
                caches: BTreeMap::new(),
            }
        }

        pub(super) fn unwind(&mut self, sample: &StackSample<'_>) -> Option<Vec<u64>> {
            if sample.user_stack.is_empty() {
                return None;
            }
            let initial_regs = native_regs(sample)?;
            let stack_pointer = native_stack_pointer(sample)?;
            let pc = instruction_pointer(sample)?;
            let unwinder = self.unwinders.get(&sample.pid)?;
            let cache = self.caches.entry(sample.pid).or_default();
            let stack = sample.user_stack;
            let mut read_stack = |address: u64| -> Result<u64, ()> {
                let offset = address.checked_sub(stack_pointer).ok_or(())? as usize;
                let bytes: [u8; 8] = stack
                    .get(offset..offset.checked_add(8).ok_or(())?)
                    .ok_or(())?
                    .try_into()
                    .map_err(|_| ())?;
                Ok(u64::from_ne_bytes(bytes))
            };
            let mut iter = unwinder.iter_frames(pc, initial_regs, cache, &mut read_stack);
            let mut frames = Vec::new();
            while frames.len() < 512 {
                match iter.next() {
                    Ok(Some(frame)) => frames.push(frame.address()),
                    Ok(None) | Err(_) => break,
                }
            }
            // A lone PC did not actually unwind; retain the kernel callchain.
            (frames.len() > 1).then_some(frames)
        }
    }

    #[cfg(target_arch = "x86_64")]
    pub(super) fn instruction_pointer(sample: &StackSample<'_>) -> Option<u64> {
        sample.register(8)
    }

    #[cfg(target_arch = "x86_64")]
    fn native_stack_pointer(sample: &StackSample<'_>) -> Option<u64> {
        sample.register(7)
    }

    #[cfg(target_arch = "x86_64")]
    fn native_regs(sample: &StackSample<'_>) -> Option<framehop::UnwindRegsNative> {
        Some(framehop::x86_64::UnwindRegsX86_64::new(
            instruction_pointer(sample)?,
            native_stack_pointer(sample)?,
            sample.register(6)?,
        ))
    }

    #[cfg(target_arch = "aarch64")]
    pub(super) fn instruction_pointer(sample: &StackSample<'_>) -> Option<u64> {
        sample.register(32)
    }

    #[cfg(target_arch = "aarch64")]
    fn native_stack_pointer(sample: &StackSample<'_>) -> Option<u64> {
        sample.register(31)
    }

    #[cfg(target_arch = "aarch64")]
    fn native_regs(sample: &StackSample<'_>) -> Option<framehop::UnwindRegsNative> {
        Some(framehop::aarch64::UnwindRegsAarch64::new(
            sample.register(30)?,
            native_stack_pointer(sample)?,
            sample.register(29)?,
        ))
    }
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
mod dwarf {
    use super::{ProcAddr, StackSample};

    /// No DWARF unwinder for this architecture: the kernel callchain stands.
    pub(super) struct Unwinder;

    impl Unwinder {
        pub(super) fn new(_modules: &[ProcAddr]) -> Self {
            Unwinder
        }

        pub(super) fn unwind(&mut self, _sample: &StackSample<'_>) -> Option<Vec<u64>> {
            None
        }
    }

    pub(super) fn instruction_pointer(_sample: &StackSample<'_>) -> Option<u64> {
        None
    }
}
