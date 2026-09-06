//! Runs the built `mperf` binary through every subcommand against a real
//! workload. Each test asserts the command finishes and produces its output;
//! `truth` holds the analytic assertions.

use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::Duration,
};

const MPERF: &str = env!("CARGO_BIN_EXE_mperf");
const WORKLOAD: &str = env!("MPERF_SMOKE_BIN");
const THREADED_WORKLOAD: &str = env!("MPERF_THREADS_BIN");

fn pmu_available() -> bool {
    if env::var_os("MPERF_NO_PMU").is_some() {
        eprintln!("skipped: MPERF_NO_PMU is set");
        return false;
    }
    let level = fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
        .ok()
        .and_then(|value| value.trim().parse::<i32>().ok());
    assert!(
        level.is_some_and(|level| level <= -1),
        "perf_event access is unavailable (kernel.perf_event_paranoid={level:?}); \
         set kernel.perf_event_paranoid=-1, or set MPERF_NO_PMU=1 to skip the profiler tests explicitly"
    );
    true
}

fn mperf(args: &[&str]) -> Output {
    Command::new(MPERF)
        .args(args)
        .output()
        .expect("failed to launch mperf")
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_ok(output: &Output, what: &str) -> String {
    let log = text(output);
    assert!(
        output.status.success(),
        "{what} failed: {}\n{log}",
        output.status
    );
    assert!(!log.contains("panicked"), "{what} panicked\n{log}");
    log
}

fn results_dir(name: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!("mperf-smoke-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn record(scenario: &str, dir: &Path) -> Output {
    mperf(&[
        "record",
        "-s",
        scenario,
        "-o",
        dir.to_str().unwrap(),
        "--",
        WORKLOAD,
    ])
}

fn query(dir: &Path, sql: &str) -> String {
    let output = mperf(&["query", "-f", "json", dir.to_str().unwrap(), sql]);
    assert_ok(&output, &format!("query `{sql}`"))
}

fn assert_tables(dir: &Path, tables: &[&str]) {
    for table in tables {
        let rows = query(dir, &format!("select count(*) as n from {table}"));
        assert!(
            !rows.contains("\"n\": 0") && !rows.contains("\"n\":0"),
            "{table} is empty: {rows}"
        );
    }
}

fn show_opens(dir: &Path) {
    let mut child = Command::new("script")
        .args([
            "-qec",
            &format!("{MPERF} show {}", dir.display()),
            "/dev/null",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run `script`");
    thread::sleep(Duration::from_millis(1500));
    let _ = child.stdin.take().unwrap().write_all(b"q");
    let output = child.wait_with_output().unwrap();
    assert_ok(&output, "show");
}

#[test]
fn doctor_and_list_run() {
    let log = text(&mperf(&["doctor"]));
    assert!(!log.contains("panicked"), "doctor panicked\n{log}");
    assert!(
        log.contains("perf_event_paranoid"),
        "doctor did not report perf access\n{log}"
    );
    assert_ok(&mperf(&["list"]), "list");
}

#[test]
fn stat_counts_the_workload() {
    if !pmu_available() {
        return;
    }
    let log = assert_ok(&mperf(&["stat", "--", WORKLOAD]), "stat");
    assert!(
        log.contains("smoke checksum"),
        "workload did not run\n{log}"
    );
    assert!(
        log.contains("cycles") && log.contains("instructions"),
        "no counters\n{log}"
    );
}

#[test]
fn snapshot_records_queries_exports_and_shows() {
    if !pmu_available() {
        return;
    }
    let dir = results_dir("snapshot");
    assert_ok(&record("snapshot", &dir), "record -s snapshot");
    assert!(dir.join("info.json").is_file());
    assert_tables(
        &dir,
        &[
            "samples",
            "hotspots",
            "modules",
            "stacks",
            "capture_fidelity",
        ],
    );
    let hot = query(
        &dir,
        "select func_name from hotspots order by cycles desc limit 3",
    );
    assert!(
        hot.contains("leaf_work"),
        "leaf_work is not a top hotspot:\n{hot}"
    );
    assert_ok(
        &mperf(&["events-export", dir.to_str().unwrap()]),
        "events-export",
    );
    show_opens(&dir);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn other_scenarios_record_or_explain() {
    if !pmu_available() {
        return;
    }
    for (scenario, table) in [
        ("tma", "tma_summary"),
        ("roofline", "samples"),
        ("mem", "samples"),
    ] {
        let dir = results_dir(scenario);
        let output = record(scenario, &dir);
        let log = text(&output);
        assert!(
            !log.contains("panicked"),
            "record -s {scenario} panicked\n{log}"
        );
        if output.status.success() {
            assert_tables(&dir, &[table]);
        } else {
            assert!(
                log.contains("Error"),
                "{scenario} failed without an explanation\n{log}"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }
}

/// A loop run by two threads at once is timed as one wall-clock window
/// with two threads' worth of CPU time. Needs an accounting engine; a host
/// without one must refuse with an explanation rather than mis-time it.
#[test]
fn roofline_times_every_thread_of_a_parallel_loop() {
    if !pmu_available() {
        return;
    }
    let dir = results_dir("roofline-threads");
    let output = mperf(&[
        "record",
        "-s",
        "roofline",
        "-o",
        dir.to_str().unwrap(),
        "--",
        THREADED_WORKLOAD,
        "2",
        "20000",
    ]);
    let log = text(&output);
    assert!(!log.contains("panicked"), "record panicked\n{log}");
    if !output.status.success() {
        assert!(
            log.contains("Error"),
            "roofline failed without an explanation\n{log}"
        );
        eprintln!("skipped: no Roofline accounting engine on this host\n{log}");
        let _ = fs::remove_dir_all(&dir);
        return;
    }
    let rows: serde_json::Value = serde_json::from_str(&query(
        &dir,
        "SELECT thread_count, duration_ns, cpu_time_ns, timing_samples FROM roofline_loops \
         WHERE timing_quality = 'high-confidence' ORDER BY cpu_time_ns DESC NULLS LAST LIMIT 1",
    ))
    .unwrap();
    let row = rows
        .as_array()
        .and_then(|rows| rows.first())
        .unwrap_or_else(|| panic!("no timed loop in the recording:\n{log}"));
    let field = |name: &str| {
        row[name]
            .as_i64()
            .unwrap_or_else(|| panic!("{name} in {row}"))
    };
    let (threads, duration, cpu) = (
        field("thread_count"),
        field("duration_ns"),
        field("cpu_time_ns"),
    );
    assert_eq!(threads, 2, "the daxpy loop ran on two threads: {row}");
    assert!(
        cpu > duration && cpu <= duration * 2,
        "two concurrent threads: cpu_time_ns {cpu} should be between duration_ns {duration} and twice it"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn recover_repairs_a_killed_recording() {
    if !pmu_available() {
        return;
    }
    let dir = results_dir("recover");
    let mut child = Command::new(MPERF)
        .args([
            "record",
            "-s",
            "snapshot",
            "-o",
            dir.to_str().unwrap(),
            "--",
            WORKLOAD,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(1200));
    unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    child.wait().unwrap();
    assert_ok(&mperf(&["recover", dir.to_str().unwrap()]), "recover");
    fs::remove_dir_all(&dir).unwrap();
}
