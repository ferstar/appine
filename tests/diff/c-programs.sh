# gcc 编译并运行：浮点、自动向量化、原子操作和线程、setjmp；再比对编译器的输出；
# 几个线程共用一个 fd 用文件位置读写、两个进程同时 O_APPEND 追加；别的线程 chdir、
# 改名、setuid 之后 stat 立刻看到；共享文件映射里跨进程的 futex
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
cat > diff3.c <<'C'
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define ROUNDS 2000

/* 检查线程做完开头的检查之后置 1；主线程每轮改完加一（奇数），检查线程检查完加一（偶数） */
static atomic_int ready, turn;
static atomic_int done;
static atomic_int bad, stress_bad;
static ino_t ino;

static void wait_for(int v)
{
    while (atomic_load(&turn) < v)
        sched_yield();
}

/* 一开始当前目录是 start。第 i 轮：当前目录是 d<i%2>，里面有 in<i%2>；三个目录里都有
   which，长度各不相同；文件叫 n<i%2> */
static void *checker(void *arg)
{
    (void)arg;
    struct stat st;
    char p[64];
    bad += !(lstat("which", &st) == 0 && st.st_size == 9);
    atomic_store(&ready, 1);
    for (int i = 0; i < ROUNDS; i++) {
        wait_for(2 * i + 1);
        int k = i % 2;
        bad += !(lstat("which", &st) == 0 && st.st_size == k);
        snprintf(p, sizeof p, "in%d", k);
        bad += lstat(p, &st) != 0;
        snprintf(p, sizeof p, "in%d", !k);
        bad += !(lstat(p, &st) != 0 && errno == ENOENT);
        snprintf(p, sizeof p, "/tmp/fs3/n%d", k);
        bad += !(stat(p, &st) == 0 && st.st_size == 3 && st.st_ino == ino);
        snprintf(p, sizeof p, "../n%d", !k);
        bad += !(stat(p, &st) != 0 && errno == ENOENT);
        atomic_fetch_add(&turn, 1);
    }
    return NULL;
}

/* 不同步地一直 stat：拿到的总是同一个文件 */
static void *stress(void *arg)
{
    (void)arg;
    struct stat st;
    while (!atomic_load(&done)) {
        for (int k = 0; k < 2; k++) {
            char p[32];
            snprintf(p, sizeof p, "/tmp/fs3/n%d", k);
            if (stat(p, &st) == 0)
                stress_bad += st.st_size != 3 || st.st_ino != ino;
            else
                stress_bad += errno != ENOENT;
        }
    }
    return NULL;
}

static atomic_int phase;
static int helper_before, helper_after, helper_errno;

/* 先 stat 一次（填进缓存），等主线程 setuid 之后再 stat */
static void *helper(void *arg)
{
    (void)arg;
    struct stat st;
    helper_before = stat("/tmp/fs3/priv/x", &st);
    atomic_store(&phase, 1);
    while (atomic_load(&phase) < 2)
        sched_yield();
    helper_after = stat("/tmp/fs3/priv/x", &st);
    helper_errno = errno;
    return NULL;
}

static void put(const char *path, const char *data)
{
    FILE *f = fopen(path, "w");
    fputs(data, f);
    fclose(f);
}

int main(void)
{
    struct stat st;
    mkdir("/tmp/fs3", 0755);
    mkdir("/tmp/fs3/start", 0755);
    mkdir("/tmp/fs3/d0", 0755);
    mkdir("/tmp/fs3/d1", 0755);
    put("/tmp/fs3/start/which", "123456789");
    put("/tmp/fs3/d0/which", "");
    put("/tmp/fs3/d1/which", "1");
    close(creat("/tmp/fs3/d0/in0", 0644));
    close(creat("/tmp/fs3/d1/in1", 0644));
    unlink("/tmp/fs3/n0");
    put("/tmp/fs3/n1", "abc");
    chdir("/tmp/fs3/start");
    stat("/tmp/fs3/n1", &st);
    ino = st.st_ino;
    pthread_t c, s[2];
    pthread_create(&c, NULL, checker, NULL);
    for (int i = 0; i < 2; i++)
        pthread_create(&s[i], NULL, stress, NULL);
    while (!atomic_load(&ready))
        sched_yield();
    for (int i = 0; i < ROUNDS; i++) {
        int k = i % 2;
        char from[32], to[32];
        snprintf(from, sizeof from, "/tmp/fs3/n%d", !k);
        snprintf(to, sizeof to, "/tmp/fs3/n%d", k);
        bad += chdir(k ? "/tmp/fs3/d1" : "/tmp/fs3/d0") != 0;
        bad += rename(from, to) != 0;
        atomic_fetch_add(&turn, 1);
        wait_for(2 * i + 2);
    }
    atomic_store(&done, 1);
    pthread_join(c, NULL);
    for (int i = 0; i < 2; i++)
        pthread_join(s[i], NULL);
    printf("stat after changes: %d bad, stress %d bad\n", bad, stress_bad);

    /* 换成普通用户之后要检查搜索权限（musl 让每个线程都换） */
    mkdir("/tmp/fs3/priv", 0700);
    close(creat("/tmp/fs3/priv/x", 0644));
    pthread_create(&c, NULL, helper, NULL);
    while (atomic_load(&phase) < 1)
        sched_yield();
    if (setuid(65534) != 0)
        return 1;
    atomic_store(&phase, 2);
    pthread_join(c, NULL);
    printf("as root %d, after setuid %d %s\n", helper_before, helper_after,
           helper_after ? strerror(helper_errno) : "");
    return 0;
}
C
gcc -O2 -o diff3 diff3.c -pthread
./diff3
rm -rf /tmp/fs3
# 共享文件映射里的 futex：父子进程各自映射同一个文件，宿主地址不同，要按文件和偏移认出
# 是同一个 futex（不持 BKL 的 futex 只按宿主地址算键，要避开这种映射）
cat > diff4.c <<'C'
#define _GNU_SOURCE
#include <stdatomic.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define FUTEX_WAIT 0
#define FUTEX_WAKE 1

static long futex(atomic_int *addr, int op, int val, const struct timespec *ts)
{
    return syscall(SYS_futex, addr, op, val, ts, NULL, 0);
}

int main(void)
{
    int fd = memfd_create("futex", 0);
    if (fd < 0 || ftruncate(fd, 8192) != 0)
        return 1;
    atomic_int *a = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (a == MAP_FAILED)
        return 1;
    pid_t pid = fork();
    if (pid == 0) {
        atomic_int *b = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (b == MAP_FAILED)
            _exit(1);
        struct timespec nap = {0, 50000000};
        nanosleep(&nap, NULL);
        atomic_store(&b[1024], 1);
        futex(&b[1024], FUTEX_WAKE, 1, NULL);
        _exit(0);
    }
    struct timespec limit = {3, 0}, start, end;
    clock_gettime(CLOCK_MONOTONIC, &start);
    int timeouts = 0;
    while (atomic_load(&a[1024]) == 0)
        if (futex(&a[1024], FUTEX_WAIT, 0, &limit) != 0 && atomic_load(&a[1024]) == 0)
            timeouts++;
    clock_gettime(CLOCK_MONOTONIC, &end);
    int status;
    waitpid(pid, &status, 0);
    long ms = (end.tv_sec - start.tv_sec) * 1000 + (end.tv_nsec - start.tv_nsec) / 1000000;
    printf("woken by the other process: %s, timeouts %d, child %d\n", ms < 2000 ? "yes" : "no",
           timeouts, WEXITSTATUS(status));
    return 0;
}
C
gcc -O2 -o diff4 diff4.c
./diff4
