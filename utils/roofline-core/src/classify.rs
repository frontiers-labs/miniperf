//! Instruction classification shared by the QEMU plugin and the DynamoRIO
//! client. RISC-V classification is generated from the vendored TMDL spec at
//! build time; x86 and aarch64 use mnemonic rules.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Target {
    Riscv,
    X86,
    Aarch64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RvvKind {
    Integer,
    Float,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RiscvKind {
    ScalarInteger,
    ScalarFloat,
    ScalarDouble,
    VectorInteger,
    VectorFloat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RiscvCost {
    pub kind: RiscvKind,
    pub factor: u64,
    pub sew_scale: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RiscvClassification {
    Counted(RiscvCost),
    NonCompute,
    Unclassified,
}

pub struct RiscvOperationSpec {
    mnemonic: &'static str,
    masked: bool,
    classification: RiscvClassification,
}

include!(concat!(env!("OUT_DIR"), "/riscv_operations.rs"));

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BlockCost {
    pub vaddr: u64,
    pub end_vaddr: u64,
    pub flow: FlowKind,
    pub scalar_int: u64,
    pub scalar_float: u64,
    pub scalar_double: u64,
    pub vector_int: u64,
    pub vector_float: u64,
    pub vector_double: u64,
    pub instructions: u64,
}

impl BlockCost {
    pub fn add(&mut self, other: Self) {
        self.scalar_int += other.scalar_int;
        self.scalar_float += other.scalar_float;
        self.scalar_double += other.scalar_double;
        self.vector_int += other.vector_int;
        self.vector_float += other.vector_float;
        self.vector_double += other.vector_double;
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FlowKind {
    #[default]
    Normal,
    Call,
    Return,
}

pub fn classify_flow(target: Option<Target>, disassembly: &str) -> FlowKind {
    let operation = mnemonic(disassembly);
    let operands = disassembly
        .to_ascii_lowercase()
        .split_once(&operation)
        .map(|(_, operands)| operands.trim().replace(' ', ""))
        .unwrap_or_default();

    match target {
        Some(Target::Riscv) => {
            if matches!(operation.as_str(), "ret" | "c.ret")
                || matches!(operation.as_str(), "jr" | "c.jr")
                    && matches!(operands.as_str(), "ra" | "x1")
                || operation == "jalr"
                    && (operands.starts_with("zero,ra") || operands.starts_with("x0,x1"))
            {
                FlowKind::Return
            } else if operation == "call"
                || matches!(operation.as_str(), "jal" | "jalr")
                    && (operands.starts_with("ra,") || operands.starts_with("x1,"))
                || operation == "c.jal"
                || operation == "c.jalr"
            {
                FlowKind::Call
            } else {
                FlowKind::Normal
            }
        }
        Some(Target::X86) if operation.starts_with("ret") => FlowKind::Return,
        Some(Target::X86) if operation.starts_with("call") => FlowKind::Call,
        Some(Target::Aarch64) => match operation.as_str() {
            "ret" => FlowKind::Return,
            "bl" | "blr" => FlowKind::Call,
            _ => FlowKind::Normal,
        },
        _ => FlowKind::Normal,
    }
}

pub fn mnemonic(disassembly: &str) -> String {
    disassembly
        .split_whitespace()
        .find(|part| {
            part.chars().any(|c| c.is_ascii_alphabetic())
                && !part.trim_end_matches(':').starts_with("0x")
        })
        .unwrap_or_default()
        .trim_matches(|c: char| c == ':' || c == ',')
        .to_ascii_lowercase()
}

pub fn classify_riscv(disassembly: &str) -> RiscvClassification {
    let mnemonic = mnemonic(disassembly);
    let masked = is_masked(disassembly);
    RISCV_OPERATIONS
        .binary_search_by(|operation| {
            operation
                .mnemonic
                .cmp(mnemonic.as_str())
                .then_with(|| operation.masked.cmp(&masked))
        })
        .ok()
        .map(|index| RISCV_OPERATIONS[index].classification)
        .unwrap_or(RiscvClassification::Unclassified)
}

pub fn is_masked(disassembly: &str) -> bool {
    disassembly.to_ascii_lowercase().contains("v0.t")
}

pub fn classify_x86(mnemonic: &str, disassembly: &str) -> BlockCost {
    let mut cost = BlockCost::default();
    let opcode = mnemonic.strip_prefix('v').unwrap_or(mnemonic);
    let fused = is_x86_fused(opcode);
    let operations = if fused { 2 } else { 1 };

    if is_x86_float_arithmetic(opcode) {
        if opcode.ends_with("ss") {
            cost.scalar_float = operations;
        } else if opcode.ends_with("sd") {
            cost.scalar_double = operations;
        } else if opcode.ends_with("ps") {
            cost.vector_float = operations * x86_vector_bits(disassembly) / 32;
        } else if opcode.ends_with("pd") {
            cost.vector_double = operations * x86_vector_bits(disassembly) / 64;
        } else if opcode.starts_with('f') {
            // x87 uses extended precision internally. Keep it in the closest
            // existing bucket until the event schema has an explicit type.
            cost.scalar_double = operations;
        }
    } else if let Some(element_bits) = x86_vector_integer_element_bits(opcode) {
        cost.vector_int = operations * x86_vector_bits(disassembly) / element_bits;
    } else if is_x86_scalar_integer(opcode) {
        cost.scalar_int = 1;
    }

    cost
}

fn is_x86_fused(opcode: &str) -> bool {
    ["fmadd", "fmsub", "fnmadd", "fnmsub"]
        .iter()
        .any(|prefix| opcode.starts_with(prefix))
}

fn is_x86_float_arithmetic(opcode: &str) -> bool {
    [
        "add", "sub", "mul", "div", "sqrt", "min", "max", "cmp", "comi", "ucomi", "fmadd", "fmsub",
        "fnmadd", "fnmsub",
    ]
    .iter()
    .any(|prefix| opcode.starts_with(prefix))
        && ["ss", "sd", "ps", "pd"]
            .iter()
            .any(|suffix| opcode.ends_with(suffix))
        || [
            "fadd", "faddp", "fsub", "fsubp", "fsubr", "fmul", "fdiv", "fdivp", "fdivr", "fsqrt",
            "fcom", "fcomp", "fucom", "fucomp",
        ]
        .contains(&opcode)
}

fn x86_vector_bits(disassembly: &str) -> u64 {
    if disassembly.contains("zmm") {
        512
    } else if disassembly.contains("ymm") {
        256
    } else {
        128
    }
}

fn x86_vector_integer_element_bits(opcode: &str) -> Option<u64> {
    let vector_integer = [
        "padd", "psub", "pmul", "pmadd", "pavg", "pmin", "pmax", "pcmpeq", "pcmpgt", "psll",
        "psrl", "psra",
    ]
    .iter()
    .any(|prefix| opcode.starts_with(prefix));
    if !vector_integer {
        return None;
    }

    match opcode.chars().last()? {
        'b' => Some(8),
        'w' => Some(16),
        'd' => Some(32),
        'q' => Some(64),
        _ => None,
    }
}

fn is_x86_scalar_integer(opcode: &str) -> bool {
    let opcodes = [
        "add", "adc", "sub", "sbb", "imul", "mul", "idiv", "div", "inc", "dec", "neg", "shl",
        "shr", "sar", "sal", "and", "or", "xor", "cmp", "test",
    ];
    opcodes.contains(&opcode)
        || opcode
            .strip_suffix(['b', 'w', 'l', 'q'])
            .is_some_and(|opcode| opcodes.contains(&opcode))
}

/// NEON element arrangement. Handles both the binutils style ("v0.2d",
/// "%q0.4s") and DynamoRIO's style, where vector operands are %q registers
/// followed by a `$0x0N` element-size immediate with N = log2(element
/// bytes), e.g. "fmul %q31 %q29 $0x03 -> %q31". Returns
/// (elements, element_bits).
fn aarch64_arrangement(disassembly: &str) -> Option<(u64, u64)> {
    let lower = disassembly.to_ascii_lowercase();
    for (needle, elements, bits) in [
        (".2d", 2_u64, 64_u64),
        (".4s", 4, 32),
        (".2s", 2, 32),
        (".8h", 8, 16),
        (".4h", 4, 16),
        (".16b", 16, 8),
        (".8b", 8, 8),
    ] {
        if lower.contains(needle) {
            return Some((elements, bits));
        }
    }
    if lower.contains("%q") {
        let size = lower
            .split_whitespace()
            .filter_map(|part| part.strip_prefix("$0x"))
            .next_back()
            .and_then(|value| u32::from_str_radix(value, 16).ok())?;
        if size <= 3 {
            let bits = 8_u64 << size;
            return Some((128 / bits, bits));
        }
    }
    None
}

fn aarch64_scalar_fp_bits(disassembly: &str) -> u64 {
    // Scalar FP operands are d/s/h registers; the widest one decides.
    let lower = disassembly.to_ascii_lowercase();
    for part in lower.split(|c: char| !c.is_ascii_alphanumeric() && c != '%') {
        let part = part.trim_start_matches('%');
        if part.len() >= 2 && part.starts_with('d') && part[1..].chars().all(|c| c.is_ascii_digit())
        {
            return 64;
        }
    }
    32
}

pub fn classify_aarch64(mnemonic: &str, disassembly: &str) -> BlockCost {
    let mut cost = BlockCost::default();
    let fused = [
        "fmla", "fmls", "fmadd", "fmsub", "fnmadd", "fnmsub", "fmlal", "fmlsl", "mla", "mls",
        "madd", "msub",
    ]
    .contains(&mnemonic);
    let operations = if fused { 2 } else { 1 };

    let float_arith = [
        "fadd", "fsub", "fmul", "fdiv", "fsqrt", "fmin", "fmax", "fmla", "fmls", "fmadd", "fmsub",
        "fnmadd", "fnmsub", "fnmul", "fabd", "faddp", "fmulx",
    ]
    .contains(&mnemonic);
    let int_arith = [
        "add", "sub", "mul", "mla", "mls", "madd", "msub", "smull", "umull", "sdiv", "udiv", "neg",
        "and", "orr", "eor", "lsl", "lsr", "asr", "adc", "sbc", "addp",
    ]
    .contains(&mnemonic);

    if float_arith {
        if let Some((elements, bits)) = aarch64_arrangement(disassembly) {
            if bits == 64 {
                cost.vector_double = operations * elements;
            } else {
                cost.vector_float = operations * elements;
            }
        } else if aarch64_scalar_fp_bits(disassembly) == 64 {
            cost.scalar_double = operations;
        } else {
            cost.scalar_float = operations;
        }
    } else if int_arith {
        if let Some((elements, _)) = aarch64_arrangement(disassembly) {
            cost.vector_int = operations * elements;
        } else {
            cost.scalar_int = 1;
        }
    }

    cost
}

pub fn active_elements(start: u64, end: u64, mask: Option<&[u8]>) -> Option<u64> {
    let Some(mask) = mask else {
        return Some(end.saturating_sub(start));
    };
    if end > mask.len() as u64 * 8 {
        return None;
    }
    Some(
        (start..end)
            .filter(|bit| mask[(bit / 8) as usize] & (1 << (bit % 8)) != 0)
            .count() as u64,
    )
}

pub fn rvv_sew(vtype: u64, xlen: u32) -> Option<u64> {
    if xlen == 0 || xlen > 64 || vtype & (1_u64 << (xlen - 1)) != 0 {
        return None;
    }
    8_u64.checked_shl(((vtype >> 3) & 0x7) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_call_and_return_edges() {
        for instruction in [
            "jal ra,0x20",
            "jalr x1,a0,0",
            "call 0x40",
            "c.jal 0x10",
            "c.jalr ra",
        ] {
            assert_eq!(
                classify_flow(Some(Target::Riscv), instruction),
                FlowKind::Call,
                "{instruction}"
            );
        }
        for instruction in ["ret", "jr ra", "jalr zero,ra,0", "c.jr x1"] {
            assert_eq!(
                classify_flow(Some(Target::Riscv), instruction),
                FlowKind::Return,
                "{instruction}"
            );
        }
        assert_eq!(
            classify_flow(Some(Target::Riscv), "jal zero,0x20"),
            FlowKind::Normal
        );
        assert_eq!(
            classify_flow(Some(Target::X86), "callq 0x20"),
            FlowKind::Call
        );
        assert_eq!(classify_flow(Some(Target::X86), "retq"), FlowKind::Return);
        assert_eq!(
            classify_flow(Some(Target::Aarch64), "bl 0x20"),
            FlowKind::Call
        );
        assert_eq!(
            classify_flow(Some(Target::Aarch64), "ret"),
            FlowKind::Return
        );
    }

    #[test]
    fn classifies_tmdl_scalar_and_compressed_operations() {
        assert_eq!(
            classify_riscv("fadd.d fa0,fa1,fa2"),
            RiscvClassification::Counted(RiscvCost {
                kind: RiscvKind::ScalarDouble,
                factor: 1,
                sew_scale: 1,
            })
        );
        assert_eq!(
            classify_riscv("c.add a0,a1"),
            RiscvClassification::Counted(RiscvCost {
                kind: RiscvKind::ScalarInteger,
                factor: 1,
                sew_scale: 1,
            })
        );
    }

    #[test]
    fn classifies_tmdl_vector_arithmetic() {
        assert_eq!(
            classify_riscv("vfmacc.vv v8,v9,v10,v0.t"),
            RiscvClassification::Counted(RiscvCost {
                kind: RiscvKind::VectorFloat,
                factor: 2,
                sew_scale: 1,
            })
        );
        assert_eq!(
            classify_riscv("vfwadd.vv v8,v9,v10"),
            RiscvClassification::Counted(RiscvCost {
                kind: RiscvKind::VectorFloat,
                factor: 1,
                sew_scale: 2,
            })
        );
        assert_eq!(
            classify_riscv("vfredusum.vs v8,v9,v10"),
            RiscvClassification::Counted(RiscvCost {
                kind: RiscvKind::VectorFloat,
                factor: 1,
                sew_scale: 1,
            })
        );
        assert_eq!(
            classify_riscv("vmacc.vv v8,v9,v10"),
            RiscvClassification::Counted(RiscvCost {
                kind: RiscvKind::VectorInteger,
                factor: 2,
                sew_scale: 1,
            })
        );
    }

    #[test]
    fn does_not_count_control_flow_or_expand_integer_remainder() {
        for instruction in [
            "auipc a0,0x10",
            "jal ra,0x20",
            "jalr ra,a0,0",
            "csrrw a0,fcsr,a1",
            "csrrwi a0,fcsr,1",
        ] {
            assert_eq!(
                classify_riscv(instruction),
                RiscvClassification::NonCompute,
                "{instruction}"
            );
        }
        assert_eq!(
            classify_riscv("rem a0,a1,a2"),
            RiscvClassification::Counted(RiscvCost {
                kind: RiscvKind::ScalarInteger,
                factor: 1,
                sew_scale: 1,
            })
        );
    }

    #[test]
    fn treats_non_arithmetic_float_behavior_as_non_compute() {
        for instruction in [
            "vmfeq.vv v1,v2,v3",
            "vfcvt.x.f.v v1,v2",
            "vfsgnj.vv v1,v2,v3",
        ] {
            assert_eq!(
                classify_riscv(instruction),
                RiscvClassification::NonCompute,
                "{instruction}"
            );
        }
    }

    #[test]
    fn classifies_scalar_float_variants_and_local_results() {
        for instruction in ["fmadd.d fa0,fa1,fa2,fa3", "fmadd.d fa0,fa1,fa2,fa3,rne"] {
            assert_eq!(
                classify_riscv(instruction),
                RiscvClassification::Counted(RiscvCost {
                    kind: RiscvKind::ScalarDouble,
                    factor: 2,
                    sew_scale: 1,
                })
            );
        }
        for instruction in ["fsgnj.s fa0,fa1,fa2", "fsgnj.d fa0,fa1,fa2"] {
            assert_eq!(classify_riscv(instruction), RiscvClassification::NonCompute);
        }
    }

    #[test]
    fn leaves_missing_and_todo_operations_unclassified() {
        assert_eq!(
            classify_riscv("vfrsqrt7.v v1,v2"),
            RiscvClassification::Unclassified
        );
        assert_eq!(
            classify_riscv("madeup a0,a1"),
            RiscvClassification::Unclassified
        );
    }

    #[test]
    fn applies_rvv_mask_vstart_and_sew() {
        assert_eq!(active_elements(2, 7, Some(&[0b0110_1100])), Some(4));
        assert_eq!(active_elements(2, 7, None), Some(5));
        assert_eq!(active_elements(9, 7, None), Some(0));
        assert_eq!(active_elements(0, 9, Some(&[0xff])), None);
        assert_eq!(rvv_sew(2 << 3, 64), Some(32));
        assert_eq!(rvv_sew(3 << 3, 64), Some(64));
        assert_eq!(rvv_sew(1_u64 << 63, 64), None);
    }

    #[test]
    fn classifies_x86_scalar_and_vector_operations() {
        let scalar = classify_x86("mulsd", "mulsd 0x8(%rax),%xmm0");
        assert_eq!(scalar.scalar_double, 1);

        let vector = classify_x86("vfmadd132ps", "vfmadd132ps %ymm1,%ymm2,%ymm3");
        assert_eq!(vector.vector_float, 16);

        let integer = classify_x86("vpaddd", "vpaddd %zmm1,%zmm2,%zmm3");
        assert_eq!(integer.vector_int, 16);
    }

    #[test]
    fn does_not_treat_x86_simd_logic_as_scalar_integer_work() {
        let cost = classify_x86("orpd", "orpd %xmm1,%xmm0");
        assert_eq!(cost.scalar_int, 0);
    }

    #[test]
    fn classifies_aarch64_scalar_and_vector_operations() {
        let scalar = classify_aarch64("fmul", "fmul %d1 %d2 -> %d0");
        assert_eq!(scalar.scalar_double, 1);

        let scalar32 = classify_aarch64("fadd", "fadd %s1 %s2 -> %s0");
        assert_eq!(scalar32.scalar_float, 1);

        let vector = classify_aarch64("fmla", "fmla %q1.2d %q2.2d -> %q0.2d");
        assert_eq!(vector.vector_double, 4);

        let vector_int = classify_aarch64("add", "add %q1.4s %q2.4s -> %q0.4s");
        assert_eq!(vector_int.vector_int, 4);

        // DynamoRIO's operand style: element size as a $0x0N immediate.
        let dr_double = classify_aarch64("fmul", "fmul   %q31 %q29 $0x03 -> %q31");
        assert_eq!(dr_double.vector_double, 2);

        let dr_float = classify_aarch64("fadd", "fadd   %q31 %q30 $0x02 -> %q31");
        assert_eq!(dr_float.vector_float, 4);

        let dr_fused = classify_aarch64("fmla", "fmla   %q0 %q1 %q2 $0x03 -> %q0");
        assert_eq!(dr_fused.vector_double, 4);

        let logic = classify_aarch64("ldr", "ldr (%x0) -> %q1");
        assert_eq!(logic, BlockCost::default());
    }
}
