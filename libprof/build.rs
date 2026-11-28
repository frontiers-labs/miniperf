use std::{error::Error, fs, path::PathBuf};

use glob::glob;
use pmu_data::PlatformDesc;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=events/");

    let arch = std::env::var("CARGO_CFG_TARGET_ARCH")?;
    let native_events = std::env::var_os("CARGO_FEATURE_EVENTS_NATIVE").is_some();
    let mut families = Vec::new();
    for entry in glob("events/**/*.json")? {
        let path = entry?;
        println!("cargo:rerun-if-changed={}", path.display());

        // Validate every shipped table, even when this build excludes its architecture.
        let json = fs::read_to_string(path)?;
        let data: PlatformDesc = serde_json::from_str(&json)?;
        let arch_feature = match data.arch.as_str() {
            "x86_64" => "CARGO_FEATURE_EVENTS_X86_64",
            "aarch64" => "CARGO_FEATURE_EVENTS_AARCH64",
            "riscv64" | "riscv64gc" => "CARGO_FEATURE_EVENTS_RISCV64",
            _ => continue,
        };
        if (native_events && arch == data.arch) || std::env::var_os(arch_feature).is_some() {
            families.push(json);
        }
    }

    let output = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is missing")?);
    fs::write(
        output.join("pmu-catalog.json"),
        format!("[{}]", families.join(",")),
    )?;
    Ok(())
}
