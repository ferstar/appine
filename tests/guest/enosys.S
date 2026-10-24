// 未实现的 syscall 必须返回 -ENOSYS 而不是让程序崩溃（设计文档 §7.5）。
// 返回 -38 时退出码为 0，否则为 1。
    .text
    .globl  _start
_start:
    mov     x8, #1000               // 不存在的 syscall 编号
    svc     #0
    cmn     x0, #38
    cset    x0, ne
    mov     x8, #93                 // __NR_exit
    svc     #0
