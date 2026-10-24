/*
 * 基准程序的最小运行时：不依赖 libc，只用 write 和 exit_group 两个 syscall，
 * 这样 M0 的内核层就能运行它们（设计文档 §14 M2）。
 */
#ifndef BENCH_RT_H
#define BENCH_RT_H

#include <stddef.h>
#include <stdint.h>

int main(int argc, char **argv);

long rt_write(int fd, const void *buf, size_t len);
_Noreturn void rt_exit(int code);

void rt_print(const char *s);
void rt_print_u64(uint64_t v);
void rt_print_hex(const void *p, size_t n);
_Noreturn void rt_fail(const char *what);

/* argv[index] 的十进制值；没有这个参数时返回 fallback */
uint64_t rt_arg_u64(int argc, char **argv, int index, uint64_t fallback);

/* 编译器可能生成对这些函数的调用 */
void *memcpy(void *dst, const void *src, size_t n);
void *memmove(void *dst, const void *src, size_t n);
void *memset(void *dst, int c, size_t n);
int memcmp(const void *a, const void *b, size_t n);

#endif /* BENCH_RT_H */
