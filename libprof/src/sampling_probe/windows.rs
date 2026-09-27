//! Windows workload for the live ETW sampling probe.

use crate::Process;

pub(super) fn workload_command() -> Vec<String> {
    vec![
        "cmd.exe".to_owned(),
        "/C".to_owned(),
        "for /L %i in (0,0,1) do @rem".to_owned(),
    ]
}

pub(super) fn stop_workload(process: &Process) {
    let _ = process.terminate();
}
