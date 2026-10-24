// lockstep 的原生执行跳板。
//
// 整段代码会被拷贝进一个 JIT 页，被测指令写在 lockstep_tramp_slot 处，
// 所以这里只能用位置无关、不引用外部符号的指令。被测指令不能读写 SP，
// 寄存器的保存与恢复全部经由栈完成。
//
//   void tramp(struct lockstep_state *st);   // st: x[31] @0, nzcv @248

    .text
    .p2align 2
    .globl  lockstep_tramp_start
    .globl  lockstep_tramp_slot
    .globl  lockstep_tramp_end

lockstep_tramp_start:
    stp     x29, x30, [sp, #-16]!
    stp     x27, x28, [sp, #-16]!
    stp     x25, x26, [sp, #-16]!
    stp     x23, x24, [sp, #-16]!
    stp     x21, x22, [sp, #-16]!
    stp     x19, x20, [sp, #-16]!
    stp     x0, xzr, [sp, #-16]!        // 保存 st，同时保持 16 字节对齐

    ldr     x1, [x0, #248]
    msr     nzcv, x1
    ldp     x2, x3, [x0, #16]
    ldp     x4, x5, [x0, #32]
    ldp     x6, x7, [x0, #48]
    ldp     x8, x9, [x0, #64]
    ldp     x10, x11, [x0, #80]
    ldp     x12, x13, [x0, #96]
    ldp     x14, x15, [x0, #112]
    ldp     x16, x17, [x0, #128]
    ldp     x18, x19, [x0, #144]
    ldp     x20, x21, [x0, #160]
    ldp     x22, x23, [x0, #176]
    ldp     x24, x25, [x0, #192]
    ldp     x26, x27, [x0, #208]
    ldp     x28, x29, [x0, #224]
    ldr     x30, [x0, #240]
    ldp     x0, x1, [x0]

lockstep_tramp_slot:
    nop                                 // 被测指令

    sub     sp, sp, #256
    stp     x0, x1, [sp, #0]
    stp     x2, x3, [sp, #16]
    stp     x4, x5, [sp, #32]
    stp     x6, x7, [sp, #48]
    stp     x8, x9, [sp, #64]
    stp     x10, x11, [sp, #80]
    stp     x12, x13, [sp, #96]
    stp     x14, x15, [sp, #112]
    stp     x16, x17, [sp, #128]
    stp     x18, x19, [sp, #144]
    stp     x20, x21, [sp, #160]
    stp     x22, x23, [sp, #176]
    stp     x24, x25, [sp, #192]
    stp     x26, x27, [sp, #208]
    stp     x28, x29, [sp, #224]
    mrs     x0, nzcv
    stp     x30, x0, [sp, #240]

    ldr     x0, [sp, #256]              // st
    mov     x1, #0
1:  ldr     x2, [sp, x1]
    str     x2, [x0, x1]
    add     x1, x1, #8
    cmp     x1, #256
    b.ne    1b

    add     sp, sp, #(256 + 16)
    ldp     x19, x20, [sp], #16
    ldp     x21, x22, [sp], #16
    ldp     x23, x24, [sp], #16
    ldp     x25, x26, [sp], #16
    ldp     x27, x28, [sp], #16
    ldp     x29, x30, [sp], #16
    ret
lockstep_tramp_end:

    .section .note.GNU-stack, "", %progbits
