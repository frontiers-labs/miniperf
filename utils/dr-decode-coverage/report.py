#!/usr/bin/env python3
"""Reports which instructions in a set of binaries DynamoRIO cannot decode.

DynamoRIO refuses to run a block containing an instruction it cannot decode:
the application takes SIGILL at that address instead. On a RISC-V host whose
userspace is built for a newer profile than DynamoRIO's decoder covers, that
kills every process under drrun. Run this against the binaries a scenario will
actually execute before blaming the tool that runs on top.

    report.py --probe ./dr_decode_probe /lib/riscv64-linux-gnu/libc.so.6 ./workload

Needs objdump for the target architecture on PATH (pass --objdump otherwise)
and the probe binary built for the host under test:

    cmake -S utils/dr-decode-coverage -B build -DDynamoRIO_DIR=<dr-build>/cmake
    cmake --build build
"""

import argparse
import collections
import re
import shutil
import subprocess
import sys

# "   13484:\t0ed75b33          \tczero.eqz\ts6,a4,a3"
DISASSEMBLY = re.compile(r"^\s*[0-9a-f]+:\t([0-9a-f]+)\s*\t([^\s]+)")


def instructions(objdump, path):
    """Yields (encoding, mnemonic) for every instruction objdump prints."""
    output = subprocess.run(
        [objdump, "-d", path],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    for line in output.splitlines():
        match = DISASSEMBLY.match(line)
        if match is not None:
            yield match.group(1), match.group(2)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", nargs="+")
    parser.add_argument("--probe", default="./dr_decode_probe")
    parser.add_argument("--objdump", default="objdump")
    parser.add_argument(
        "--all", action="store_true", help="list decodable mnemonics too"
    )
    args = parser.parse_args()

    if shutil.which(args.objdump) is None:
        sys.exit(f"{args.objdump} not found; pass --objdump")

    # One probe call for the whole corpus: encodings are deduplicated, and the
    # same encoding always carries the same mnemonic.
    mnemonics = {}
    sites = collections.Counter()
    for path in args.binaries:
        for encoding, mnemonic in instructions(args.objdump, path):
            mnemonics.setdefault(encoding, mnemonic)
            sites[encoding] += 1

    probe = subprocess.run(
        [args.probe],
        input="\n".join(mnemonics) + "\n",
        capture_output=True,
        text=True,
        check=True,
    ).stdout

    failed = collections.Counter()
    examples = {}
    decoded = collections.Counter()
    for line in probe.splitlines():
        fields = line.split("\t")
        encoding = f"{int(fields[0], 16):x}"
        # The probe strips leading zeros; match it back to the objdump text.
        for candidate in (encoding, encoding.zfill(4), encoding.zfill(8)):
            if candidate in mnemonics:
                encoding = candidate
                break
        mnemonic = mnemonics.get(encoding, "?")
        if fields[1] == "FAIL":
            failed[mnemonic] += sites[encoding]
            examples.setdefault(mnemonic, encoding)
        else:
            decoded[mnemonic] += sites[encoding]

    total = sum(sites.values())
    undecodable = sum(failed.values())
    print(f"{total} instructions, {len(mnemonics)} distinct encodings")
    print(f"{undecodable} instructions ({len(failed)} mnemonics) fail to decode")
    if failed:
        print()
        print(f"{'mnemonic':<20}{'sites':>8}  example")
        for mnemonic, count in failed.most_common():
            print(f"{mnemonic:<20}{count:>8}  {examples[mnemonic]}")
    if args.all:
        print()
        print(f"{'decoded mnemonic':<20}{'sites':>8}")
        for mnemonic, count in decoded.most_common():
            print(f"{mnemonic:<20}{count:>8}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
