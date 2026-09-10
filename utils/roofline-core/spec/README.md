# Vendored TIR RISC-V specification

`riscv-ast-v1.json` is the compact form of TIR's version 1 checked-AST JSON
for the complete RISC-V backend input set. `TIR_REVISION` records the exact
upstream revision, and `LICENSE` contains TIR's Apache-2.0 license.

Refresh all three files from a clean TIR checkout:

```sh
utils/roofline-core/update-tmdl-spec.sh /path/to/tir
```

The QEMU plugin parses the JSON only while building and generates a static
operation table. The vendored document is not present in the plugin binary.
