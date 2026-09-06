//! Link flags for the DuckDB build pinned in `deps/manifest.toml`.
//!
//! The library itself is installed by `utils/deps/setup-duckdb.sh`, which has
//! to run before cargo does: `libduckdb-sys` declares `links = "duckdb"`, so
//! both its build script and its compilation happen before any dependent's
//! build script, and rustc bundles the static archive into the rlib as it
//! compiles. A build script here can only contribute the flags that
//! `libduckdb-sys` omits.

use std::env;

fn main() {
    if env::var_os("CARGO_FEATURE_SESSION").is_none() {
        return;
    }

    println!("cargo:rerun-if-changed=../deps/manifest.toml");

    // DuckDB is C++, and `libduckdb-sys` emits the C++ runtime only from its
    // `bundled` path -- the one that compiles the amalgamation itself. Linking
    // the pinned library instead means naming that runtime here.
    let runtime = match env::var("CARGO_CFG_TARGET_OS")
        .expect("Cargo sets it")
        .as_str()
    {
        "macos" => "c++",
        "windows" => return,
        _ => "stdc++",
    };
    println!("cargo:rustc-link-lib=dylib={runtime}");
}
