/*
 * 一个很小的字节码解释器：switch 分发会编译成跳转表，产生大量间接分支，
 * 用来近似 python 这类解释器负载。值一律是无符号数，溢出时按模回绕。
 * 用法：vm [n]
 */
#include "rt.h"

enum {
    OP_PUSH, /* 后跟一个立即数 */
    OP_LOAD, /* 后跟局部变量编号 */
    OP_STORE,
    OP_ADD,
    OP_SUB,
    OP_MUL,
    OP_MOD,
    OP_LT,
    OP_JZ, /* 后跟目标地址 */
    OP_JMP,
    OP_XOR,
    OP_SHR,
    OP_HALT,
};

/* locals: 0 = i, 1 = acc, 2 = n；acc += ((i * i) % 7) ^ (acc >> 3)，i 从 0 到 n - 1 */
static const uint64_t program[] = {
    OP_PUSH, 0, OP_STORE, 0,                            /*  0: i = 0 */
    OP_PUSH, 0, OP_STORE, 1,                            /*  4: acc = 0 */
    OP_LOAD, 0, OP_LOAD, 2, OP_LT, OP_JZ, 43,           /*  8: while (i < n) */
    OP_LOAD, 0, OP_LOAD, 0, OP_MUL, OP_PUSH, 7, OP_MOD, /* 15:   (i * i) % 7 */
    OP_LOAD, 1, OP_PUSH, 3, OP_SHR, OP_XOR,             /* 23:   ^ (acc >> 3) */
    OP_LOAD, 1, OP_ADD, OP_STORE, 1,                    /* 29:   acc += ... */
    OP_LOAD, 0, OP_PUSH, 1, OP_ADD, OP_STORE, 0,        /* 34:   i++ */
    OP_JMP, 8,                                          /* 41 */
    OP_HALT,                                            /* 43 */
};

static uint64_t run(uint64_t n)
{
    const uint64_t *code = program;
    uint64_t stack[32], locals[3] = {0, 0, n};
    int sp = 0;
    size_t pc = 0;
    for (;;) {
        switch (code[pc++]) {
        case OP_PUSH: stack[sp++] = code[pc++]; break;
        case OP_LOAD: stack[sp++] = locals[code[pc++]]; break;
        case OP_STORE: locals[code[pc++]] = stack[--sp]; break;
        case OP_ADD: sp--; stack[sp - 1] += stack[sp]; break;
        case OP_SUB: sp--; stack[sp - 1] -= stack[sp]; break;
        case OP_MUL: sp--; stack[sp - 1] *= stack[sp]; break;
        case OP_MOD: sp--; stack[sp - 1] %= stack[sp]; break;
        case OP_LT: sp--; stack[sp - 1] = stack[sp - 1] < stack[sp]; break;
        case OP_JZ: {
            size_t target = (size_t)code[pc++];
            if (!stack[--sp])
                pc = target;
            break;
        }
        case OP_JMP: pc = (size_t)code[pc]; break;
        case OP_XOR: sp--; stack[sp - 1] ^= stack[sp]; break;
        case OP_SHR: sp--; stack[sp - 1] >>= stack[sp]; break;
        case OP_HALT: return locals[1];
        default: rt_fail("vm opcode");
        }
    }
}

int main(int argc, char **argv)
{
    if (run(10) != 20)
        rt_fail("vm self-test");
    uint64_t n = rt_arg_u64(argc, argv, 1, 100000);
    rt_print("vm ");
    rt_print_u64(run(n));
    rt_print("\n");
    return 0;
}
