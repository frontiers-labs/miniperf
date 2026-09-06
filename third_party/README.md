# Vendored crates

## libduckdb-sys

An unmodified copy of `libduckdb-sys` 1.10505.0 from crates.io, with one
addition: a dependency on `miniperf-duckdb-prefetch`. `build.rs` and `src/` are
byte-identical to the published crate.

That single edge is the whole point. `libduckdb-sys` emits
`cargo:rustc-link-lib=static=duckdb_static`, and rustc bundles that archive
into the crate's rlib *as it compiles the crate*, so the library has to be on
disk before then. Cargo orders a `links` crate ahead of every dependent, which
puts `miniperf-store`'s build script far too late. A dependency is the hook
that lands early enough, because cargo builds it, running its build script,
before it compiles the dependent.

It is a normal dependency, not a build-dependency, on purpose:
build-dependencies are compiled for the host, so their build scripts read the
host's `CARGO_CFG_TARGET_ARCH` and would install the wrong architecture when
cross-compiling. That failure is quiet until the link, where it surfaces as
`Relocations in generic ELF (EM: 62)` on a riscv64 build.

`duckdb.tar.gz` (the 5.9 MB DuckDB amalgamation) and `Cargo.lock` are dropped:
the amalgamation is reachable only through the `bundled` feature, which must
stay off. Turning `bundled` on compiles DuckDB from source and ignores the
pinned library entirely -- that is what the `duckdb` crate's `parquet` feature
does, which is why `miniperf-store` does not ask for it.

### Re-vendoring

`deps/manifest.toml` already requires bumping the DuckDB pin and the `duckdb`
crate together, because the pinned library must match the crate's pregenerated
bindings. Re-vendor as part of that same change:

```sh
cargo fetch
rm -rf third_party/libduckdb-sys
cp -r ~/.cargo/registry/src/*/libduckdb-sys-<version> third_party/libduckdb-sys
chmod -R u+w third_party/libduckdb-sys
rm -f third_party/libduckdb-sys/duckdb.tar.gz third_party/libduckdb-sys/Cargo.lock
```

Then re-apply the `[dependencies.miniperf-duckdb-prefetch]` entry and update
the version in the workspace `[patch.crates-io]` comment. `git diff`
against the registry copy should show nothing else.

## duckdb-prefetch

Downloads the DuckDB pinned in `deps/manifest.toml`, verifies its SHA-256 and
unpacks it into `deps/cache/duckdb`, where `.cargo/config.toml` points
`libduckdb-sys` through `DUCKDB_LIB_DIR`. It has no dependencies of its own, so
it cannot pull anything into the graph ahead of the crate that needs it, and
its library target is deliberately empty -- all the work is in its build
script. A stamp file naming the platform and digest makes re-runs a no-op and
reinstalls when the target or the pin changes.
