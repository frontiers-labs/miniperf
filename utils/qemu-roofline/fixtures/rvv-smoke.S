.section .rodata
.align 3
mask:
    .byte 0x5

.section .text
.globl _start
_start:
    li t0, 4
    vsetvli t0, t0, e32, m1, ta, ma

    vadd.vv v1, v2, v3

    la t1, mask
    vlm.v v0, (t1)
    vadd.vv v1, v2, v3, v0.t

    vfadd.vv v1, v2, v3
    vfmacc.vv v1, v2, v3
    vmfeq.vv v4, v2, v3
    vfcvt.x.f.v v4, v2

    vsetvli t0, t0, e64, m1, ta, ma
    vfadd.vv v1, v2, v3

    sub a0, a0, a0
    li a7, 93
    ecall
