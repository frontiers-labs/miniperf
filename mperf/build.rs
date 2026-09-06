use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=src/roofline/calibrate_rvv.c");
    println!("cargo:rerun-if-changed=../utils/profile_smoke.c");
    println!("cargo:rerun-if-changed=../utils/roofline_threads.c");

    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("riscv64") {
        cc::Build::new()
            .file("src/roofline/calibrate_rvv.c")
            .opt_level(3)
            .flag("-march=rv64gcv")
            .flag("-mabi=lp64d")
            .compile("mperf_roofline_rvv");
    }

    let smoke = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("profile_smoke");
    let status = Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
        .args([
            "-O2",
            "-g",
            "-fno-omit-frame-pointer",
            "../utils/profile_smoke.c",
            "-o",
        ])
        .arg(&smoke)
        .status()
        .expect("failed to run the C compiler");
    assert!(status.success(), "failed to build utils/profile_smoke.c");
    println!("cargo:rustc-env=MPERF_SMOKE_BIN={}", smoke.display());

    let threads = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("roofline_threads");
    let status = Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
        .args([
            "-O2",
            "-g",
            "-fno-omit-frame-pointer",
            "-pthread",
            "../utils/roofline_threads.c",
            "-o",
        ])
        .arg(&threads)
        .status()
        .expect("failed to run the C compiler");
    assert!(status.success(), "failed to build utils/roofline_threads.c");
    println!("cargo:rustc-env=MPERF_THREADS_BIN={}", threads.display());
}
