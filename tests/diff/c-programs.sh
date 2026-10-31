# gcc 编译并运行：浮点、自动向量化、原子操作和线程、setjmp；再比对编译器的输出；
# 几个线程共用一个 fd 用文件位置读写、两个进程同时 O_APPEND 追加；别的线程 chdir、
# 改名、setuid 之后 stat 立刻看到；共享文件映射里跨进程的 futex；新映射的匿名内存是零；
# 点积指令；SHA3、SHA512、I8MM；LRCPC、LRCPC2
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

# 新映射的匿名内存一定是零：appine 把拆掉的宿主内存丢弃内容之后留着，下一次申请同样大小
# 的直接复用（docs/rounds/109-spawn-mmap-bkl.md）。几个线程和子进程反复映射、检查、写满、
# 拆掉同样大小的几种块
cat > diff5.c <<'C'
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static long dirty;

static void *churn(void *arg)
{
    long seed = (long)arg, bad = 0;
    size_t sizes[] = {4096, 65536, 1 << 20, 3 * 4096};
    for (int i = 0; i < 400; i++) {
        size_t len = sizes[(i + seed) % 4];
        unsigned char *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED)
            return (void *)-1;
        for (size_t j = 0; j < len; j += 512)
            bad += p[j] != 0;
        memset(p, 0xab, len);
        munmap(p, len);
    }
    __atomic_add_fetch(&dirty, bad, __ATOMIC_RELAXED);
    return NULL;
}

int main(void)
{
    pthread_t t[4];
    for (long i = 0; i < 4; i++)
        pthread_create(&t[i], NULL, churn, (void *)i);
    pid_t pid = fork();
    if (pid == 0) {
        churn((void *)7);
        _exit(dirty != 0);
    }
    for (int i = 0; i < 4; i++)
        pthread_join(t[i], NULL);
    int status;
    waitpid(pid, &status, 0);
    printf("non-zero bytes in new mappings: %ld, child %d\n", dirty, WEXITSTATUS(status));
    return 0;
}
C
gcc -O2 -o diff5 diff5.c -pthread
./diff5

# 点积指令（ASIMDDP）：宿主有时宣告，HWCAP、ID_AA64ISAR0_EL1 的 DP 字段、/proc/cpuinfo 的
# asimddp 一致；SDOT、UDOT 的向量形式和按元素形式，-O3 时编译器把 int8 的点积循环向量化成
# SDOT。整个文件只加 +dotprod，别的 v8.2 指令（RDM 等）appine 不宣告
cat > dotprod.c <<'C'
#include <arm_neon.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/auxv.h>

#define HWCAP_ASIMDDP (1 << 20)

static int8_t a[4096], b[4096];
static uint8_t ua[4096], ub[4096];

static int32_t dot_loop(const int8_t *x, const int8_t *y, int n)
{
    int32_t s = 0;
    for (int i = 0; i < n; i++)
        s += x[i] * y[i];
    return s;
}

static void show(const char *name, int32x4_t v)
{
    printf("%s %d %d %d %d\n", name, vgetq_lane_s32(v, 0), vgetq_lane_s32(v, 1),
           vgetq_lane_s32(v, 2), vgetq_lane_s32(v, 3));
}

int main(void)
{
    unsigned long hw = getauxval(AT_HWCAP);
    uint64_t isar0;
    __asm__ volatile("mrs %0, id_aa64isar0_el1" : "=r"(isar0));
    printf("hwcap %d, DP field %d\n", !!(hw & HWCAP_ASIMDDP), (int)(isar0 >> 44 & 15));
    if (!(hw & HWCAP_ASIMDDP))
        return 0;
    uint32_t seed = 1;
    for (int i = 0; i < 4096; i++) {
        seed = seed * 1103515245 + 12345;
        a[i] = seed >> 16;
        b[i] = seed >> 24;
        ua[i] = seed >> 8;
        ub[i] = seed >> 20;
    }
    int8x16_t va = vld1q_s8(a), vb = vld1q_s8(b);
    uint8x16_t vua = vld1q_u8(ua), vub = vld1q_u8(ub);
    int32x4_t acc = {-7, 100000, -2147483000, 2147483000};
    uint32x4_t uacc = vreinterpretq_u32_s32(acc);
    int32x2_t zero = vdup_n_s32(0);
    show("sdot 4s", vdotq_s32(acc, va, vb));
    show("udot 4s", vreinterpretq_s32_u32(vdotq_u32(uacc, vua, vub)));
    show("sdot 2s", vcombine_s32(vdot_s32(vget_low_s32(acc), vget_low_s8(va), vget_high_s8(vb)), zero));
    for (int lane = 0; lane < 4; lane++) {
        int32x4_t s, u;
        switch (lane) {
        case 0: s = vdotq_laneq_s32(acc, va, vb, 0); u = vreinterpretq_s32_u32(vdotq_laneq_u32(uacc, vua, vub, 0)); break;
        case 1: s = vdotq_laneq_s32(acc, va, vb, 1); u = vreinterpretq_s32_u32(vdotq_laneq_u32(uacc, vua, vub, 1)); break;
        case 2: s = vdotq_laneq_s32(acc, va, vb, 2); u = vreinterpretq_s32_u32(vdotq_laneq_u32(uacc, vua, vub, 2)); break;
        default: s = vdotq_laneq_s32(acc, va, vb, 3); u = vreinterpretq_s32_u32(vdotq_laneq_u32(uacc, vua, vub, 3)); break;
        }
        printf("lane %d: ", lane);
        show("sdot", s);
        printf("lane %d: ", lane);
        show("udot", u);
    }
    show("sdot 2s lane 1", vcombine_s32(vdot_lane_s32(vget_low_s32(acc), vget_low_s8(va), vget_high_s8(vb), 1), zero));
    printf("loop %d\n", dot_loop(a, b, 4096));
    return 0;
}
C
gcc -O3 -march=armv8-a+dotprod -o dotprod dotprod.c && ./dotprod
echo "sdot in the loop: $(gcc -O3 -march=armv8-a+dotprod -S -o - dotprod.c | grep -c 'sdot	v')"
echo "cpuinfo: $(grep -m 1 ^Features /proc/cpuinfo | grep -cw asimddp)"

# SHA3（EOR3、BCAX、RAX1、XAR）、SHA512、I8MM（SMMLA、UMMLA、USMMLA、USDOT、SUDOT）：宿主有时
# 宣告，HWCAP、HWCAP2、ID_AA64ISAR0_EL1 的 SHA2、SHA3 字段、ID_AA64ISAR1_EL1 的 I8MM 字段、
# /proc/cpuinfo 一致，intrinsics 的结果相同。整个文件只加 +sha3+i8mm
cat > crypto.c <<'C'
#include <arm_neon.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/auxv.h>

#define HWCAP_SHA3 (1 << 17)
#define HWCAP_SHA512 (1 << 21)
#define HWCAP2_I8MM (1 << 13)

static void show8(const char *name, uint8x16_t v)
{
    uint8_t b[16];
    vst1q_u8(b, v);
    printf("%s", name);
    for (int i = 0; i < 16; i++)
        printf(" %02x", b[i]);
    printf("\n");
}

static void show64(const char *name, uint64x2_t v)
{
    printf("%s %016llx %016llx\n", name, (unsigned long long)vgetq_lane_u64(v, 0),
           (unsigned long long)vgetq_lane_u64(v, 1));
}

static void show32(const char *name, int32x4_t v)
{
    printf("%s %d %d %d %d\n", name, vgetq_lane_s32(v, 0), vgetq_lane_s32(v, 1),
           vgetq_lane_s32(v, 2), vgetq_lane_s32(v, 3));
}

int main(void)
{
    unsigned long hw = getauxval(AT_HWCAP), hw2 = getauxval(AT_HWCAP2);
    uint64_t isar0, isar1;
    __asm__ volatile("mrs %0, id_aa64isar0_el1" : "=r"(isar0));
    __asm__ volatile("mrs %0, id_aa64isar1_el1" : "=r"(isar1));
    printf("sha3 %d sha512 %d i8mm %d; SHA2 field %d, SHA3 field %d, I8MM field %d\n",
           !!(hw & HWCAP_SHA3), !!(hw & HWCAP_SHA512), !!(hw2 & HWCAP2_I8MM),
           (int)(isar0 >> 12 & 15), (int)(isar0 >> 32 & 15), (int)(isar1 >> 52 & 15));
    uint8_t x[64];
    for (int i = 0; i < 64; i++)
        x[i] = i * 37 + 11;
    uint8x16_t a = vld1q_u8(x), b = vld1q_u8(x + 16), c = vld1q_u8(x + 32);
    uint64x2_t A = vreinterpretq_u64_u8(a), B = vreinterpretq_u64_u8(b), C = vreinterpretq_u64_u8(c);
    if (hw & HWCAP_SHA3) {
        show8("eor3", veor3q_u8(a, b, c));
        show8("bcax", vbcaxq_u8(a, b, c));
        show64("rax1", vrax1q_u64(A, B));
        show64("xar", vxarq_u64(A, B, 23));
    }
    if (hw & HWCAP_SHA512) {
        show64("sha512h", vsha512hq_u64(A, B, C));
        show64("sha512h2", vsha512h2q_u64(A, B, C));
        show64("sha512su0", vsha512su0q_u64(A, B));
        show64("sha512su1", vsha512su1q_u64(A, B, C));
    }
    if (hw2 & HWCAP2_I8MM) {
        int8x16_t sa = vreinterpretq_s8_u8(a), sb = vreinterpretq_s8_u8(b);
        int32x4_t acc = {1, -2, 3000000, -2147483000};
        uint32x4_t uacc = vreinterpretq_u32_s32(acc);
        show32("smmla", vmmlaq_s32(acc, sa, sb));
        show32("ummla", vreinterpretq_s32_u32(vmmlaq_u32(uacc, a, b)));
        show32("usmmla", vusmmlaq_s32(acc, a, sb));
        show32("usdot", vusdotq_s32(acc, a, sb));
        show32("usdot 2s", vcombine_s32(vusdot_s32(vget_low_s32(acc), vget_low_u8(a), vget_low_s8(sb)),
                                        vdup_n_s32(0)));
        show32("usdot lane 2", vusdotq_laneq_s32(acc, a, sb, 2));
        show32("sudot lane 3", vsudotq_laneq_s32(acc, sa, b, 3));
    }
    return 0;
}
C
gcc -O2 -march=armv8-a+sha3+i8mm -o crypto crypto.c && ./crypto
echo "instructions: $(gcc -O2 -march=armv8-a+sha3+i8mm -S -o - crypto.c | grep -cE '^	(eor3|bcax|rax1|xar|sha512h|sha512h2|sha512su0|sha512su1|smmla|ummla|usmmla|usdot|sudot)	')"
echo "cpuinfo: $(grep -m 1 ^Features /proc/cpuinfo | tr ' ' '\n' | grep -cxE 'sha3|sha512|i8mm')"

# LRCPC：宿主有时宣告，HWCAP、ID_AA64ISAR1_EL1 的 LRCPC 字段、/proc/cpuinfo 的 lrcpc 一致；
# LDAPR、LDAPRH、LDAPRB 读到的值，-march=armv8-a+rcpc 时 acquire 读编成 LDAPR，两个线程用
# STLR、LDAPR 传消息时读到序号之后它对应的数据一定已经写好
cat > lrcpc.c <<'C'
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/auxv.h>

#define HWCAP_LRCPC (1 << 15)
#define N 200000

static uint64_t slots[64];
static uint64_t seq;
/* 只读的页上也能 LDAPR（读不要写权限） */
static const uint64_t ro_word = 0x0123456789abcdefull;

static void *producer(void *arg)
{
    for (uint64_t i = 1; i <= N; i++) {
        slots[i % 64] = i * 3;
        __atomic_store_n(&seq, i, __ATOMIC_RELEASE);
    }
    return NULL;
}

int main(void)
{
    unsigned long hw = getauxval(AT_HWCAP);
    uint64_t isar1;
    __asm__ volatile("mrs %0, id_aa64isar1_el1" : "=r"(isar1));
    /* 宿主还有 LRCPC2（LDAPUR 这些）时 Linux 报 2，appine 只实现了 LRCPC，报 1 */
    printf("lrcpc %d, LRCPC field at least 1: %d\n", !!(hw & HWCAP_LRCPC), (int)(isar1 >> 20 & 15) >= 1);
    if (!(hw & HWCAP_LRCPC))
        return 0;
    uint64_t word = 0x8899aabbccddeeffull, x;
    uint32_t w, h, b;
    __asm__ volatile("ldapr %0, [%1]" : "=r"(x) : "r"(&word) : "memory");
    __asm__ volatile("ldapr %w0, [%1]" : "=r"(w) : "r"(&word) : "memory");
    __asm__ volatile("ldaprh %w0, [%1]" : "=r"(h) : "r"((char *)&word + 2) : "memory");
    __asm__ volatile("ldaprb %w0, [%1]" : "=r"(b) : "r"((char *)&word + 7) : "memory");
    printf("ldapr %016llx %08x ldaprh %04x ldaprb %02x\n", (unsigned long long)x, w, h, b);
    __asm__ volatile("ldapr %0, [%1]" : "=r"(x) : "r"(&ro_word) : "memory");
    printf("ldapr from .rodata %016llx\n", (unsigned long long)x);
    pthread_t t;
    pthread_create(&t, NULL, producer, NULL);
    uint64_t seen = 0, bad = 0, reads = 0;
    while (seen < N) {
        uint64_t s = __atomic_load_n(&seq, __ATOMIC_ACQUIRE);
        if (s == 0)
            continue;
        uint64_t v = *(volatile uint64_t *)&slots[s % 64];
        if (v < s * 3 || (v - s * 3) % (64 * 3) != 0)
            bad++;
        seen = s;
        reads++;
    }
    pthread_join(t, NULL);
    printf("message passing: %s, out of order %llu\n", reads > 0 ? "read" : "none", (unsigned long long)bad);
    return 0;
}
C
gcc -O2 -march=armv8-a+rcpc -pthread -o lrcpc lrcpc.c && ./lrcpc
echo "ldapr in the code: $(gcc -O2 -march=armv8-a+rcpc -S -o - lrcpc.c | grep -c '	ldapr')"
echo "cpuinfo: $(grep -m 1 ^Features /proc/cpuinfo | tr ' ' '\n' | grep -cx lrcpc)"

# LRCPC2（带偏移的 LDAPUR*、LDAPURS*、STLUR*）：宿主有时宣告，HWCAP 的 ilrcpc、
# ID_AA64ISAR1_EL1 的 LRCPC 字段（至少 2）、cpuinfo 一致；各种宽度、补零和符号扩展的读，正负
# 偏移，写，.rodata 上的读。汇编器不一定认这些助记符，直接写指令字，固定用 x0、x1
cat > lrcpc2.c <<'C'
#include <stdint.h>
#include <stdio.h>
#include <sys/auxv.h>

#define HWCAP_ILRCPC (1 << 26)

static uint64_t buf[4] = {0x8899aabbccddeeffull, 0x0011223344556677ull, 0, 0};
static const uint64_t ro[2] = {0x0123456789abcdefull, 0xfedcba9876543210ull};

#define LOAD(word, base)                                                                           \
    ({                                                                                             \
        register uint64_t b __asm__("x1") = (uint64_t)(base);                                      \
        register uint64_t r __asm__("x0");                                                         \
        __asm__ volatile(".inst " #word : "=r"(r) : "r"(b) : "memory");                            \
        r;                                                                                         \
    })
#define STORE(word, base, value)                                                                   \
    do {                                                                                           \
        register uint64_t b __asm__("x1") = (uint64_t)(base);                                      \
        register uint64_t v __asm__("x0") = (value);                                               \
        __asm__ volatile(".inst " #word : : "r"(b), "r"(v) : "memory");                            \
    } while (0)

int main(void)
{
    unsigned long hw = getauxval(AT_HWCAP);
    uint64_t isar1;
    __asm__ volatile("mrs %0, id_aa64isar1_el1" : "=r"(isar1));
    /* 宿主还有 LRCPC3 时 Linux 报 3，appine 报 2 */
    printf("ilrcpc %d, LRCPC field at least 2: %d\n", !!(hw & HWCAP_ILRCPC), (int)(isar1 >> 20 & 15) >= 2);
    if (!(hw & HWCAP_ILRCPC))
        return 0;
    printf("ldapur x  %016llx\n", (unsigned long long)LOAD(0xd9408020, buf));
    printf("ldapur w  %016llx\n", (unsigned long long)LOAD(0x99404020, buf));
    printf("ldapurh   %016llx\n", (unsigned long long)LOAD(0x59406020, buf));
    printf("ldapurb   %016llx\n", (unsigned long long)LOAD(0x19407020, buf));
    printf("ldapursb  %016llx\n", (unsigned long long)LOAD(0x19804020, buf));
    printf("ldapursh  %016llx\n", (unsigned long long)LOAD(0x59c06020, buf));
    printf("ldapursw  %016llx\n", (unsigned long long)LOAD(0x99804020, buf));
    printf("ldapur -8 %016llx\n", (unsigned long long)LOAD(0xd95f8020, buf + 2));
    printf("rodata    %016llx\n", (unsigned long long)LOAD(0xd9408020, ro));
    STORE(0xd9010020, buf, 0x1122334455667788ull);
    STORE(0x59012020, buf, 0xbeefull);
    STORE(0x19015020, buf, 0x5aull);
    printf("stored    %016llx %016llx\n", (unsigned long long)buf[2], (unsigned long long)buf[3]);
    return 0;
}
C
gcc -O1 -o lrcpc2 lrcpc2.c && ./lrcpc2
echo "cpuinfo: $(grep -m 1 ^Features /proc/cpuinfo | tr ' ' '\n' | grep -cx ilrcpc)"
# FP16（标量和向量的半精度运算）和 JSCVT（FJCVTZS）：宿主有时宣告。半精度的数组运算由编译器
# 向量化成 fmla v.8h；FPCR.FZ16 写得进去、把非规格化的半精度结果冲成 0；FJCVTZS 按 JavaScript
# 的 ToInt32 取模 2^32，精确时 Z 置位
cat > fp16.c <<'C'
#include <arm_neon.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/auxv.h>

#define HWCAP_FPHP (1 << 9)
#define HWCAP_ASIMDHP (1 << 10)
#define HWCAP_JSCVT (1 << 13)

static __fp16 a[1024], b[1024], c[1024];

static void fma_loop(int n)
{
    for (int i = 0; i < n; i++)
        c[i] = a[i] * b[i] + c[i];
}

static uint16_t bits(__fp16 h)
{
    uint16_t u;
    memcpy(&u, &h, 2);
    return u;
}

static void show(const char *name, float16x8_t v)
{
    uint16_t u[8];
    memcpy(u, &v, 16);
    printf("%s", name);
    for (int i = 0; i < 8; i++)
        printf(" %04x", u[i]);
    printf("\n");
}

static unsigned jcvt(double d, int *z)
{
    register double in __asm__("d1") = d;
    register unsigned out __asm__("w0");
    unsigned long nzcv;
    __asm__ volatile(".inst 0x1e7e0020\n\tmrs %1, nzcv" : "=r"(out), "=r"(nzcv) : "w"(in) : "cc");
    *z = (nzcv >> 30) & 1;
    return out;
}

int main(void)
{
    unsigned long hw = getauxval(AT_HWCAP);
    uint64_t pfr0, isar1;
    __asm__ volatile("mrs %0, id_aa64pfr0_el1" : "=r"(pfr0));
    __asm__ volatile("mrs %0, id_aa64isar1_el1" : "=r"(isar1));
    printf("hwcap fphp %d asimdhp %d jscvt %d; FP %d AdvSIMD %d JSCVT %d\n", !!(hw & HWCAP_FPHP),
           !!(hw & HWCAP_ASIMDHP), !!(hw & HWCAP_JSCVT), (int)(pfr0 >> 16 & 15),
           (int)(pfr0 >> 20 & 15), (int)(isar1 >> 12 & 15));
    if (hw & HWCAP_JSCVT) {
        double in[] = {3.0, -1.5, 4294967301.0, 2147483648.0, -2147483649.0, 1e20, -0.0,
                       0.5, INFINITY, -INFINITY, NAN, 1.0 / 3, 123456789012.75};
        for (unsigned i = 0; i < sizeof in / sizeof *in; i++) {
            int z;
            unsigned w = jcvt(in[i], &z);
            printf("fjcvtzs %g -> %u z %d\n", in[i], w, z);
        }
    }
    if (!(hw & HWCAP_FPHP) || !(hw & HWCAP_ASIMDHP))
        return 0;
    for (int i = 0; i < 1024; i++) {
        a[i] = (__fp16)((i % 37) * 0.25f - 4);
        b[i] = (__fp16)((i % 11) * 0.125f + 0.5f);
        c[i] = (__fp16)(i * 0.01f);
    }
    fma_loop(1024);
    unsigned sum = 0;
    for (int i = 0; i < 1024; i++)
        sum = sum * 31 + bits(c[i]);
    printf("fma loop checksum %08x, c[1000] = %g\n", sum, (float)c[1000]);
    float16x8_t x = vld1q_f16(a + 3), y = vld1q_f16(b + 5), z = vld1q_f16(c + 7);
    show("vfmaq", vfmaq_f16(z, x, y));
    show("vdivq", vdivq_f16(x, y));
    show("vsqrtq", vsqrtq_f16(vabsq_f16(x)));
    show("vrndnq", vrndnq_f16(vmulq_n_f16(x, (__fp16)1.37f)));
    show("vrecpeq", vrecpeq_f16(y));
    show("vfmaq_laneq", vfmaq_laneq_f16(z, x, y, 6));
    uint16x8_t gt = vcgtq_f16(x, y);
    int16x8_t conv = vcvtq_s16_f16(vmulq_n_f16(x, (__fp16)3.3f));
    show("vcgtq", vreinterpretq_f16_u16(gt));
    show("vcvtq_s16", vreinterpretq_f16_s16(conv));
    printf("vmaxnmvq %04x vminvq %04x\n", bits(vmaxnmvq_f16(x)), bits(vminvq_f16(y)));
    __fp16 h = (__fp16)2.5f, k = (__fp16)-0.75f;
    printf("scalar: add %04x div %04x fma %04x cvt %d %u sqrt %04x\n", bits(h + k),
           bits(vdivh_f16(h, k)), bits(vfmah_f16(h, k, h)), vcvth_s32_f16(k * (__fp16)10),
           vcvtah_u32_f16(h), bits(vsqrth_f16(h)));
    /* FZ16：非规格化的半精度结果冲成 0（单精度的 FZ 不管半精度） */
    volatile __fp16 tiny = (__fp16)6.1e-5f, half = (__fp16)0.25f, r0, r1;
    r0 = tiny * half;
    uint64_t fpcr;
    __asm__ volatile("mrs %0, fpcr" : "=r"(fpcr)::"memory");
    __asm__ volatile("msr fpcr, %0" ::"r"(fpcr | 1 << 19) : "memory");
    r1 = tiny * half;
    uint64_t now;
    __asm__ volatile("mrs %0, fpcr" : "=r"(now)::"memory");
    __asm__ volatile("msr fpcr, %0" ::"r"(fpcr) : "memory");
    printf("subnormal %04x, with FZ16 %04x, FZ16 kept %d\n", bits(r0), bits(r1), !!(now & 1 << 19));
    return 0;
}
C
gcc -O3 -march=armv8.2-a+fp16 -o fp16 fp16.c -lm && ./fp16
echo "fmla in the loop: $(gcc -O3 -march=armv8.2-a+fp16 -S -o - fp16.c | grep -c 'fmla	v[0-9]*\.8h')"
echo "cpuinfo: $(grep -m 1 ^Features /proc/cpuinfo | tr ' ' '\n' | grep -cxE 'fphp|asimdhp|jscvt')"
