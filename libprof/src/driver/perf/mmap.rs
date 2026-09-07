use std::{ffi::CStr, sync::atomic::AtomicU64};

use perf_event_open_sys::bindings::{
    perf_event_header, perf_event_mmap_page, PERF_RECORD_LOST, PERF_RECORD_MMAP, PERF_RECORD_SAMPLE,
};
use smallvec::{SmallVec, ToSmallVec};

use super::branch::{BranchMode, BranchRecord};
use crate::sink::UserRegs;

pub struct Records {
    metadata: *mut perf_event_mmap_page,
    sample_regs_user: u64,
    branch_mode: Option<BranchMode>,
    memory_layout: bool,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub(crate) struct EventValue {
    pub value: u64,
    pub id: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct BranchEntry {
    from: u64,
    to: u64,
    flags: u64,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // Avoid another allocation per perf ring record.
pub enum MmapRecord {
    Sample {
        ip: u64,
        pid: u32,
        tid: u32,
        cpu: u32,
        time: u64,
        time_enabled: u64,
        time_running: u64,
        values: SmallVec<[EventValue; 4]>,
        callstack: SmallVec<[u64; 8]>,
        lbr_callstack: SmallVec<[u64; 8]>,
        user_regs: Option<UserRegs>,
        user_stack: Vec<u8>,
    },
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    MemSample {
        ip: u64,
        pid: u32,
        tid: u32,
        cpu: u32,
        time: u64,
        data_addr: u64,
        latency: u64,
        data_src: u64,
        callstack: SmallVec<[u64; 8]>,
        lbr_callstack: SmallVec<[u64; 8]>,
        user_regs: Option<UserRegs>,
        user_stack: Vec<u8>,
    },
    Address {
        pid: u32,
        start: u64,
        len: u64,
        offset: u64,
        filename: String,
    },
    Lost {
        count: u64,
    },
    Unknown,
}

/// Internal structure for reading from mmap ring buffer
#[repr(C)]
pub struct SampleFormat {
    header: perf_event_header,
    ip: u64,
    pid: u32,
    tid: u32,
    time: u64,
    id: u64,
    cpu: u32,
    _res: u32, // Reserved unused value
    read: ReadFormat,
}

#[repr(C)]
pub struct ReadFormat {
    pub nr: u64,
    pub time_enabled: u64,
    pub time_running: u64,
}

#[repr(C)]
struct ProcMmap {
    header: perf_event_header,
    pid: u32,
    tid: u32,
    addr: u64,
    len: u64,
    pgoff: u64,
    // Filename
}

#[repr(C)]
struct LostRecord {
    header: perf_event_header,
    id: u64,
    lost: u64,
}

impl Records {
    pub fn from_ptr(
        ptr: *mut u8,
        sample_regs_user: u64,
        branch_mode: Option<BranchMode>,
    ) -> Records {
        Records {
            metadata: ptr as *mut perf_event_mmap_page,
            sample_regs_user,
            branch_mode,
            memory_layout: false,
        }
    }

    /// Parse samples using the precise-memory layout (`PERF_SAMPLE_ADDR` +
    /// `WEIGHT_STRUCT` + `DATA_SRC` instead of grouped counter reads).
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    pub fn memory(ptr: *mut u8, sample_regs_user: u64, branch_mode: Option<BranchMode>) -> Records {
        Records {
            metadata: ptr as *mut perf_event_mmap_page,
            sample_regs_user,
            branch_mode,
            memory_layout: true,
        }
    }

    unsafe fn data_head(&self) -> u64 {
        let atomic = AtomicU64::from_ptr(&mut (*self.metadata).data_head as *mut u64);
        atomic.load(std::sync::atomic::Ordering::Acquire)
    }

    unsafe fn data_tail(&self) -> u64 {
        let atomic = AtomicU64::from_ptr(&mut (*self.metadata).data_tail as *mut u64);
        atomic.load(std::sync::atomic::Ordering::Acquire)
    }

    fn data_size(&self) -> usize {
        unsafe { (*self.metadata).data_size as usize }
    }

    fn data_offset(&self) -> usize {
        unsafe { (*self.metadata).data_offset as usize }
    }

    fn update_tail(&mut self, offset: usize) {
        unsafe {
            let atomic = AtomicU64::from_ptr(&mut (*self.metadata).data_tail as *mut u64);
            atomic.fetch_add(offset as u64, std::sync::atomic::Ordering::Release);
        }
    }

    unsafe fn copy_from_ring(&self, absolute_offset: u64, dst: &mut [u8]) {
        let data_offset = self.data_offset();
        let data_size = self.data_size();
        let base_ptr = (self.metadata as *const u8).add(data_offset);

        let mut remaining = dst.len();
        let mut dst_offset = 0usize;
        let mut abs_offset = absolute_offset;

        while remaining > 0 {
            let start = (abs_offset % data_size as u64) as usize;
            let chunk = std::cmp::min(remaining, data_size - start);
            std::ptr::copy_nonoverlapping(
                base_ptr.add(start),
                dst.as_mut_ptr().add(dst_offset),
                chunk,
            );
            dst_offset += chunk;
            abs_offset += chunk as u64;
            remaining -= chunk;
        }
    }
}

impl Iterator for Records {
    type Item = MmapRecord;

    fn next(&mut self) -> Option<Self::Item> {
        let data_head = unsafe { self.data_head() };
        let data_tail = unsafe { self.data_tail() };

        if data_tail == data_head {
            return None;
        }

        let available = data_head.saturating_sub(data_tail);
        let header_size = std::mem::size_of::<perf_event_header>();

        if available < header_size as u64 {
            return None;
        }

        let mut header_buf = [0u8; std::mem::size_of::<perf_event_header>()];
        unsafe {
            self.copy_from_ring(data_tail, &mut header_buf);
        }

        let header =
            unsafe { std::ptr::read_unaligned(header_buf.as_ptr() as *const perf_event_header) };

        if header.size == 0 {
            self.update_tail(header_size);
            return Some(MmapRecord::Unknown);
        }

        let record_size = header.size as usize;

        if record_size < header_size {
            self.update_tail(header_size);
            return Some(MmapRecord::Unknown);
        }

        if record_size > self.data_size() {
            self.update_tail(header_size);
            return Some(MmapRecord::Unknown);
        }

        if available < header.size as u64 {
            return None;
        }

        let mut record_buf = vec![0u8; record_size];
        unsafe {
            self.copy_from_ring(data_tail, &mut record_buf);
        }

        self.update_tail(record_size);

        let record = match header.type_ {
            PERF_RECORD_SAMPLE if self.memory_layout => {
                MemSampleFormat::parse(&record_buf, self.sample_regs_user, self.branch_mode)
                    .unwrap_or(MmapRecord::Unknown)
            }
            PERF_RECORD_SAMPLE => match SampleFormat::read_from_bytes(&record_buf) {
                Some(sample_format) => {
                    let values = sample_format.read_values(&record_buf);
                    let callstack = sample_format.read_callchain(&record_buf);
                    let lbr_callstack = match self.branch_mode {
                        Some(mode) => sample_format.read_branch_callstack(&record_buf, mode),
                        None => SmallVec::new(),
                    };
                    let (user_regs, user_stack) = sample_format.read_user_state(
                        &record_buf,
                        self.sample_regs_user,
                        self.branch_mode.is_some(),
                    );

                    MmapRecord::Sample {
                        ip: sample_format.ip,
                        pid: sample_format.pid,
                        tid: sample_format.tid,
                        cpu: sample_format.cpu,
                        time: sample_format.time,
                        time_enabled: sample_format.read.time_enabled,
                        time_running: sample_format.read.time_running,
                        values,
                        callstack,
                        lbr_callstack,
                        user_regs,
                        user_stack,
                    }
                }
                None => MmapRecord::Unknown,
            },
            PERF_RECORD_MMAP => match ProcMmap::read_from_bytes(&record_buf) {
                Some(mmap_record) => {
                    let filename = ProcMmap::filename(&record_buf);
                    MmapRecord::Address {
                        pid: mmap_record.pid,
                        start: mmap_record.addr,
                        len: mmap_record.len,
                        offset: mmap_record.pgoff,
                        filename,
                    }
                }
                None => MmapRecord::Unknown,
            },
            PERF_RECORD_LOST => LostRecord::read_from_bytes(&record_buf)
                .map(|record| MmapRecord::Lost { count: record.lost })
                .unwrap_or(MmapRecord::Unknown),
            _ => MmapRecord::Unknown,
        };

        Some(record)
    }
}

impl SampleFormat {
    fn read_from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < std::mem::size_of::<Self>() {
            return None;
        }

        Some(unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const Self) })
    }

    fn read_values(&self, record: &[u8]) -> SmallVec<[EventValue; 4]> {
        let values_offset = std::mem::size_of::<SampleFormat>();
        let count = self.read.nr as usize;
        let values_end = values_offset + count.saturating_mul(std::mem::size_of::<EventValue>());

        if record.len() < values_end {
            return SmallVec::new();
        }

        unsafe {
            std::slice::from_raw_parts(
                record.as_ptr().add(values_offset) as *const EventValue,
                count,
            )
            .to_smallvec()
        }
    }

    fn read_callchain(&self, record: &[u8]) -> SmallVec<[u64; 8]> {
        let values_offset = std::mem::size_of::<SampleFormat>();
        let values_size = self.read.nr as usize * std::mem::size_of::<EventValue>();
        let base_offset = values_offset + values_size;

        if record.len() < base_offset + std::mem::size_of::<u64>() {
            return SmallVec::new();
        }

        let mut len_bytes = [0u8; std::mem::size_of::<u64>()];
        len_bytes.copy_from_slice(&record[base_offset..base_offset + std::mem::size_of::<u64>()]);
        let nr_callchain = u64::from_ne_bytes(len_bytes) as usize;

        let callchain_offset = base_offset + std::mem::size_of::<u64>();
        let callchain_end =
            callchain_offset + nr_callchain.saturating_mul(std::mem::size_of::<u64>());

        if record.len() < callchain_end {
            return SmallVec::new();
        }

        let slice = unsafe {
            std::slice::from_raw_parts(
                record.as_ptr().add(callchain_offset) as *const u64,
                nr_callchain,
            )
        };

        if slice.len() > 1 {
            slice[1..].to_smallvec()
        } else {
            slice.to_smallvec()
        }
    }

    fn callchain_end(&self, record: &[u8]) -> Option<usize> {
        let base = std::mem::size_of::<SampleFormat>()
            .checked_add((self.read.nr as usize).checked_mul(std::mem::size_of::<EventValue>())?)?;
        let nr = read_u64(record, base)? as usize;
        base.checked_add(std::mem::size_of::<u64>())?
            .checked_add(nr.checked_mul(std::mem::size_of::<u64>())?)
            .filter(|end| *end <= record.len())
    }

    fn branch_stack_end(&self, record: &[u8]) -> Option<usize> {
        let base = self.callchain_end(record)?;
        let nr = read_u64(record, base)? as usize;
        base.checked_add(8)?
            .checked_add(nr.checked_mul(std::mem::size_of::<BranchEntry>())?)
            .filter(|end| *end <= record.len())
    }

    fn read_branch_callstack(&self, record: &[u8], mode: BranchMode) -> SmallVec<[u64; 8]> {
        let Some(base) = self.callchain_end(record) else {
            return SmallVec::new();
        };
        let Some(end) = self.branch_stack_end(record) else {
            return SmallVec::new();
        };
        mode.frames(self.ip, branch_records(record, base + 8, end))
    }

    fn read_user_state(
        &self,
        record: &[u8],
        mask: u64,
        sample_branch_stack: bool,
    ) -> (Option<UserRegs>, Vec<u8>) {
        if mask == 0 {
            return (None, Vec::new());
        }

        let Some(mut offset) = (if sample_branch_stack {
            self.branch_stack_end(record)
        } else {
            self.callchain_end(record)
        }) else {
            return (None, Vec::new());
        };
        let Some(abi) = read_u64(record, offset) else {
            return (None, Vec::new());
        };
        offset += std::mem::size_of::<u64>();

        let value_count = if abi == 0 {
            0
        } else {
            mask.count_ones() as usize
        };
        let Some(values_end) = offset.checked_add(value_count.saturating_mul(8)) else {
            return (None, Vec::new());
        };
        if values_end > record.len() {
            return (None, Vec::new());
        }
        let values = (0..value_count)
            .map(|index| read_u64(record, offset + index * 8).unwrap_or_default())
            .collect();
        offset = values_end;

        let regs = (abi != 0).then_some(UserRegs { abi, mask, values });
        let Some(stack_size) = read_u64(record, offset).map(|value| value as usize) else {
            return (regs, Vec::new());
        };
        offset += 8;
        let Some(stack_end) = offset.checked_add(stack_size) else {
            return (regs, Vec::new());
        };
        if stack_end > record.len() {
            return (regs, Vec::new());
        }
        let stack = record[offset..stack_end].to_vec();
        // PERF_SAMPLE_STACK_USER appends dyn_size after the requested-size byte array.
        let dynamic_size = read_u64(record, stack_end).unwrap_or(stack_size as u64) as usize;
        let dynamic_size = dynamic_size.min(stack.len());
        (regs, stack[..dynamic_size].to_vec())
    }
}

/// Parser for the precise-memory sample layout: `PERF_SAMPLE_IP | TID | TIME |
/// ADDR | ID | CPU | PERIOD | CALLCHAIN | BRANCH_STACK | REGS_USER |
/// STACK_USER | WEIGHT_STRUCT | DATA_SRC`, in the field order `perf_event.h`
/// documents.
struct MemSampleFormat;

impl MemSampleFormat {
    fn parse(record: &[u8], regs_mask: u64, branch_mode: Option<BranchMode>) -> Option<MmapRecord> {
        let mut offset = std::mem::size_of::<perf_event_header>();
        let ip = read_u64(record, offset)?;
        offset += 8;
        let ids = read_u64(record, offset)?;
        offset += 8;
        let (pid, tid) = (ids as u32, (ids >> 32) as u32);
        let time = read_u64(record, offset)?;
        offset += 8;
        let data_addr = read_u64(record, offset)?;
        offset += 16; // addr, then id
        let cpu = read_u64(record, offset)? as u32;
        offset += 16; // cpu + reserved, then period

        let nr = read_u64(record, offset)? as usize;
        offset += 8;
        let end = offset.checked_add(nr.checked_mul(8)?)?;
        if end > record.len() {
            return None;
        }
        let mut callstack = SmallVec::with_capacity(nr);
        for index in 0..nr {
            callstack.push(read_u64(record, offset + index * 8)?);
        }
        // The kernel prefixes the callchain with a context marker.
        if callstack.len() > 1 {
            callstack.remove(0);
        }
        offset = end;

        let mut lbr_callstack = SmallVec::new();
        if let Some(mode) = branch_mode {
            let entries = read_u64(record, offset)? as usize;
            let entries_offset = offset + 8;
            let entries_end = entries_offset
                .checked_add(entries.checked_mul(std::mem::size_of::<BranchEntry>())?)?;
            if entries_end > record.len() {
                return None;
            }
            lbr_callstack = mode.frames(ip, branch_records(record, entries_offset, entries_end));
            offset = entries_end;
        }

        let mut user_regs = None;
        if regs_mask != 0 {
            let abi = read_u64(record, offset)?;
            offset += 8;
            let count = if abi == 0 {
                0
            } else {
                regs_mask.count_ones() as usize
            };
            let values_end = offset.checked_add(count.checked_mul(8)?)?;
            if values_end > record.len() {
                return None;
            }
            let values = (0..count)
                .map(|index| read_u64(record, offset + index * 8).unwrap_or_default())
                .collect();
            offset = values_end;
            if abi != 0 {
                user_regs = Some(UserRegs {
                    abi,
                    mask: regs_mask,
                    values,
                });
            }

            let size = read_u64(record, offset)? as usize;
            offset += 8;
            if size != 0 {
                let stack_end = offset.checked_add(size)?;
                if stack_end > record.len() {
                    return None;
                }
                let dynamic_size = (read_u64(record, stack_end)? as usize).min(size);
                let stack = record[offset..offset + dynamic_size].to_vec();
                offset = stack_end + 8;
                return Some(Self::finish(
                    MemFields {
                        ip,
                        pid,
                        tid,
                        cpu,
                        time,
                        data_addr,
                        callstack,
                        lbr_callstack,
                        user_regs,
                        user_stack: stack,
                    },
                    record,
                    offset,
                ));
            }
        }

        Some(Self::finish(
            MemFields {
                ip,
                pid,
                tid,
                cpu,
                time,
                data_addr,
                callstack,
                lbr_callstack,
                user_regs,
                user_stack: Vec::new(),
            },
            record,
            offset,
        ))
    }

    fn finish(fields: MemFields, record: &[u8], offset: usize) -> MmapRecord {
        let MemFields {
            ip,
            pid,
            tid,
            cpu,
            time,
            data_addr,
            callstack,
            lbr_callstack,
            user_regs,
            user_stack,
        } = fields;
        // `union perf_sample_weight` keeps the access latency in its low 32
        // bits under both the legacy `WEIGHT` and the 5.12 `WEIGHT_STRUCT`
        // layouts, so one decode serves both.
        let latency = read_u64(record, offset).unwrap_or_default() & 0xffff_ffff;
        let data_src = read_u64(record, offset + 8).unwrap_or_default();
        MmapRecord::MemSample {
            ip,
            pid,
            tid,
            cpu,
            time,
            data_addr,
            latency,
            data_src,
            callstack,
            lbr_callstack,
            user_regs,
            user_stack,
        }
    }
}

/// Everything a precise-memory sample carries before its weight and data
/// source, which the parser reads last.
struct MemFields {
    ip: u64,
    pid: u32,
    tid: u32,
    cpu: u32,
    time: u64,
    data_addr: u64,
    callstack: SmallVec<[u64; 8]>,
    lbr_callstack: SmallVec<[u64; 8]>,
    user_regs: Option<UserRegs>,
    user_stack: Vec<u8>,
}

/// Iterate the `perf_branch_entry` array between two record offsets.
fn branch_records(
    record: &[u8],
    start: usize,
    end: usize,
) -> impl Iterator<Item = BranchRecord> + '_ {
    (start..end)
        .step_by(std::mem::size_of::<BranchEntry>())
        .map(|offset| BranchRecord {
            from: read_u64(record, offset).unwrap_or_default(),
            flags: read_u64(record, offset + 16).unwrap_or_default(),
        })
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let end = offset.checked_add(8)?;
    let value: [u8; 8] = bytes.get(offset..end)?.try_into().ok()?;
    Some(u64::from_ne_bytes(value))
}

impl ProcMmap {
    fn read_from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < std::mem::size_of::<Self>() {
            return None;
        }

        Some(unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const Self) })
    }

    fn filename(bytes: &[u8]) -> String {
        let start = std::mem::size_of::<Self>();
        if bytes.len() <= start {
            return String::new();
        }

        match CStr::from_bytes_until_nul(&bytes[start..]) {
            Ok(cstr) => cstr.to_string_lossy().into_owned(),
            Err(_) => String::new(),
        }
    }
}

impl LostRecord {
    fn read_from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < std::mem::size_of::<Self>() {
            return None;
        }
        Some(unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const Self) })
    }
}
