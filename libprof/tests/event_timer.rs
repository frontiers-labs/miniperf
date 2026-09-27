use std::time::Duration;

use libprof::{Counter, EventTimer};

#[test]
fn with_kernel_counts_context_switches() {
    let counters = [Counter::Cycles, Counter::ContextSwitches];
    let Ok(timer) = EventTimer::with_kernel(&counters) else {
        eprintln!("skipped: perf events unavailable");
        return;
    };
    let span = timer.start().unwrap();
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(1));
    }
    let m = span.stop().unwrap();
    let switches = m.iter().nth(1).unwrap().raw();
    assert!(switches >= 5, "{switches} context switches over 5 sleeps");
}
