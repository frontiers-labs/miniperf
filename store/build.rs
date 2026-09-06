//! Link flags for the DuckDB build pinned in `deps/manifest.toml`.
//!
//! The library itself is installed by `miniperf-duckdb-prefetch`, a
//! build-dependency of the vendored `libduckdb-sys`, because that is the only
//! hook that runs before rustc bundles the static archive into that crate's
//! rlib. See `third_party/README.md`.

use std::env;

fn main() {
    if env::var_os("CARGO_FEATURE_SESSION").is_none() {
        return;
    }

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
