//! Reports what a scenario's sampling group actually collects on this host.
//!
//! A recording whose hardware counters were silently shed looks like a working
//! recording until something reads a `pmu_*` column that is not there, so this
//! runs the real group against a live workload and prints the samples and the
//! whole-run totals it produced.
//!
//! ```sh
//! cargo run --example sampling_probe
//! ```
use libprof::{probe_sampling_group, Counter};

fn main() -> anyhow::Result<()> {
    let narrow = std::env::var_os("NARROW").is_some();
    let requested = if narrow {
        vec![Counter::Cycles, Counter::Instructions, Counter::CpuClock]
    } else {
        vec![
            Counter::Cycles,
            Counter::Instructions,
            Counter::LLCReferences,
            Counter::LLCMisses,
            Counter::BranchMisses,
            Counter::BranchInstructions,
            Counter::StalledCyclesBackend,
            Counter::StalledCyclesFrontend,
            Counter::CpuClock,
            Counter::CpuMigrations,
            Counter::PageFaults,
            Counter::ContextSwitches,
        ]
    };

    let millis = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(300);
    let probe = probe_sampling_group(&requested, millis)?;

    println!(
        "requested {} counters, opened {} ({} hardware)",
        probe.requested.len(),
        probe.opened.len(),
        probe.hardware_opened().len()
    );
    let dropped = probe
        .hardware_dropped()
        .iter()
        .map(|counter| counter.name())
        .collect::<Vec<_>>();
    if !dropped.is_empty() {
        println!("dropped: {}", dropped.join(", "));
    }
    println!("samples: {}", probe.samples);
    println!(
        "collapsed to software only: {}",
        probe.collapsed_to_software()
    );
    println!("healthy: {}", probe.is_healthy());

    // Cross-check against a counting driver on an identical workload: the
    // sampler's totals should land in the same ballpark.
    let process = libprof::Process::new(
        &[
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "while :; do :; done".to_owned(),
        ],
        &[],
    )?;
    let mut counting = libprof::CountingDriverBuilder::new()
        .counters(&[Counter::Cycles, Counter::Instructions])
        .process(Some(&process))
        .build()?;
    counting.start()?;
    process.cont();
    std::thread::sleep(std::time::Duration::from_millis(millis));
    unsafe { libc::kill(process.pid(), libc::SIGKILL) };
    let _ = process.wait();
    counting.stop()?;
    let counted = counting.counters()?;
    for counter in [Counter::Cycles, Counter::Instructions] {
        let name = counter.name().to_owned();
        println!(
            "  counting {name:<25} {}",
            counted.get(counter).map_or(0, |value| value.value)
        );
    }
    Ok(())
}
