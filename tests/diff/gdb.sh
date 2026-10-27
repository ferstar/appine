# gdb：断点、单步、看变量、软件和硬件观察点、硬件断点、线程。两边的加载地址和 pid 不同，
# 归一化之后比
set -e
cd /tmp
cat > loop.c <<'C'
#include <stdio.h>

static int square(int x)
{
    int y = x * x;
    return y;
}

int counter;

int main(void)
{
    int total = 0;
    for (int i = 1; i <= 3; i++) {
        counter++;
        total += square(i);
    }
    printf("total %d\n", total);
    return 0;
}
C
cat > threads.c <<'C'
#include <pthread.h>
#include <stdio.h>

static int done;

static void *worker(void *arg)
{
    done = *(int *)arg;
    return 0;
}

int main(void)
{
    pthread_t t;
    int v = 7;
    pthread_create(&t, 0, worker, &v);
    pthread_join(t, 0);
    printf("done %d\n", done);
    return 0;
}
C
gcc -g -O0 -o loop loop.c
gcc -g -O0 -o threads threads.c -lpthread
norm() {
    sed -E 's/0x[0-9a-f]+/ADDR/g; s/(process|LWP|Thread) [0-9]+/\1 N/g; s/^\[Inferior 1 \(process N\)/[Inferior 1/'
}
# 软件观察点（逐条单步之后比较）
gdb -batch -nx -ex 'set can-use-hw-watchpoints 0' -ex 'break square' -ex run -ex bt -ex 'print x' \
    -ex next -ex next -ex 'print y' -ex finish -ex 'watch counter' -ex continue -ex continue -ex delete \
    -ex stepi -ex 'x/2i $pc' -ex 'print $w0' -ex 'print $pc == &square + 4' -ex continue ./loop 2>&1 | norm
# square 先执行过（Tier 1 已经翻译了它），之后再下断点
gdb -batch -nx -ex 'break main' -ex run -ex next -ex next -ex next -ex next -ex 'break square' -ex continue \
    -ex bt -ex 'delete 2' -ex continue ./loop 2>&1 | norm
# 不打印线程的创建和退出：程序最后的输出和 gdb 报告线程退出谁先谁后不一定
gdb -batch -nx -ex 'set print thread-events off' -ex 'break worker' -ex run -ex 'info threads' \
    -ex 'print *(int *)arg' -ex finish -ex continue ./threads 2>&1 | norm
# 停在系统调用上（PTRACE_SYSCALL）
gdb -batch -nx -ex 'catch syscall writev' -ex run -ex 'info registers x8' -ex continue -ex continue ./loop 2>&1 | norm
# 硬件观察点（写、不对齐的两个字节、读、读写）和硬件断点，另一个线程触发观察点。appine 的
# 只有解释器检查，有它们的线程只用 Tier 0 执行。最后 kill：跑到结束的话，程序最后的输出和
# gdb 报告线程退出谁先谁后不一定
cat > watch.c <<'C'
#include <pthread.h>
#include <stdio.h>

struct rec {
    char tag;
    short half;
    int value;
} rec;
volatile int counter;
int reads = 5;

static int work(int i)
{
    rec.half = i;
    return reads;
}

static void *thread(void *arg)
{
    (void)arg;
    counter += 100;
    return 0;
}

int main(void)
{
    int sum = 0;
    for (int i = 0; i < 3; i++) {
        counter += 2;
        sum += work(i);
    }
    pthread_t t;
    pthread_create(&t, 0, thread, 0);
    pthread_join(t, 0);
    printf("%d %d %d\n", sum, rec.half, counter);
    return 0;
}
C
gcc -g -O0 -o watch watch.c -lpthread
gdb -batch -nx -ex 'break main' -ex run -ex 'watch counter' -ex continue -ex continue -ex 'delete 2' \
    -ex 'watch rec.half' -ex continue -ex 'delete 3' -ex 'rwatch reads' -ex continue -ex 'delete 4' \
    -ex 'awatch rec.tag' -ex 'info watchpoints' -ex 'delete 5' -ex 'hbreak work' -ex continue -ex bt -ex 'delete 6' \
    -ex 'watch counter' -ex continue -ex 'info threads' -ex kill ./watch 2>&1 | norm
