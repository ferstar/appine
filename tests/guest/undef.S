// 未定义指令：guest 应当以 SIGILL 终止。
    .text
    .globl  _start
_start:
    udf     #0
