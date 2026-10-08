use std::{env, error::Error, fs, fs::File, path::Path};

use event_import::{import_amd, import_arm_telemetry, import_intel};

const USAGE: &str = "\
usage:
  event-import intel <source-file-or-dir> <output.json> <family-id> <name>
  event-import intel-linux <source-dir> <output.json> <family-id> <name>
  event-import amd-linux <source-dir> <output.json> <family-id> <name>
  event-import arm-telemetry <source.json> <output.json> <family-id> <name>

Converts Intel perfmon, Linux perf, or Arm Telemetry Solution PMU JSON into
miniperf's platform format. Uncore and extra-MSR events are not imported.";

fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 6 {
        return Err(USAGE.into());
    }

    let output = Path::new(&args[3]);
    let source = Path::new(&args[2]);
    let platform = match args[1].as_str() {
        "intel" | "intel-linux" => import_intel(source, &args[4], &args[5])?,
        "amd-linux" => import_amd(source, &args[4], &args[5])?,
        "arm-telemetry" => import_arm_telemetry(source, &args[4], &args[5])?,
        _ => return Err(USAGE.into()),
    };
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    serde_json::to_writer_pretty(File::create(output)?, &platform)?;
    println!(
        "imported {} events and {} metrics into {}",
        platform.events.len(),
        platform.metrics.len(),
        output.display()
    );
    Ok(())
}
