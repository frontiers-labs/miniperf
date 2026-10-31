#!/usr/bin/env python3
"""Keep the injected collector and shims independent of the analyzer database."""

import argparse
from collections import deque
import json
from pathlib import Path
import re
import subprocess
import sys


ROOT = Path(__file__).resolve().parent.parent
FORBIDDEN_NATIVE = re.compile(r"duckdb|miniperf[-_]store(?![-_]writer)", re.IGNORECASE)


def run(command):
    result = subprocess.run(command, check=True, capture_output=True, text=True)
    return result.stdout


def dependency_violations(metadata):
    packages = {package["id"]: package["name"] for package in metadata["packages"]}
    roots = [
        package for package in metadata["workspace_members"]
        if packages[package] == "miniperf-collector-core"
        or packages[package].startswith("miniperf-shim")
    ]
    if not any(packages[package] == "miniperf-collector-core" for package in roots):
        raise ValueError("Cargo metadata has no miniperf-collector-core workspace member")

    # Cargo resolves dependencies for every target unless --filter-platform is
    # supplied. Follow only normal edges; build tools and tests are not injected.
    graph = {
        node["id"]: [
            dependency["pkg"] for dependency in node["deps"]
            if any(kind["kind"] is None for kind in dependency["dep_kinds"])
        ]
        for node in metadata["resolve"]["nodes"]
    }
    violations = []
    for root in sorted(roots, key=packages.get):
        pending = deque([(root, [packages[root]])])
        visited = set()
        while pending:
            package, path = pending.popleft()
            if package in visited:
                continue
            visited.add(package)
            name = packages[package]
            if name == "miniperf-store" or "duckdb" in name:
                violations.append(" -> ".join(path))
                break
            pending.extend(
                (dependency, path + [packages[dependency]])
                for dependency in graph[package]
            )
    return roots, violations


def verify_artifact(platform, artifact):
    if not artifact.is_file():
        raise ValueError(f"collector artifact is missing: {artifact}")
    if platform.startswith("linux-"):
        commands = [
            ["readelf", "--dynamic", "--wide", str(artifact)],
            ["readelf", "--symbols", "--wide", str(artifact)],
        ]
    elif platform.startswith("macos-"):
        commands = [["otool", "-L", str(artifact)], ["nm", "-a", str(artifact)]]
    else:
        raise ValueError(f"unsupported collector platform: {platform}")
    for command in commands:
        for line in run(command).splitlines():
            # otool and nm can print the input filename as a heading.
            if line.strip() == f"{artifact}:":
                continue
            if FORBIDDEN_NATIVE.search(line):
                raise ValueError(f"{artifact} contains a database dependency or symbol: {line.strip()}")
    print(f"collector artifact isolation passed: {artifact}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", help="package platform, such as linux-aarch64 or macos-aarch64")
    parser.add_argument("--artifact", type=Path, help="collector shared library to inspect")
    args = parser.parse_args()
    if bool(args.platform) != bool(args.artifact):
        parser.error("--platform and --artifact must be supplied together")
    try:
        if args.artifact:
            verify_artifact(args.platform, args.artifact)
        else:
            metadata = json.loads(run([
                "cargo", "metadata", "--format-version", "1", "--locked",
                "--manifest-path", str(ROOT / "Cargo.toml"),
            ]))
            roots, violations = dependency_violations(metadata)
            if violations:
                raise ValueError("forbidden collector dependencies:\n  " + "\n  ".join(violations))
            print(f"collector dependency isolation passed ({len(roots)} crates)")
    except subprocess.CalledProcessError as error:
        print(f"collector isolation: {error.cmd[0]} failed:\n{error.stderr.strip()}", file=sys.stderr)
        return 1
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"collector isolation: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
