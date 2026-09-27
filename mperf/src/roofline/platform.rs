use std::sync::{Arc, atomic::AtomicBool};

use crate::event_dispatcher::EventDispatcher;

pub(super) fn publish_initial_process_maps(dispatcher: &Arc<EventDispatcher>, pid: u32) {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    crate::record::publish_process_maps(dispatcher, pid);
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let _ = (dispatcher, pid);
}

#[cfg(target_os = "windows")]
pub(super) fn start_module_poller(
    dispatcher: Arc<EventDispatcher>,
    pid: u32,
    stop: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    use std::sync::atomic::Ordering;

    Some(std::thread::spawn(move || {
        let mut seen = std::collections::HashSet::new();
        while !stop.load(Ordering::Acquire) {
            for module in libprof::process_modules(pid) {
                if seen.insert((module.addr, module.len, module.filename.clone())) {
                    libprof::Sink::record(dispatcher.as_ref(), libprof::Record::ProcAddr(module));
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }))
}

#[cfg(not(target_os = "windows"))]
pub(super) fn start_module_poller(
    _dispatcher: Arc<EventDispatcher>,
    _pid: u32,
    _stop: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    None
}

pub(super) fn missing_counters_warning(counters: &[&str]) -> String {
    #[cfg(target_os = "windows")]
    {
        format!(
            "Windows did not expose these optional sampling counters: {}",
            counters.join(", ")
        )
    }
    #[cfg(not(target_os = "windows"))]
    {
        format!(
            "this host's PMU cannot run the full sampling group, so these counters were not sampled: {}",
            counters.join(", ")
        )
    }
}
