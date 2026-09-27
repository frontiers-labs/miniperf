#!/usr/bin/env python3
"""Read and regenerate deps/manifest.toml.

`get` resolves a dotted key for shell callers. `write` rebuilds the manifest
from the current upstream pins, optional `--set` overrides, and the `.meta`
sidecars that the dependency build scripts drop next to each artifact.
`matrix` renders the [support] table as a GitHub Actions build matrix and
`check` fails when the built artifacts do not cover it, so the matrix and the
completeness gate cannot drift apart.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
import tomllib

DEFAULT_MANIFEST = pathlib.Path(__file__).resolve().parents[2] / "deps" / "manifest.toml"


def load(path: pathlib.Path) -> dict:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def resolve(manifest: dict, key: str):
    value = manifest
    for part in key.split("."):
        if not isinstance(value, dict) or part not in value:
            raise SystemExit(f"deps/manifest.toml has no key '{key}'")
        value = value[part]
    return value


def read_meta(path: pathlib.Path) -> dict[str, str]:
    fields = {}
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        name, _, value = line.partition("=")
        fields[name.strip()] = value.strip()
    missing = {"dependency", "platform", "file"} - fields.keys()
    if missing:
        raise SystemExit(f"{path} is missing required fields: {', '.join(sorted(missing))}")
    return fields


def collect_artifacts(directory: pathlib.Path) -> dict[str, dict[str, dict[str, str]]]:
    artifacts: dict[str, dict[str, dict[str, str]]] = {}
    for meta_path in sorted(directory.rglob("*.meta")):
        meta = read_meta(meta_path)
        archive = meta_path.parent / meta["file"]
        checksum_path = archive.with_name(archive.name + ".sha256")
        if not archive.is_file():
            raise SystemExit(f"{meta_path} references a missing artifact: {archive}")
        if not checksum_path.is_file():
            raise SystemExit(f"{archive} has no .sha256 next to it")
        checksum = checksum_path.read_text().split()[0]
        entry = {"file": meta["file"], "sha256": checksum}
        previous = artifacts.setdefault(meta["dependency"], {}).get(meta["platform"])
        if previous is not None and previous != entry:
            raise SystemExit(
                f"conflicting artifacts for {meta['dependency']}/{meta['platform']}: "
                f"{previous['file']} and {entry['file']}"
            )
        artifacts[meta["dependency"]][meta["platform"]] = entry
    return artifacts


def quote(value: str) -> str:
    if '"' in value or "\\" in value:
        raise SystemExit(f"refusing to emit a TOML value needing escapes: {value!r}")
    return f'"{value}"'


RUNNERS = {
    "linux-x86_64": "ubuntu-24.04",
    "linux-aarch64": "ubuntu-24.04-arm",
    "linux-riscv64": "ubuntu-24.04",
    "macos-aarch64": "macos-14",
    "windows-x86_64": "windows-2022",
}


def render_support(support: dict) -> list[str]:
    lines = [
        "# Platforms each dependency must build for. deps.yml generates its build",
        "# matrix from this table and fails the run when the published release does",
        "# not cover it, so a partial build can never be published. DynamoRIO has no",
        "# arm64 macOS port; qemu-user targets Linux hosts only.",
        "[support]",
    ]
    for dependency in sorted(support):
        platforms = ", ".join(quote(platform) for platform in support[dependency])
        lines.append(f"{dependency} = [{platforms}]")
    return lines


def missing_artifacts(support: dict, artifacts: dict) -> list[str]:
    return [
        f"{dependency}/{platform}"
        for dependency in sorted(support)
        for platform in support[dependency]
        if platform not in artifacts.get(dependency, {})
    ]


def render(upstream: dict, release: str, artifacts: dict, support: dict) -> str:
    lines = [
        "# Pinned external binary dependencies.",
        "#",
        "# `.github/workflows/deps.yml` builds every dependency listed under [upstream]",
        "# for every platform that supports it, publishes the results as a dated",
        "# prerelease, and opens a pull request rewriting this file with the new",
        "# `release` tag and `[artifacts]` table. Build scripts and packaging read the",
        "# pins from here; nothing downloads “latest”.",
        "#",
        "# DuckDB's version must match the DuckDB that the pinned `duckdb` crate",
        "# generated its bindings from, because miniperf-store links the prebuilt",
        "# library against those pregenerated bindings. Bump both together.",
        "",
        f"release = {quote(release)}",
    ]
    for name in sorted(upstream):
        lines.append("")
        lines.append(f"[upstream.{name}]")
        for key in sorted(upstream[name]):
            lines.append(f"{key} = {quote(str(upstream[name][key]))}")

    lines.append("")
    lines.extend(render_support(support))

    lines.append("")
    lines.append("# Built by the dependency workflow; every platform in [support] must be")
    lines.append("# present here or the run fails without publishing.")
    lines.append("[artifacts]")
    for dependency in sorted(artifacts):
        for platform in sorted(artifacts[dependency]):
            entry = artifacts[dependency][platform]
            lines.append("")
            lines.append(f"[artifacts.{dependency}.{quote(platform)}]")
            lines.append(f"file = {quote(entry['file'])}")
            lines.append(f"sha256 = {quote(entry['sha256'])}")
    return "\n".join(lines) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=pathlib.Path, default=DEFAULT_MANIFEST)
    subcommands = parser.add_subparsers(dest="command", required=True)

    get = subcommands.add_parser("get", help="print a dotted key")
    get.add_argument("key")

    write = subcommands.add_parser("write", help="regenerate the manifest")
    write.add_argument("--release", required=True)
    write.add_argument("--artifacts", type=pathlib.Path, required=True)
    write.add_argument(
        "--set",
        action="append",
        default=[],
        metavar="upstream.qemu.version=11.1.0",
        help="override an upstream pin before rendering",
    )
    write.add_argument("--output", type=pathlib.Path)

    matrix = subcommands.add_parser("matrix", help="print the build matrix as JSON")
    matrix.add_argument("--dependency", help="restrict the matrix to one dependency")

    check = subcommands.add_parser("check", help="fail when [support] is not covered")
    check.add_argument(
        "--artifacts",
        type=pathlib.Path,
        help="verify freshly built .meta sidecars instead of the manifest's own table",
    )

    arguments = parser.parse_args()
    manifest = load(arguments.manifest)

    support = manifest.get("support", {})

    if arguments.command == "get":
        value = resolve(manifest, arguments.key)
        if isinstance(value, dict):
            raise SystemExit(f"'{arguments.key}' is a table, not a value")
        print(value)
        return

    if arguments.command == "matrix":
        include = [
            {
                "dependency": dependency,
                "platform": platform,
                "runner": RUNNERS[platform],
                "cross": "riscv64" if platform == "linux-riscv64" else "",
            }
            for dependency in sorted(support)
            for platform in support[dependency]
            if arguments.dependency in (None, dependency)
        ]
        unknown = {entry["platform"] for entry in include} - RUNNERS.keys()
        if unknown:
            raise SystemExit(f"no runner mapped for: {', '.join(sorted(unknown))}")
        print(json.dumps({"include": include}))
        return

    if arguments.command == "check":
        if arguments.artifacts:
            artifacts = collect_artifacts(arguments.artifacts)
        else:
            artifacts = manifest.get("artifacts", {})
        missing = missing_artifacts(support, artifacts)
        for dependency in sorted(support):
            for platform in support[dependency]:
                built = platform in artifacts.get(dependency, {})
                print(f"{'ok  ' if built else 'MISS'} {dependency}/{platform}")
        if missing:
            raise SystemExit(
                f"\n{len(missing)} dependency build(s) missing: {', '.join(missing)}"
            )
        print(f"\nall {sum(len(p) for p in support.values())} dependency builds present")
        return

    upstream = manifest.get("upstream", {})
    for override in arguments.set:
        key, separator, value = override.partition("=")
        if not separator:
            raise SystemExit(f"--set expects key=value, got {override!r}")
        parts = key.split(".")
        if len(parts) != 3 or parts[0] != "upstream":
            raise SystemExit(f"--set only supports upstream.<dependency>.<field>, got {key!r}")
        if parts[1] not in upstream:
            raise SystemExit(f"unknown dependency in --set: {parts[1]}")
        upstream[parts[1]][parts[2]] = value

    artifacts = collect_artifacts(arguments.artifacts)
    missing = missing_artifacts(support, artifacts)
    if missing:
        raise SystemExit(
            f"refusing to write a manifest missing {len(missing)} build(s): "
            f"{', '.join(missing)}"
        )
    rendered = render(upstream, arguments.release, artifacts, support)
    if arguments.output:
        arguments.output.write_text(rendered)
    else:
        sys.stdout.write(rendered)


if __name__ == "__main__":
    main()
