.section .data
.balign 16
input:
    .double 1.0, 2.0
output:
    .zero 16

.section .text
.globl _start
_start:
    movapd input(%rip), %xmm0
    addpd %xmm0, %xmm0
    mulpd %xmm0, %xmm0
    movapd %xmm0, output(%rip)

    xorl %edi, %edi
    movl $60, %eax
    syscall
