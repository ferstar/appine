# gcc 编译并运行：浮点、自动向量化、原子操作和线程、setjmp；再比对编译器的输出；
# 几个线程共用一个 fd 用文件位置读写、两个进程同时 O_APPEND 追加
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
cat > diff2.c <<'C'
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHUNK 4096
#define CHUNKS 256
#define RECORDS 1000

static int fd;
static unsigned char seen[CHUNKS];
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static int bad;

/* 每次 read 拿到的必须是一整块、而且没有别的线程拿过 */
static void *reader(void *arg)
{
    (void)arg;
    unsigned buf[CHUNK / 4];
    for (;;) {
        ssize_t n = read(fd, buf, CHUNK);
        if (n <= 0)
            break;
        pthread_mutex_lock(&lock);
        unsigned c = buf[0] / (CHUNK / 4);
        if (n != CHUNK || c >= CHUNKS || seen[c]++)
            bad++;
        for (int i = 0; i < CHUNK / 4; i++)
            if (buf[i] != c * (CHUNK / 4) + i)
                bad++;
        pthread_mutex_unlock(&lock);
    }
    return NULL;
}

static void *writer(void *arg)
{
    char rec[64];
    for (int i = 0; i < RECORDS; i++) {
        snprintf(rec, sizeof rec, "%ld:%04d%*s\n", (long)arg, i, 54, "");
        rec[63] = '\n';
        if (write(fd, rec, sizeof rec) != sizeof rec)
            bad++;
    }
    return NULL;
}

/* 文件里每个 (线程, 序号) 的记录各出现一次 */
static int count_records(const char *path, int writers)
{
    FILE *f = fopen(path, "r");
    static unsigned char got[8][RECORDS];
    memset(got, 0, sizeof got);
    char line[128];
    int n = 0;
    while (fgets(line, sizeof line, f)) {
        int w, i;
        if (sscanf(line, "%d:%d", &w, &i) == 2 && w >= 0 && w < writers && i >= 0 && i < RECORDS &&
            !got[w][i]++)
            n++;
    }
    fclose(f);
    return n;
}

int main(void)
{
    unsigned *data = malloc(CHUNK * CHUNKS);
    for (unsigned i = 0; i < CHUNK * CHUNKS / 4; i++)
        data[i] = i;
    fd = open("shared.bin", O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (write(fd, data, CHUNK * CHUNKS) != CHUNK * CHUNKS)
        return 1;
    lseek(fd, 0, SEEK_SET);
    pthread_t t[4];
    for (long i = 0; i < 4; i++)
        pthread_create(&t[i], NULL, reader, NULL);
    for (int i = 0; i < 4; i++)
        pthread_join(t[i], NULL);
    int chunks = 0;
    for (int c = 0; c < CHUNKS; c++)
        chunks += seen[c] == 1;
    printf("shared reads: %d chunks, %d bad\n", chunks, bad);
    close(fd);

    fd = open("shared.txt", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    for (long i = 0; i < 4; i++)
        pthread_create(&t[i], NULL, writer, (void *)i);
    for (int i = 0; i < 4; i++)
        pthread_join(t[i], NULL);
    printf("shared writes: size %ld, %d records, %d bad\n", (long)lseek(fd, 0, SEEK_END),
           count_records("shared.txt", 4), bad);
    close(fd);

    /* 两个进程各自打开、O_APPEND 追加 */
    unlink("append.txt");
    pid_t pid = fork();
    fd = open("append.txt", O_WRONLY | O_CREAT | O_APPEND, 0644);
    writer((void *)(long)(pid == 0));
    close(fd);
    if (pid == 0)
        _exit(bad != 0);
    int status;
    waitpid(pid, &status, 0);
    printf("appends: %d records, child %d\n", count_records("append.txt", 2), WEXITSTATUS(status));
    return 0;
}
C
gcc -O2 -o diff2 diff2.c -pthread
./diff2
