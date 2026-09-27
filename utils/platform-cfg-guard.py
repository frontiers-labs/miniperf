#!/usr/bin/env python3
"""Fail when platform-conditional compilation leaks out of libprof.

`#[cfg(target_os = ...)]` and `#[cfg_attr(..., target_arch = ...)]` make every
target compile a different program, so a change that is green on one platform
routinely breaks another. The CLI keeps those selections in its dedicated
platform modules, while libprof owns the operating-system implementations.

`cfg!(target_os = ...)` still compiles both branches, but Windows policy in
shared CLI paths belongs in their platform modules. Guard those paths against
accidental Windows policy regressions as well.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
GUARDED = ("mperf", "mperf-gui", "store", "mperf-data")
ALLOWLIST = ROOT / "utils" / "platform-cfg-allowlist.txt"
# `#!` too: an inner attribute gates a whole module just as effectively.
ATTRIBUTE = re.compile(r"#!?\[\s*cfg(_attr)?\s*\(")
RUNTIME_CFG = re.compile(r"\bcfg!\s*\(")
COMMON_COMMANDS = (
    "mperf/src/stat.rs",
    "mperf/src/record.rs",
    "mperf/src/roofline/mod.rs",
    "mperf/src/postprocess/roofline.rs",
    "mperf/src/source.rs",
    "mperf/src/doctor.rs",
)
# `unix` and `windows` are the same leak spelled shorter, and target_family,
# target_env, target_vendor and target_pointer_width all pick a platform.
PLATFORM = re.compile(
    r"\b(unix|windows|target_(os|arch|family|env|vendor|pointer_width))\b"
)


def predicates(source, pattern):
    """Yield `(line, text)` for every balanced platform predicate in `source`.

    Attributes are matched by balancing parentheses rather than per line: a
    multi-line `#[cfg(all(\\n    target_os = "linux", ...))]` hides the
    platform predicate from any single-line pattern.
    """
    for match in pattern.finditer(source):
        depth = 0
        for end in range(match.end() - 1, len(source)):
            if source[end] == "(":
                depth += 1
            elif source[end] == ")":
                depth -= 1
                if depth == 0:
                    break
        else:
            continue
        yield source.count("\n", 0, match.start()) + 1, source[match.start() : end + 1]


def main():
    allowed = {
        line.split("#", 1)[0].strip()
        for line in ALLOWLIST.read_text(encoding="utf-8").splitlines()
        if line.split("#", 1)[0].strip()
    }
    hits = []
    for crate in GUARDED:
        for path in sorted((ROOT / crate).rglob("*.rs")):
            relative = path.relative_to(ROOT).as_posix()
            if relative in allowed:
                continue
            source = path.read_text(encoding="utf-8")
            hits += [
                f"{relative}:{line}: {' '.join(text.split())}"
                for line, text in predicates(source, ATTRIBUTE)
                if PLATFORM.search(text)
            ]

    for relative in COMMON_COMMANDS:
        source = (ROOT / relative).read_text(encoding="utf-8")
        hits += [
            f"{relative}:{line}: {' '.join(text.split())}"
            for line, text in predicates(source, RUNTIME_CFG)
            if 'target_os = "windows"' in text
        ]

    stale = sorted(entry for entry in allowed if not (ROOT / entry).is_file())
    for entry in stale:
        print(f"allowlisted file no longer exists: {entry}", file=sys.stderr)

    if hits:
        print(
            f"{len(hits)} misplaced platform predicate(s):\n",
            file=sys.stderr,
        )
        print("\n".join(hits), file=sys.stderr)
        print(
            "\nMove platform policy into a platform module or libprof. "
            "See CONTRIBUTING.md.",
            file=sys.stderr,
        )
    return 1 if hits or stale else 0


if __name__ == "__main__":
    sys.exit(main())
