// 读未映射的地址：guest 应当以 SIGSEGV 终止。
    .text
    .globl  _start
_start:
    mov     x0, #0x1000
    ldr     x1, [x0]
    mov     x0, #0
    mov     x8, #93
    svc     #0
