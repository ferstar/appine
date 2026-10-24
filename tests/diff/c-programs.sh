# gcc 编译并运行：浮点、自动向量化、原子操作和线程、setjmp；再比对编译器的输出
set -e
cd /tmp
cat > diff1.c <<'C'
#include <math.h>
#include <pthread.h>
#include <setjmp.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static atomic_long counter;
static long plain;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static jmp_buf env;

static void *worker(void *arg)
{
    for (int i = 0; i < 100000; i++) {
        atomic_fetch_add(&counter, (long)arg);
        pthread_mutex_lock(&lock);
        plain += 1;
        pthread_mutex_unlock(&lock);
    }
    return NULL;
}

static int cmp(const void *a, const void *b)
{
    return *(const int *)a - *(const int *)b;
}

int main(void)
{
    double s = 0;
    for (int i = 1; i < 100000; i++)
        s += 1.0 / ((double)i * i);
    printf("%.17g %.17g %.17g %.17g\n", s, sqrt(2.0), exp(1.0), atan2(1, -1));
    float v[1024], acc = 0;
    for (int i = 0; i < 1024; i++)
        v[i] = sinf(i * 0.01f);
    for (int i = 0; i < 1024; i++)
        acc += v[i] * v[i];
    printf("%.9g\n", acc);
    int a[4096];
    for (int i = 0; i < 4096; i++)
        a[i] = (i * 7919) % 4096;
    qsort(a, 4096, sizeof(int), cmp);
    long sum = 0;
    for (int i = 0; i < 4096; i++)
        sum += (long)a[i] * i;
    printf("%ld\n", sum);
    char buf[300];
    memset(buf, 'a', sizeof buf);
    buf[299] = 0;
    printf("%zu %d\n", strlen(buf), memcmp(buf, buf + 1, 200));
    pthread_t t[4];
    for (long i = 0; i < 4; i++)
        pthread_create(&t[i], NULL, worker, (void *)(i + 1));
    for (int i = 0; i < 4; i++)
        pthread_join(t[i], NULL);
    printf("%ld %ld\n", (long)counter, plain);
    if (setjmp(env) == 0)
        longjmp(env, 42);
    printf("jumped\n");
    return 0;
}
C
gcc -O3 -o diff1 diff1.c -lm -pthread
./diff1
gcc -O2 -S -o - diff1.c | md5sum
