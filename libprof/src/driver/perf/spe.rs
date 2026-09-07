use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use libc::{close, mmap, munmap, sysconf, MAP_FAILED, MAP_SHARED, PROT_READ, PROT_WRITE};
use perf_event_open_sys as sys;
use perf_event_open_sys::bindings::{perf_event_attr, perf_event_header, perf_event_mmap_page};
use smallvec::SmallVec;

use crate::driver::{SamplingDriver, Sink};
use crate::sink::{MemSample, Record};
use crate::{Counter, Error};

use super::sysfs;
use super::UnsafeMmap;

const SYSFS_ROOT: &str = "/sys/bus/event_source/devices";
/// Data ring only carries `PERF_RECORD_AUX` notifications; keep it small.
const DATA_PAGES: usize = 8;
const AUX_PAGES: usize = 256;
/// Micro-ops between samples; raised to the PMU's `caps/min_interval` when
/// the hardware demands a longer one.
const DEFAULT_SAMPLE_PERIOD: u64 = 4096;

const PERF_RECORD_AUX: u32 = 11;
const PERF_AUX_FLAG_TRUNCATED: u64 = 0x01;

/// The `arm_spe_*` sysfs PMU directory, when the host exposes one.
pub fn spe_pmu_path() -> Option<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(SYSFS_ROOT)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("arm_spe"))
        })
        .collect();
    paths.sort();
    paths.into_iter().next()
}

/// Precise memory sampling through the Arm Statistical Profiling Extension.
/// Opens the `arm_spe_*` AUX event on every SPE-capable CPU (the PMU's
/// `cpumask`; little cores without the extension simply contribute no
/// samples), decodes the profiling packet stream and reports each sampled
/// load/store as a [`MemSample`].
pub struct PerfSpeSamplingDriver {
    channels: Vec<SpeChannel>,
    page_size: usize,
    running: Arc<AtomicBool>,
    lost_samples: Arc<AtomicU64>,
    bytes_drained: Arc<AtomicU64>,
    thread_handle: Option<thread::JoinHandle<()>>,
    pid: u32,
}

struct SpeChannel {
    fd: i32,
    cpu: u32,
    base: UnsafeMmap,
    aux: UnsafeMmap,
    aux_len: usize,
}

unsafe impl Send for PerfSpeSamplingDriver {}
unsafe impl Sync for PerfSpeSamplingDriver {}

impl PerfSpeSamplingDriver {
    pub fn new(pid: i32) -> Result<PerfSpeSamplingDriver, Error> {
        let counter = Counter::Custom("arm_spe".to_owned());
        let pmu = spe_pmu_path().ok_or_else(|| {
            Error::InvalidConfiguration("no arm_spe_* PMU exposed in sysfs".to_owned())
        })?;
        let type_id = read_number(&pmu.join("type")).ok_or_else(|| {
            Error::InvalidConfiguration("arm_spe PMU advertises no type id".to_owned())
        })? as u32;
        let cpus = read_cpumask(&pmu.join("cpumask")).ok_or_else(|| {
            Error::InvalidConfiguration("arm_spe PMU advertises no cpumask".to_owned())
        })?;
        let min_interval = read_number(&pmu.join("caps/min_interval")).unwrap_or(1024);

        let mut attr = perf_event_attr::default();
        attr.size = std::mem::size_of::<perf_event_attr>() as u32;
        attr.type_ = type_id;
        attr.sample_period = DEFAULT_SAMPLE_PERIOD.max(min_interval);
        attr.set_disabled(0);
        attr.set_exclude_kernel(1);
        attr.set_exclude_hv(1);
        attr.aux_watermark = 4096;
        for (field, value) in [
            ("ts_enable", 1),
            ("load_filter", 1),
            ("store_filter", 1),
            ("jitter", 1),
            ("min_latency", 0),
        ] {
            sysfs::set_format_field(&mut attr, &pmu, field, value);
        }

        let page_size = unsafe { sysconf(libc::_SC_PAGE_SIZE) } as usize;
        let mut channels: Vec<SpeChannel> = Vec::new();
        let cleanup = |channels: &[SpeChannel], page_size: usize| {
            for channel in channels {
                unsafe {
                    munmap(channel.aux.ptr.cast(), channel.aux_len);
                    munmap(channel.base.ptr.cast(), page_size * (DATA_PAGES + 1));
                    close(channel.fd);
                }
            }
        };

        for &cpu in &cpus {
            // Threads spawned after the open should stay sampled; some kernels
            // reject inheritance on AUX events, so retry without it.
            attr.set_inherit(1);
            let mut fd = unsafe { sys::perf_event_open(&mut attr, pid, cpu, -1, 0) };
            if fd < 0 {
                attr.set_inherit(0);
                fd = unsafe { sys::perf_event_open(&mut attr, pid, cpu, -1, 0) };
            }
            if fd < 0 {
                let error = Error::perf_event_open(&counter, Some(cpu));
                cleanup(&channels, page_size);
                return Err(error);
            }

            let base_len = page_size * (DATA_PAGES + 1);
            let base = unsafe {
                mmap(
                    std::ptr::null_mut(),
                    base_len,
                    PROT_READ | PROT_WRITE,
                    MAP_SHARED,
                    fd,
                    0,
                ) as *mut u8
            };
            if base as *mut libc::c_void == MAP_FAILED {
                let source = std::io::Error::last_os_error();
                unsafe { close(fd) };
                cleanup(&channels, page_size);
                return Err(Error::PerfMmap {
                    counter: "arm_spe".to_owned(),
                    length: base_len,
                    source,
                });
            }

            let aux_len = page_size * AUX_PAGES;
            let metadata = base as *mut perf_event_mmap_page;
            let aux = unsafe {
                (*metadata).aux_offset = base_len as u64;
                (*metadata).aux_size = aux_len as u64;
                mmap(
                    std::ptr::null_mut(),
                    aux_len,
                    PROT_READ | PROT_WRITE,
                    MAP_SHARED,
                    fd,
                    base_len as i64,
                ) as *mut u8
            };
            if aux as *mut libc::c_void == MAP_FAILED {
                let source = std::io::Error::last_os_error();
                unsafe {
                    munmap(base.cast(), base_len);
                    close(fd);
                }
                cleanup(&channels, page_size);
                return Err(Error::PerfMmap {
                    counter: "arm_spe (aux area)".to_owned(),
                    length: aux_len,
                    source,
                });
            }

            channels.push(SpeChannel {
                fd,
                cpu: cpu as u32,
                base: UnsafeMmap { ptr: base },
                aux: UnsafeMmap { ptr: aux },
                aux_len,
            });
        }

        if channels.is_empty() {
            return Err(Error::InvalidConfiguration(
                "arm_spe PMU exposes an empty cpumask".to_owned(),
            ));
        }

        Ok(PerfSpeSamplingDriver {
            channels,
            page_size,
            running: Arc::new(AtomicBool::new(false)),
            lost_samples: Arc::new(AtomicU64::new(0)),
            bytes_drained: Arc::new(AtomicU64::new(0)),
            thread_handle: None,
            pid: pid as u32,
        })
    }
}

impl SamplingDriver for PerfSpeSamplingDriver {
    fn counters(&self) -> Vec<Counter> {
        vec![Counter::Custom("arm_spe".to_owned())]
    }

    fn start(&mut self, callback: Arc<dyn Sink>) -> Result<(), Error> {
        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let lost_samples = self.lost_samples.clone();
        let bytes_drained = self.bytes_drained.clone();
        let pid = self.pid;
        let clock = TimerCalibration::new();
        let mut streams: Vec<SpeStream> = self
            .channels
            .iter()
            .map(|channel| SpeStream {
                base: channel.base.ptr,
                aux: channel.aux.ptr,
                aux_len: channel.aux_len,
                cpu: channel.cpu,
                carry: Vec::new(),
                carry_offset: 0,
                decoder: SpeRecord::default(),
                event_histogram: std::collections::BTreeMap::new(),
                debug: std::env::var_os("MPERF_SPE_DEBUG").is_some(),
            })
            .collect();

        self.thread_handle = Some(thread::spawn(move || loop {
            for stream in &mut streams {
                stream.drain(&lost_samples, &bytes_drained);
                stream.decode(pid, &clock, callback.as_ref());
            }
            if !running.load(Ordering::SeqCst) {
                if std::env::var_os("MPERF_SPE_DEBUG").is_some() {
                    for stream in &streams {
                        if !stream.event_histogram.is_empty() {
                            eprintln!(
                                "arm_spe cpu{} events histogram: {:?}",
                                stream.cpu, stream.event_histogram
                            );
                        }
                    }
                }
                break;
            }
            thread::sleep(Duration::from_micros(500));
        }));

        Ok(())
    }

    fn stop(&mut self) -> Result<(), Error> {
        for channel in &self.channels {
            unsafe { sys::ioctls::DISABLE(channel.fd, 0) };
        }
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.thread_handle.take() {
            handle.join().map_err(|_| Error::WorkerPanicked)?;
        }
        if std::env::var_os("MPERF_SPE_DEBUG").is_some() {
            eprintln!(
                "arm_spe: drained {} AUX bytes across {} CPUs",
                self.bytes_drained.load(Ordering::Relaxed),
                self.channels.len()
            );
        }
        let lost = self.lost_samples.load(Ordering::Relaxed);
        if lost != 0 {
            return Err(Error::SamplesLost { count: lost });
        }
        // On big.LITTLE only the SPE-capable cores produce samples; a short
        // task the scheduler kept on little cores yields nothing, silently.
        if self.bytes_drained.load(Ordering::Relaxed) == 0 {
            return Err(Error::InvalidConfiguration(
                "no SPE data was captured — the workload likely never ran on an SPE-capable \
                 core; pin it to the arm_spe cpumask (see \
                 /sys/bus/event_source/devices/arm_spe_0/cpumask)"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

impl Drop for PerfSpeSamplingDriver {
    fn drop(&mut self) {
        for channel in &self.channels {
            unsafe {
                munmap(channel.aux.ptr.cast(), channel.aux_len);
                munmap(channel.base.ptr.cast(), self.page_size * (DATA_PAGES + 1));
                close(channel.fd);
            }
        }
    }
}

/// One CPU's AUX byte stream plus the decode state that survives across
/// drains: packets and records may straddle an AUX watermark flush.
struct SpeStream {
    base: *mut u8,
    aux: *mut u8,
    aux_len: usize,
    cpu: u32,
    carry: Vec<u8>,
    /// Absolute AUX-buffer offset of `carry[0]`; alignment packets pad
    /// relative to this position, not to our copy's address.
    carry_offset: u64,
    decoder: SpeRecord,
    event_histogram: std::collections::BTreeMap<u64, u64>,
    debug: bool,
}

unsafe impl Send for SpeStream {}

impl SpeStream {
    fn drain(&mut self, lost_samples: &AtomicU64, bytes_drained: &AtomicU64) {
        let metadata = self.base as *mut perf_event_mmap_page;
        unsafe {
            let head_atomic = AtomicU64::from_ptr(&mut (*metadata).aux_head as *mut u64);
            let tail_atomic = AtomicU64::from_ptr(&mut (*metadata).aux_tail as *mut u64);
            let head = head_atomic.load(Ordering::Acquire);
            let tail = tail_atomic.load(Ordering::Relaxed);
            if head > tail {
                let length = (head - tail) as usize;
                bytes_drained.fetch_add(length as u64, Ordering::Relaxed);
                let start = self.carry.len();
                self.carry.resize(start + length, 0);
                let mut copied = 0usize;
                while copied < length {
                    let ring_pos = ((tail + copied as u64) % self.aux_len as u64) as usize;
                    let chunk = (length - copied).min(self.aux_len - ring_pos);
                    std::ptr::copy_nonoverlapping(
                        self.aux.add(ring_pos),
                        self.carry.as_mut_ptr().add(start + copied),
                        chunk,
                    );
                    copied += chunk;
                }
                tail_atomic.store(head, Ordering::Release);
            }
            self.consume_data_ring(lost_samples);
        }
    }

    /// The data ring only signals AUX activity; scan it for truncation flags
    /// and release it so the kernel never sees it as full.
    unsafe fn consume_data_ring(&mut self, lost_samples: &AtomicU64) {
        let metadata = self.base as *mut perf_event_mmap_page;
        let head_atomic = AtomicU64::from_ptr(&mut (*metadata).data_head as *mut u64);
        let tail_atomic = AtomicU64::from_ptr(&mut (*metadata).data_tail as *mut u64);
        let head = head_atomic.load(Ordering::Acquire);
        let mut tail = tail_atomic.load(Ordering::Relaxed);
        let data_offset = (*metadata).data_offset as usize;
        let data_size = (*metadata).data_size as usize;
        let base = (self.base as *const u8).add(data_offset);
        while tail + std::mem::size_of::<perf_event_header>() as u64 <= head {
            let at = |offset: u64, len: usize| -> Vec<u8> {
                let mut bytes = vec![0u8; len];
                for (index, byte) in bytes.iter_mut().enumerate() {
                    *byte = *base.add(((offset + index as u64) % data_size as u64) as usize);
                }
                bytes
            };
            let header = at(tail, std::mem::size_of::<perf_event_header>());
            let type_ = u32::from_ne_bytes(header[0..4].try_into().unwrap());
            let size = u16::from_ne_bytes(header[6..8].try_into().unwrap()) as u64;
            if size == 0 || tail + size > head {
                break;
            }
            if type_ == PERF_RECORD_AUX && size as usize >= 8 + 24 {
                let body = at(tail + 8, 24);
                let flags = u64::from_ne_bytes(body[16..24].try_into().unwrap());
                if flags & PERF_AUX_FLAG_TRUNCATED != 0 {
                    lost_samples.fetch_add(1, Ordering::Relaxed);
                }
            }
            tail += size;
        }
        tail_atomic.store(tail, Ordering::Release);
    }

    fn decode(&mut self, pid: u32, clock: &TimerCalibration, callback: &dyn Sink) {
        let mut cursor = 0usize;
        loop {
            let buf = &self.carry[cursor..];
            if buf.is_empty() {
                break;
            }
            let absolute = self.carry_offset + cursor as u64;
            let (packet, consumed) = next_packet(buf, absolute);
            if consumed == 0 {
                break;
            }
            cursor += consumed;
            if self.debug {
                if let SpePacket::Events(mask) = packet {
                    *self.event_histogram.entry(mask).or_insert(0) += 1;
                }
            }
            if let Some(sample) = self.decoder.apply(packet) {
                let time = if sample.timestamp != 0 {
                    clock.to_monotonic(sample.timestamp)
                } else {
                    monotonic_now()
                };
                callback.record(Record::MemSample(MemSample {
                    ip: sample.pc,
                    pid,
                    tid: pid,
                    cpu: self.cpu,
                    time,
                    data_addr: sample.data_addr,
                    latency: sample.latency,
                    data_src: sample.data_src(),
                    callstack: SmallVec::new(),
                    lbr_callstack: SmallVec::new(),
                    user_regs: None,
                    user_stack: Vec::new(),
                }));
            }
        }
        self.carry.drain(..cursor);
        self.carry_offset += cursor as u64;
    }
}

const EV_L1D_ACCESS: u64 = 1 << 2;
const EV_L1D_REFILL: u64 = 1 << 3;
const EV_TLB_ACCESS: u64 = 1 << 4;
const EV_TLB_WALK: u64 = 1 << 5;
const EV_LLC_ACCESS: u64 = 1 << 8;
const EV_LLC_MISS: u64 = 1 << 9;
const EV_REMOTE_ACCESS: u64 = 1 << 10;

/// One profiling record accumulated across its packets; a record ends with an
/// END or TIMESTAMP packet.
#[derive(Default)]
struct SpeRecord {
    pc: u64,
    data_addr: u64,
    has_data_addr: bool,
    latency: u64,
    events: u64,
    is_load_store: bool,
    is_store: bool,
    timestamp: u64,
}

impl SpeRecord {
    /// Feed one packet; returns the finished record on END/TIMESTAMP when it
    /// described a sampled memory access.
    fn apply(&mut self, packet: SpePacket) -> Option<SpeRecord> {
        match packet {
            SpePacket::Address { index: 0, payload } => self.pc = instruction_address(payload),
            SpePacket::Address { index: 2, payload } => {
                self.data_addr = data_address(payload);
                self.has_data_addr = true;
            }
            SpePacket::Address { .. } => {}
            SpePacket::Counter { index: 0, payload } => self.latency = payload,
            SpePacket::Counter { .. } => {}
            SpePacket::Events(payload) => self.events = payload,
            SpePacket::OpType { class, payload } => {
                self.is_load_store = class == 1;
                self.is_store = class == 1 && payload & 0x1 != 0;
            }
            SpePacket::Timestamp(payload) => {
                self.timestamp = payload;
                return self.finish();
            }
            SpePacket::End => return self.finish(),
            SpePacket::Pad | SpePacket::Context | SpePacket::DataSource | SpePacket::Bad => {}
        }
        None
    }

    fn finish(&mut self) -> Option<SpeRecord> {
        let record = std::mem::take(self);
        (record.is_load_store && record.has_data_addr).then_some(record)
    }

    /// Synthesize the `perf_event.h` `PERF_SAMPLE_DATA_SRC` union from the
    /// record's event bits, the same mapping `tools/perf/util/arm-spe.c` uses
    /// for cores without a model-specific data-source table.
    fn data_src(&self) -> u64 {
        const LVL_SHIFT: u32 = 5;
        const TLB_SHIFT: u32 = 26;
        const REMOTE_SHIFT: u32 = 37;
        let op: u64 = if self.is_store { 0x04 } else { 0x02 };

        let lvl: u64 = if self.events & EV_LLC_MISS != 0 {
            0x04 | 0x80 // miss, satisfied from local RAM
        } else if self.events & EV_LLC_ACCESS != 0 && self.events & EV_L1D_REFILL != 0 {
            0x02 | 0x40 // L1 refill answered by the last-level cache
        } else if self.events & EV_L1D_REFILL != 0 {
            0x04 | 0x08 // L1 miss with no LLC information
        } else if self.events & EV_L1D_ACCESS != 0 {
            0x02 | 0x08 // L1 hit
        } else {
            0
        };

        let tlb: u64 = if self.events & EV_TLB_WALK != 0 {
            0x04 | 0x20
        } else if self.events & EV_TLB_ACCESS != 0 {
            0x02 | 0x08
        } else {
            0
        };

        let remote: u64 = u64::from(self.events & EV_REMOTE_ACCESS != 0);
        op | (lvl << LVL_SHIFT) | (tlb << TLB_SHIFT) | (remote << REMOTE_SHIFT)
    }
}

/// Bits [55:0] carry the address; the top byte is NS/EL for instruction
/// addresses and a tag byte for data addresses (Arm ARM D10.2.1).
fn instruction_address(payload: u64) -> u64 {
    let ns = payload >> 63 & 1;
    let el = payload >> 61 & 0b11;
    let address = payload & ((1 << 56) - 1);
    if ns != 0 && (el == 1 || el == 2) {
        address | 0xff << 56
    } else {
        address
    }
}

fn data_address(payload: u64) -> u64 {
    let address = payload & ((1 << 56) - 1);
    if (address >> 48) & 0xf0 == 0xf0 {
        address | 0xff << 56
    } else {
        address
    }
}

enum SpePacket {
    Pad,
    End,
    Timestamp(u64),
    Events(u64),
    DataSource,
    Context,
    OpType { class: u8, payload: u64 },
    Address { index: u8, payload: u64 },
    Counter { index: u8, payload: u64 },
    Bad,
}

/// Decode one packet from `buf`, whose first byte sits at absolute AUX offset
/// `absolute`. Returns the packet and consumed length; zero length means the
/// packet is still incomplete and decoding must wait for more bytes.
fn next_packet(buf: &[u8], absolute: u64) -> (SpePacket, usize) {
    let header = buf[0];
    let payload_len = |hdr: u8| 1usize << ((hdr >> 4) & 0b11);
    let payload = |skip: usize, len: usize| -> Option<u64> {
        let bytes = buf.get(skip..skip + len)?;
        let mut value = 0u64;
        for (index, byte) in bytes.iter().enumerate() {
            value |= (*byte as u64) << (8 * index);
        }
        Some(value)
    };
    let short = |packet: fn(u64) -> SpePacket| -> (SpePacket, usize) {
        let len = payload_len(header);
        match payload(1, len) {
            Some(value) => (packet(value), 1 + len),
            None => (SpePacket::Bad, 0),
        }
    };

    match header {
        0x00 => return (SpePacket::Pad, 1),
        0x01 => return (SpePacket::End, 1),
        0x71 => return short(SpePacket::Timestamp),
        _ => {}
    }
    match header & 0xcf {
        0x42 => return short(SpePacket::Events),
        0x43 => return short(|_| SpePacket::DataSource),
        _ => {}
    }
    if header & 0xfc == 0x64 {
        let len = payload_len(header);
        return match payload(1, len) {
            Some(_) => (SpePacket::Context, 1 + len),
            None => (SpePacket::Bad, 0),
        };
    }
    if header & 0xfc == 0x48 {
        let len = payload_len(header);
        return match payload(1, len) {
            Some(value) => (
                SpePacket::OpType {
                    class: header & 0b11,
                    payload: value,
                },
                1 + len,
            ),
            None => (SpePacket::Bad, 0),
        };
    }

    let (effective, index, skip) = if header & 0xfc == 0x20 {
        let Some(&header1) = buf.get(1) else {
            return (SpePacket::Bad, 0);
        };
        if header1 == 0x00 {
            // Alignment packet: pad so the next packet starts on a
            // 2^(n+1)-byte boundary of the AUX buffer.
            let alignment = 1u64 << ((header & 0x0f) + 1);
            let consumed = (alignment - (absolute & (alignment - 1))) as usize;
            if buf.len() < consumed {
                return (SpePacket::Bad, 0);
            }
            return (SpePacket::Pad, consumed);
        }
        (header1, (header & 0b11) << 3 | (header1 & 0b111), 2usize)
    } else {
        (header, header & 0b111, 1usize)
    };

    let len = payload_len(effective);
    let kind = effective & 0xf8;
    if kind == 0xb0 || kind == 0x98 {
        return match payload(skip, len) {
            Some(value) => {
                let packet = if kind == 0xb0 {
                    SpePacket::Address {
                        index,
                        payload: value,
                    }
                } else {
                    SpePacket::Counter {
                        index,
                        payload: value,
                    }
                };
                (packet, skip + len)
            }
            None => (SpePacket::Bad, 0),
        };
    }

    // Unknown header: resynchronize one byte at a time.
    (SpePacket::Bad, 1)
}

/// SPE timestamps count the generic timer (`CNTVCT_EL0`). `CLOCK_MONOTONIC`
/// ticks from the same counter on arm64, so one paired reading plus the
/// counter frequency converts exactly (modulo NTP slew, ppm over a run).
struct TimerCalibration {
    counter_origin: u64,
    monotonic_origin: u64,
    frequency: u64,
}

impl TimerCalibration {
    fn new() -> TimerCalibration {
        TimerCalibration {
            counter_origin: generic_timer_count(),
            monotonic_origin: monotonic_now(),
            frequency: generic_timer_frequency().max(1),
        }
    }

    fn to_monotonic(&self, timestamp: u64) -> u64 {
        let delta = timestamp.wrapping_sub(self.counter_origin) as i64;
        let nanoseconds = (delta as i128 * 1_000_000_000) / self.frequency as i128;
        (self.monotonic_origin as i128 + nanoseconds).max(0) as u64
    }
}

fn monotonic_now() -> u64 {
    let mut spec = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut spec) };
    spec.tv_sec as u64 * 1_000_000_000 + spec.tv_nsec as u64
}

#[cfg(target_arch = "aarch64")]
fn generic_timer_count() -> u64 {
    let value: u64;
    unsafe { std::arch::asm!("mrs {}, cntvct_el0", out(reg) value) };
    value
}

#[cfg(target_arch = "aarch64")]
fn generic_timer_frequency() -> u64 {
    let value: u64;
    unsafe { std::arch::asm!("mrs {}, cntfrq_el0", out(reg) value) };
    value
}

#[cfg(not(target_arch = "aarch64"))]
fn generic_timer_count() -> u64 {
    0
}

#[cfg(not(target_arch = "aarch64"))]
fn generic_timer_frequency() -> u64 {
    1
}

fn read_number(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

/// Parse a sysfs cpumask like `0-1,6-11` into CPU numbers.
fn read_cpumask(path: &Path) -> Option<Vec<i32>> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut cpus = Vec::new();
    for part in text.trim().split(',').filter(|part| !part.is_empty()) {
        match part.split_once('-') {
            Some((low, high)) => {
                let low = low.trim().parse::<i32>().ok()?;
                let high = high.trim().parse::<i32>().ok()?;
                cpus.extend(low..=high);
            }
            None => cpus.push(part.trim().parse().ok()?),
        }
    }
    Some(cpus)
}
