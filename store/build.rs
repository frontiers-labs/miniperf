//! Materialises the DuckDB build pinned in `deps/manifest.toml`.
//!
//! `libduckdb-sys` emits its link flags from `DUCKDB_LIB_DIR` (set in
//! `.cargo/config.toml`) without requiring the directory to exist yet, and
//! Cargo runs a dependency's build script before its dependents'. That leaves
//! this script as the last hook before `store` is linked, so it is where the
//! pinned artifact gets downloaded, checksummed and unpacked.

use std::{
    env,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

const REPOSITORY: &str = "https://github.com/alexbatashev/miniperf";

fn main() {
    if env::var_os("CARGO_FEATURE_SESSION").is_none() {
        return;
    }

    let workspace = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets it"))
        .parent()
        .expect("store lives in the workspace")
        .to_path_buf();
    let manifest_path = workspace.join("deps/manifest.toml");
    println!("cargo:rerun-if-changed={}", manifest_path.display());

    let manifest = fs::read_to_string(&manifest_path)
        .unwrap_or_else(|error| fail(&format!("cannot read {}: {error}", manifest_path.display())));
    let platform = target_platform();
    let release = manifest_value(&manifest, "release")
        .unwrap_or_else(|| fail("deps/manifest.toml has no `release` key"));
    let (file, sha256) = artifact(&manifest, &platform).unwrap_or_else(|| {
        fail(&format!(
            "deps/manifest.toml pins no DuckDB build for {platform}.\n\
             Add {platform} to the duckdb matrix in .github/workflows/deps.yml, \
             re-run the Dependencies workflow, and merge the repin pull request."
        ))
    });

    let cache = workspace.join("deps/cache/duckdb");
    let stamp = cache.join(".stamp");
    let want = format!("{platform} {sha256}\n");
    if fs::read_to_string(&stamp).ok().as_deref() == Some(want.as_str()) {
        return;
    }

    let url = format!("{REPOSITORY}/releases/download/{release}/{file}");
    let archive = download(&url, &file, &sha256, &workspace.join("deps/cache/download"));
    let _ = fs::remove_dir_all(&cache);
    fs::create_dir_all(&cache)
        .unwrap_or_else(|error| fail(&format!("cannot create cache: {error}")));
    unpack(&archive, &cache, &file);

    let library = cache.join("lib");
    if !library.is_dir() {
        fail(&format!(
            "{file} does not contain lib/; the DuckDB bundle layout changed"
        ));
    }
    File::create(&stamp)
        .and_then(|mut handle| handle.write_all(want.as_bytes()))
        .unwrap_or_else(|error| fail(&format!("cannot write stamp: {error}")));
}

fn target_platform() -> String {
    let os = env::var("CARGO_CFG_TARGET_OS").expect("Cargo sets it");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("Cargo sets it");
    let os = match os.as_str() {
        "macos" => "macos",
        "windows" => "windows",
        "linux" => "linux",
        other => fail(&format!("miniperf-store does not support {other}")),
    };
    let arch = match arch.as_str() {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        "riscv64" => "riscv64",
        other => fail(&format!("miniperf-store does not support {other}")),
    };
    format!("{os}-{arch}")
}

/// Reads a top-level `key = "value"` pair.
fn manifest_value(manifest: &str, key: &str) -> Option<String> {
    manifest
        .lines()
        .map(str::trim)
        .find_map(|line| {
            line.strip_prefix(key)?
                .trim_start()
                .strip_prefix('=')
                .map(str::trim)
        })
        .map(|value| value.trim_matches('"').to_string())
}

/// Reads `file` and `sha256` out of `[artifacts.duckdb."<platform>"]`.
fn artifact(manifest: &str, platform: &str) -> Option<(String, String)> {
    let header = format!("[artifacts.duckdb.\"{platform}\"]");
    let body = manifest.split(&header).nth(1)?;
    let table = body.split("\n[").next()?;
    Some((
        manifest_value(table, "file")?,
        manifest_value(table, "sha256")?,
    ))
}

/// Downloads `url` into `cache`, reusing an existing copy with the right digest
/// so alternating host and cross builds do not refetch it.
fn download(url: &str, file: &str, sha256: &str, cache: &Path) -> PathBuf {
    fs::create_dir_all(cache)
        .unwrap_or_else(|error| fail(&format!("cannot create download cache: {error}")));
    let out = cache.join(file);
    if out.is_file() && sha256_of(&out) == sha256 {
        return out;
    }
    run(
        "curl",
        &[
            "--fail",
            "--location",
            "--retry",
            "3",
            "--silent",
            "--show-error",
            url,
            "--output",
            &out.to_string_lossy(),
        ],
    );
    let actual = sha256_of(&out);
    if actual != sha256 {
        fail(&format!(
            "checksum mismatch for {url}\n  expected {sha256}\n  found    {actual}"
        ));
    }
    out
}

fn sha256_of(path: &Path) -> String {
    let mut contents = Vec::new();
    File::open(path)
        .and_then(|mut handle| handle.read_to_end(&mut contents))
        .unwrap_or_else(|error| fail(&format!("cannot read {}: {error}", path.display())));
    sha256::hex(&contents)
}

/// Unpacks the bundle, flattening its single top-level directory into `cache`.
fn unpack(archive: &Path, cache: &Path, file: &str) {
    let staging = cache.join(".staging");
    fs::create_dir_all(&staging).unwrap_or_else(|error| fail(&format!("cannot stage: {error}")));
    let archive = archive.to_string_lossy().to_string();
    let target = staging.to_string_lossy().to_string();
    if file.ends_with(".zip") {
        run(
            "tar",
            &["--extract", "--file", &archive, "--directory", &target],
        );
    } else {
        run(
            "tar",
            &[
                "--extract",
                "--zstd",
                "--file",
                &archive,
                "--directory",
                &target,
            ],
        );
    }

    let bundle = fs::read_dir(&staging)
        .unwrap_or_else(|error| fail(&format!("cannot list staging: {error}")))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.is_dir())
        .unwrap_or_else(|| fail(&format!("{file} contains no bundle directory")));
    for entry in fs::read_dir(&bundle).unwrap_or_else(|error| fail(&format!("{error}"))) {
        let entry = entry.unwrap_or_else(|error| fail(&format!("{error}")));
        fs::rename(entry.path(), cache.join(entry.file_name()))
            .unwrap_or_else(|error| fail(&format!("cannot install {:?}: {error}", entry.path())));
    }
    let _ = fs::remove_dir_all(&staging);
}

fn run(program: &str, arguments: &[&str]) {
    let status = std::process::Command::new(program)
        .args(arguments)
        .status()
        .unwrap_or_else(|error| fail(&format!("cannot run {program}: {error}")));
    if !status.success() {
        fail(&format!("{program} failed: {status}"));
    }
}

fn fail(message: &str) -> ! {
    let _ = writeln!(io::stderr(), "\nminiperf-store: {message}\n");
    std::process::exit(1);
}

/// Minimal SHA-256 so the build script needs no dependencies.
mod sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    pub fn hex(data: &[u8]) -> String {
        let mut state: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        let mut message = data.to_vec();
        let bits = (data.len() as u64) * 8;
        message.push(0x80);
        while message.len() % 64 != 56 {
            message.push(0);
        }
        message.extend_from_slice(&bits.to_be_bytes());

        let (blocks, _) = message.as_chunks::<64>();
        for chunk in blocks {
            let mut w = [0u32; 64];
            let (words, _) = chunk.as_chunks::<4>();
            for (slot, word) in w.iter_mut().zip(words) {
                *slot = u32::from_be_bytes(*word);
            }
            for index in 16..64 {
                let s0 = w[index - 15].rotate_right(7)
                    ^ w[index - 15].rotate_right(18)
                    ^ (w[index - 15] >> 3);
                let s1 = w[index - 2].rotate_right(17)
                    ^ w[index - 2].rotate_right(19)
                    ^ (w[index - 2] >> 10);
                w[index] = w[index - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[index - 7])
                    .wrapping_add(s1);
            }

            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
            for index in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let choose = (e & f) ^ ((!e) & g);
                let temp1 = h
                    .wrapping_add(s1)
                    .wrapping_add(choose)
                    .wrapping_add(K[index])
                    .wrapping_add(w[index]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let majority = (a & b) ^ (a & c) ^ (b & c);
                let temp2 = s0.wrapping_add(majority);
                h = g;
                g = f;
                f = e;
                e = d.wrapping_add(temp1);
                d = c;
                c = b;
                b = a;
                a = temp1.wrapping_add(temp2);
            }
            for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
                *slot = slot.wrapping_add(value);
            }
        }

        state.iter().map(|word| format!("{word:08x}")).collect()
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn matches_known_vectors() {
            assert_eq!(
                super::hex(b""),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            );
            assert_eq!(
                super::hex(b"abc"),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            );
        }
    }
}
