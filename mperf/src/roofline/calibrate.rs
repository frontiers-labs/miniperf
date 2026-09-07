use std::{
    hint::black_box,
    sync::Barrier,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use mperf_data::{
    MemoryBandwidthCalibration, MemoryLevelCalibration, RooflineCalibration, RooflineCeilings,
};
use rayon::prelude::*;

const SAMPLES: usize = 5;
const COMPUTE_ITERATIONS: usize = 20_000_000;
const MEMORY_ELEMENTS: usize = 8 * 1024 * 1024;
const MEMORY_REPETITIONS: usize = 8;
const MEMORY_CHUNK_ELEMENTS: usize = 16 * 1024;

pub(super) fn measure() -> Result<RooflineCalibration> {
    let (all, memory) = measure_ceilings()?;
    let single_thread = (all.threads > 1)
        .then(|| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .build()
                .context("build the single-thread calibration pool")?
                .install(measure_ceilings)
                .map(|(ceilings, _)| ceilings)
        })
        .transpose()?;
    Ok(RooflineCalibration {
        threads: all.threads,
        cpu_affinity: memory.cpu_affinity,
        samples: SAMPLES,
        compute_kernel: compute_kernel().name.to_string(),
        fp64_gflops: all.fp64_gflops,
        fp64_gflops_samples: all.fp64_gflops_samples,
        memory_gbytes_per_second: all.memory_gbytes_per_second,
        memory_gbytes_per_second_samples: all.memory_gbytes_per_second_samples,
        ridge_point_flops_per_byte: all.ridge_point_flops_per_byte,
        memory_working_set_bytes: memory.working_set_bytes,
        memory_levels: all.memory_levels,
        single_thread,
    })
}

/// Measures every ceiling on the current Rayon pool.
fn measure_ceilings() -> Result<(RooflineCeilings, MemoryBandwidthCalibration)> {
    let threads = rayon::current_num_threads();
    let kernel = compute_kernel();

    // Populate the Rayon pool and architecture state before timed samples.
    let _ = measure_compute_sample(kernel, threads, COMPUTE_ITERATIONS / 100);

    let compute_samples = (0..SAMPLES)
        .map(|_| measure_compute_sample(kernel, threads, COMPUTE_ITERATIONS))
        .collect::<Vec<_>>();

    let memory = measure_memory()?;

    let fp64_gflops = median(&compute_samples);
    if !fp64_gflops.is_finite() || fp64_gflops <= 0.0 {
        bail!("FP64 roof calibration produced an invalid result: {fp64_gflops}");
    }

    let memory_levels = measure_memory_levels(threads, &memory);

    let ceilings = RooflineCeilings {
        threads,
        fp64_gflops,
        fp64_gflops_samples: compute_samples,
        memory_gbytes_per_second: memory.gbytes_per_second,
        memory_gbytes_per_second_samples: memory.gbytes_per_second_samples.clone(),
        ridge_point_flops_per_byte: fp64_gflops / memory.gbytes_per_second,
        memory_levels,
    };
    Ok((ceilings, memory))
}

/// Per-thread fraction of a cache level the triad's three buffers are sized to
/// occupy. Staying well under capacity keeps the kernel resident once the
/// buffers, stack and Rayon metadata share the level.
const LEVEL_OCCUPANCY: f64 = 0.5;

/// A level's working set must be larger than the cache immediately inside it,
/// otherwise (for example) a nominal L3 run can remain resident in the sum of
/// the private L2 caches. Sequential traversal needs only a modest excess to
/// make every reuse distance larger than the inner cache.
const INNER_LEVEL_SPILL_FACTOR: f64 = 1.1;

/// Leave room for code, stacks, allocator metadata and imperfect associativity
/// in the cache being measured. If the inner caches are too large to satisfy
/// this bound, that hierarchy level cannot be isolated reliably and is skipped.
const MAX_LEVEL_OCCUPANCY: f64 = 0.85;

/// Measures a bandwidth roof per cache level plus the already-measured DRAM
/// roof. Levels whose geometry cannot be detected, or that are too small to
/// hold three usefully sized buffers, are skipped rather than guessed at.
fn measure_memory_levels(
    threads: usize,
    dram: &MemoryBandwidthCalibration,
) -> Vec<MemoryLevelCalibration> {
    let mut levels = Vec::new();
    let cache_levels = detect_cache_levels();
    for index in 0..cache_levels.len() {
        let (level, capacity, sharers) = cache_levels[index];
        let Some(elements) = level_elements(&cache_levels, index, threads) else {
            continue;
        };
        // Below a few hundred elements the loop is dominated by parallel
        // dispatch rather than by the level's bandwidth.
        if elements < 512 {
            continue;
        }
        let Some((gbytes_per_second, samples)) = measure_level_bandwidth(threads, elements) else {
            continue;
        };
        levels.push(MemoryLevelCalibration {
            level: format!("L{level}"),
            gbytes_per_second,
            gbytes_per_second_samples: samples,
            working_set_bytes: (elements * threads.max(1) * 3 * size_of::<f64>()) as u64,
            capacity_bytes: capacity,
            shared_by: sharers,
        });
    }
    levels.push(MemoryLevelCalibration {
        level: "DRAM".to_string(),
        gbytes_per_second: dram.gbytes_per_second,
        gbytes_per_second_samples: dram.gbytes_per_second_samples.clone(),
        working_set_bytes: dram.working_set_bytes,
        capacity_bytes: 0,
        shared_by: threads,
    });
    // Bandwidth must not increase as the working set grows; if the host is
    // noisy enough to invert two levels the roofs would be misleading.
    levels.dedup_by(|later, earlier| later.gbytes_per_second >= earlier.gbytes_per_second);
    levels
}

/// Chooses an equal per-worker allocation that fits in `levels[index]` but
/// exceeds every inner cache after accounting for SMT/cache sharing.
fn level_elements(levels: &[(u32, u64, usize)], index: usize, threads: usize) -> Option<usize> {
    let (level, capacity, sharers) = *levels.get(index)?;
    let threads = threads.max(1);
    let contenders = sharers.clamp(1, threads) as f64;
    let target_bytes = capacity as f64 * LEVEL_OCCUPANCY / contenders;
    let inner_bytes = levels[..index]
        .iter()
        .filter(|(inner_level, _, _)| *inner_level < level)
        .map(|(_, inner_capacity, inner_sharers)| {
            *inner_capacity as f64 / (*inner_sharers).clamp(1, threads) as f64
        })
        .fold(0.0_f64, f64::max);
    let per_thread_bytes = target_bytes.max(inner_bytes * INNER_LEVEL_SPILL_FACTOR);

    if per_thread_bytes * contenders > capacity as f64 * MAX_LEVEL_OCCUPANCY {
        return None;
    }

    Some((per_thread_bytes / (3.0 * size_of::<f64>() as f64)) as usize)
}

/// Runs the triad entirely within per-thread buffers so the traffic is served
/// by the level those buffers fit in.
fn measure_level_bandwidth(threads: usize, elements: usize) -> Option<(f64, Vec<f64>)> {
    // Enough repetitions that a resident kernel runs for a measurable time.
    let repetitions = (MEMORY_ELEMENTS * MEMORY_REPETITIONS / elements.max(1)).clamp(64, 1 << 20);
    let affinity = affinity_cpus();
    let run = || -> f64 {
        let ready = Barrier::new(threads);
        let finished = Barrier::new(threads);
        rayon::broadcast(|context| {
            // Keep each private working set on the same logical CPU for
            // the whole sample. This makes the cache-sharing arithmetic
            // above describe the workers that actually execute it.
            let _affinity_guard = affinity.as_ref().and_then(|cpus| {
                cpus.get(context.index() % cpus.len())
                    .and_then(|cpu| pin_current_thread(*cpu))
            });
            let a = vec![1.0_f64; elements];
            let b = vec![2.0_f64; elements];
            let mut c = vec![0.0_f64; elements];
            let kernel = |scale: f64, c: &mut [f64]| triad(&a, &b, c, scale);
            // Warm the buffers into the level under test.
            kernel(1.0, &mut c);
            ready.wait();
            let start = Instant::now();
            for repetition in 0..repetitions {
                // Vary the scalar so the loop cannot be hoisted.
                kernel(1.0 + repetition as f64 * 1.0e-12, &mut c);
                black_box(&c[..]);
            }
            // Include imbalance between workers in the aggregate result.
            finished.wait();
            let elapsed = nonzero_elapsed(start.elapsed());
            let bytes = repetitions as f64 * elements as f64 * 3.0 * size_of::<f64>() as f64;
            bytes / elapsed.as_secs_f64() / 1.0e9
        })
        .into_iter()
        .sum::<f64>()
    };
    let _ = run();
    let samples = (0..SAMPLES).map(|_| run()).collect::<Vec<_>>();
    let median = median(&samples);
    (median.is_finite() && median > 0.0).then_some((median, samples))
}

/// Bandwidth kernel for the cache-level roofs. Three details decide whether it
/// measures bandwidth or just its own issue rate: it must use the widest
/// vectors the host has (the crate is built for the x86-64 baseline, so without
/// an explicit `target_feature` this stays scalar and every level reports the
/// same wrong number); it must avoid `f64::mul_add`, which lowers to a libm
/// call without `+fma`; and it must zip rather than index, so no bounds check
/// blocks vectorization.
#[inline(always)]
fn triad_body(a: &[f64], b: &[f64], c: &mut [f64], scale: f64) {
    for ((out, a), b) in c.iter_mut().zip(a.iter()).zip(b.iter()) {
        *out = *a + *b * scale;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx")]
unsafe fn triad_avx(a: &[f64], b: &[f64], c: &mut [f64], scale: f64) {
    triad_body(a, b, c, scale);
}

fn triad(a: &[f64], b: &[f64], c: &mut [f64], scale: f64) {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx") {
            // SAFETY: guarded by the runtime feature check above.
            unsafe { triad_avx(a, b, c, scale) };
            return;
        }
    }
    triad_body(a, b, c, scale);
}

/// Data/unified cache levels of cpu0 as `(level, capacity, sharing CPUs)`,
/// innermost first.
#[cfg(target_os = "linux")]
fn detect_cache_levels() -> Vec<(u32, u64, usize)> {
    let Ok(indices) = std::fs::read_dir("/sys/devices/system/cpu/cpu0/cache") else {
        return Vec::new();
    };
    let mut levels = Vec::new();
    for index in indices.flatten() {
        let path = index.path();
        let read = |name: &str| {
            std::fs::read_to_string(path.join(name))
                .ok()
                .map(|value| value.trim().to_owned())
        };
        if !matches!(read("type").as_deref(), Some("Unified" | "Data")) {
            continue;
        }
        let Some(level) = read("level").and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let Some(capacity) = read("size").and_then(|value| parse_cache_size(&value)) else {
            continue;
        };
        let sharers = read("shared_cpu_list")
            .map(|list| count_cpu_list(&list))
            .unwrap_or(1);
        levels.push((level, capacity, sharers));
    }
    levels.sort_by_key(|(level, capacity, _)| (*level, *capacity));
    levels.dedup_by_key(|(level, _, _)| *level);
    levels
}

#[cfg(not(target_os = "linux"))]
fn detect_cache_levels() -> Vec<(u32, u64, usize)> {
    Vec::new()
}

/// Counts CPUs in a sysfs list such as `0,4` or `0-7`.
#[cfg(target_os = "linux")]
fn count_cpu_list(list: &str) -> usize {
    let count = list
        .split(',')
        .filter_map(|part| {
            let part = part.trim();
            match part.split_once('-') {
                Some((first, last)) => {
                    let first = first.trim().parse::<usize>().ok()?;
                    let last = last.trim().parse::<usize>().ok()?;
                    Some(last.saturating_sub(first) + 1)
                }
                None => part.parse::<usize>().ok().map(|_| 1),
            }
        })
        .sum();
    usize::max(count, 1)
}

#[cfg(target_os = "linux")]
fn parse_cache_size(value: &str) -> Option<u64> {
    let (number, multiplier) = match value.as_bytes().last().copied()? {
        b'K' | b'k' => (&value[..value.len() - 1], 1024_u64),
        b'M' | b'm' => (&value[..value.len() - 1], 1024_u64 * 1024),
        b'G' | b'g' => (&value[..value.len() - 1], 1024_u64 * 1024 * 1024),
        _ => (value, 1),
    };
    number.trim().parse::<u64>().ok()?.checked_mul(multiplier)
}

pub(super) fn measure_memory() -> Result<MemoryBandwidthCalibration> {
    let threads = rayon::current_num_threads();
    let mut a = allocate_vector(MEMORY_ELEMENTS).context("allocate memory triad input A")?;
    let mut b = allocate_vector(MEMORY_ELEMENTS).context("allocate roofline triad input B")?;
    let mut c = allocate_vector(MEMORY_ELEMENTS).context("allocate roofline triad output")?;
    a.par_iter_mut()
        .enumerate()
        .for_each(|(index, value)| *value = 1.0 + (index % 17) as f64 * 1.0e-6);
    b.par_iter_mut()
        .enumerate()
        .for_each(|(index, value)| *value = 2.0 + (index % 31) as f64 * 1.0e-6);
    c.par_iter_mut().for_each(|value| *value = 0.0);

    let _ = measure_memory_sample(&a, &b, &mut c, 1);
    let memory_samples = (0..SAMPLES)
        .map(|_| measure_memory_sample(&a, &b, &mut c, MEMORY_REPETITIONS))
        .collect::<Vec<_>>();

    let memory_gbytes_per_second = median(&memory_samples);
    if !memory_gbytes_per_second.is_finite() || memory_gbytes_per_second <= 0.0 {
        bail!(
            "memory roof calibration produced an invalid result: \
             {memory_gbytes_per_second}"
        );
    }
    let memory_working_set_bytes = 3_u64
        .saturating_mul(MEMORY_ELEMENTS as u64)
        .saturating_mul(size_of::<f64>() as u64);

    Ok(MemoryBandwidthCalibration {
        threads,
        cpu_affinity: cpu_affinity(),
        samples: SAMPLES,
        gbytes_per_second: memory_gbytes_per_second,
        gbytes_per_second_samples: memory_samples,
        working_set_bytes: memory_working_set_bytes,
        source: "effective_stream".to_string(),
        memory_levels: Vec::new(),
    })
}

pub(super) fn measure_memory_hierarchy() -> Result<MemoryBandwidthCalibration> {
    let mut memory = measure_memory()?;
    memory.memory_levels = measure_memory_levels(memory.threads, &memory);
    Ok(memory)
}

fn allocate_vector(elements: usize) -> Result<Vec<f64>> {
    let mut vector = Vec::new();
    vector
        .try_reserve_exact(elements)
        .context("reserve calibration memory")?;
    vector.resize(elements, 0.0);
    Ok(vector)
}

#[derive(Clone, Copy)]
struct ComputeKernel {
    name: &'static str,
    flops_per_iteration: u64,
    run: unsafe fn(usize, usize) -> f64,
}

fn compute_kernel() -> ComputeKernel {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("fma")
        {
            return ComputeKernel {
                name: "x86-avx512-fma-f64",
                flops_per_iteration: 128,
                run: compute_avx512,
            };
        }
        if std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma")
        {
            return ComputeKernel {
                name: "x86-avx2-fma-f64",
                flops_per_iteration: 64,
                run: compute_avx2,
            };
        }
    }

    #[cfg(all(target_arch = "riscv64", target_os = "linux"))]
    if rvv_available() {
        let vl = unsafe { mperf_roofline_rvv_vlmax() };
        if vl > 0 {
            return ComputeKernel {
                name: "riscv-rvv-fma-f64",
                flops_per_iteration: 16 * vl as u64,
                run: compute_rvv,
            };
        }
    }

    ComputeKernel {
        name: "scalar-fma-f64",
        flops_per_iteration: 16,
        run: compute_scalar,
    }
}

#[cfg(all(target_arch = "riscv64", target_os = "linux"))]
fn rvv_available() -> bool {
    const RISCV_HWCAP_ISA_V: libc::c_ulong = 1 << (b'V' - b'A');
    unsafe { libc::getauxval(libc::AT_HWCAP) & RISCV_HWCAP_ISA_V != 0 }
}

#[cfg(target_arch = "riscv64")]
unsafe extern "C" {
    fn mperf_roofline_rvv_vlmax() -> usize;
    fn mperf_roofline_compute_rvv(iterations: usize, worker: usize) -> f64;
}

#[cfg(target_arch = "riscv64")]
unsafe fn compute_rvv(iterations: usize, worker: usize) -> f64 {
    unsafe { mperf_roofline_compute_rvv(iterations, worker) }
}

fn measure_compute_sample(kernel: ComputeKernel, threads: usize, iterations: usize) -> f64 {
    let start = Instant::now();
    let checksum = rayon::broadcast(|context| unsafe { (kernel.run)(iterations, context.index()) })
        .into_iter()
        .sum::<f64>();
    let elapsed = nonzero_elapsed(start.elapsed());
    black_box(checksum);

    iterations as f64 * kernel.flops_per_iteration as f64 * threads as f64
        / elapsed.as_secs_f64()
        / 1.0e9
}

fn measure_memory_sample(a: &[f64], b: &[f64], c: &mut [f64], repetitions: usize) -> f64 {
    let start = Instant::now();
    for repetition in 0..repetitions {
        c.par_chunks_mut(MEMORY_CHUNK_ELEMENTS)
            .enumerate()
            .for_each(|(chunk_index, output)| {
                let offset = chunk_index * MEMORY_CHUNK_ELEMENTS;
                let a = &a[offset..offset + output.len()];
                let b = &b[offset..offset + output.len()];
                for index in 0..output.len() {
                    output[index] = b[index].mul_add(1.000_000_1, a[index]);
                }
            });
        black_box(c[(repetition * 8191) % c.len()]);
    }
    let elapsed = nonzero_elapsed(start.elapsed());
    let bytes = repetitions as f64 * c.len() as f64 * 3.0 * size_of::<f64>() as f64;
    bytes / elapsed.as_secs_f64() / 1.0e9
}

fn nonzero_elapsed(elapsed: Duration) -> Duration {
    elapsed.max(Duration::from_nanos(1))
}

fn median(values: &[f64]) -> f64 {
    let mut values = values.to_vec();
    values.sort_unstable_by(f64::total_cmp);
    values[values.len() / 2]
}

#[cfg(target_os = "linux")]
fn affinity_cpus() -> Option<Vec<usize>> {
    let mut set = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    if unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) } != 0 {
        return None;
    }
    let cpus = (0..libc::CPU_SETSIZE as usize)
        .filter(|cpu| unsafe { libc::CPU_ISSET(*cpu, &set) })
        .collect::<Vec<_>>();
    (!cpus.is_empty()).then_some(cpus)
}

#[cfg(not(target_os = "linux"))]
fn affinity_cpus() -> Option<Vec<usize>> {
    None
}

#[cfg(target_os = "linux")]
struct ThreadAffinityGuard {
    previous: libc::cpu_set_t,
}

#[cfg(target_os = "linux")]
impl Drop for ThreadAffinityGuard {
    fn drop(&mut self) {
        unsafe {
            libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &self.previous);
        }
    }
}

#[cfg(target_os = "linux")]
fn pin_current_thread(cpu: usize) -> Option<ThreadAffinityGuard> {
    if cpu >= libc::CPU_SETSIZE as usize {
        return None;
    }
    let mut previous = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    if unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut previous) } != 0 {
        return None;
    }
    let mut pinned = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    unsafe { libc::CPU_SET(cpu, &mut pinned) };
    if unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &pinned) } != 0 {
        return None;
    }
    Some(ThreadAffinityGuard { previous })
}

#[cfg(not(target_os = "linux"))]
struct ThreadAffinityGuard;

#[cfg(not(target_os = "linux"))]
fn pin_current_thread(_cpu: usize) -> Option<ThreadAffinityGuard> {
    None
}

fn cpu_affinity() -> Option<String> {
    affinity_cpus().map(|cpus| format_cpu_list(&cpus))
}

fn format_cpu_list(cpus: &[usize]) -> String {
    let mut ranges = Vec::new();
    let mut start = *cpus.first().unwrap_or(&0);
    let mut end = start;
    for &cpu in cpus.iter().skip(1) {
        if cpu == end + 1 {
            end = cpu;
        } else {
            ranges.push((start, end));
            start = cpu;
            end = cpu;
        }
    }
    if !cpus.is_empty() {
        ranges.push((start, end));
    }
    ranges
        .into_iter()
        .map(|(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

unsafe fn compute_scalar(iterations: usize, worker: usize) -> f64 {
    let seed = 0.1 * (worker + 1) as f64;
    let multiplier = 1.000_000_000_000_000_2_f64;
    let addend = 0.000_000_000_000_000_1_f64;
    let mut accumulators = std::array::from_fn::<_, 8, _>(|index| seed + index as f64 * 0.1);
    for _ in 0..iterations {
        for accumulator in &mut accumulators {
            *accumulator = accumulator.mul_add(multiplier, addend);
        }
    }
    black_box(accumulators).into_iter().sum()
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn compute_avx2(iterations: usize, worker: usize) -> f64 {
    use std::arch::x86_64::*;

    let seed = _mm256_set1_pd(0.1 * (worker + 1) as f64);
    let multiplier = _mm256_set1_pd(1.000_000_000_000_000_2);
    let addend = _mm256_set1_pd(0.000_000_000_000_000_1);
    let increment = _mm256_set1_pd(0.1);
    let mut a0 = seed;
    let mut a1 = _mm256_add_pd(a0, increment);
    let mut a2 = _mm256_add_pd(a1, increment);
    let mut a3 = _mm256_add_pd(a2, increment);
    let mut a4 = _mm256_add_pd(a3, increment);
    let mut a5 = _mm256_add_pd(a4, increment);
    let mut a6 = _mm256_add_pd(a5, increment);
    let mut a7 = _mm256_add_pd(a6, increment);

    for _ in 0..iterations {
        a0 = _mm256_fmadd_pd(a0, multiplier, addend);
        a1 = _mm256_fmadd_pd(a1, multiplier, addend);
        a2 = _mm256_fmadd_pd(a2, multiplier, addend);
        a3 = _mm256_fmadd_pd(a3, multiplier, addend);
        a4 = _mm256_fmadd_pd(a4, multiplier, addend);
        a5 = _mm256_fmadd_pd(a5, multiplier, addend);
        a6 = _mm256_fmadd_pd(a6, multiplier, addend);
        a7 = _mm256_fmadd_pd(a7, multiplier, addend);
    }

    let sum01 = _mm256_add_pd(a0, a1);
    let sum23 = _mm256_add_pd(a2, a3);
    let sum45 = _mm256_add_pd(a4, a5);
    let sum67 = _mm256_add_pd(a6, a7);
    let sum = _mm256_add_pd(_mm256_add_pd(sum01, sum23), _mm256_add_pd(sum45, sum67));
    let mut lanes = [0.0; 4];
    // SAFETY: `lanes` provides four writable, contiguous f64 values; the
    // unaligned store therefore writes exactly within the array.
    unsafe { _mm256_storeu_pd(lanes.as_mut_ptr(), sum) };
    black_box(lanes).into_iter().sum()
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,fma")]
unsafe fn compute_avx512(iterations: usize, worker: usize) -> f64 {
    use std::arch::x86_64::*;

    let seed = _mm512_set1_pd(0.1 * (worker + 1) as f64);
    let multiplier = _mm512_set1_pd(1.000_000_000_000_000_2);
    let addend = _mm512_set1_pd(0.000_000_000_000_000_1);
    let increment = _mm512_set1_pd(0.1);
    let mut a0 = seed;
    let mut a1 = _mm512_add_pd(a0, increment);
    let mut a2 = _mm512_add_pd(a1, increment);
    let mut a3 = _mm512_add_pd(a2, increment);
    let mut a4 = _mm512_add_pd(a3, increment);
    let mut a5 = _mm512_add_pd(a4, increment);
    let mut a6 = _mm512_add_pd(a5, increment);
    let mut a7 = _mm512_add_pd(a6, increment);

    for _ in 0..iterations {
        a0 = _mm512_fmadd_pd(a0, multiplier, addend);
        a1 = _mm512_fmadd_pd(a1, multiplier, addend);
        a2 = _mm512_fmadd_pd(a2, multiplier, addend);
        a3 = _mm512_fmadd_pd(a3, multiplier, addend);
        a4 = _mm512_fmadd_pd(a4, multiplier, addend);
        a5 = _mm512_fmadd_pd(a5, multiplier, addend);
        a6 = _mm512_fmadd_pd(a6, multiplier, addend);
        a7 = _mm512_fmadd_pd(a7, multiplier, addend);
    }

    let sum01 = _mm512_add_pd(a0, a1);
    let sum23 = _mm512_add_pd(a2, a3);
    let sum45 = _mm512_add_pd(a4, a5);
    let sum67 = _mm512_add_pd(a6, a7);
    let sum = _mm512_add_pd(_mm512_add_pd(sum01, sum23), _mm512_add_pd(sum45, sum67));
    let mut lanes = [0.0; 8];
    // SAFETY: `lanes` provides eight writable, contiguous f64 values; the
    // unaligned store therefore writes exactly within the array.
    unsafe { _mm512_storeu_pd(lanes.as_mut_ptr(), sum) };
    black_box(lanes).into_iter().sum()
}
