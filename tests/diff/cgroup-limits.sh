# docker-run-options: --privileged
# cgroup 的限制（docs/rounds/164-cgroup-limits.md）：pids.max 挡住 fork 和建线程，
# pids.current 算上僵尸进程，pids.events 记在哪里，迁移不看上限，关掉再开控制器回到默认；
# memory.max 超出时结束进程，memory.events（往上累加）和 .local，调低 memory.max 时结束超出
# 的，memory.oom.group 整组结束。原生一侧是特权容器，容器的根不是真正的根，有进程时开不了
# 控制器，先像 docker-in-docker 那样把进程都挪进 init（appine 的根是真正的根，也照做）。
# 内存只比事件的次数（max 是碰到上限的次数，Linux 每次分配都算，不比具体的数）
cd /sys/fs/cgroup
mkdir init
for p in $(cat cgroup.procs); do echo $p > init/cgroup.procs 2>/dev/null; done
echo "+pids +memory" > cgroup.subtree_control && echo "root subtree: $(cat cgroup.subtree_control)"
mkdir t
echo "t controllers: $(cat t/cgroup.controllers)"
echo "+pids +memory" > t/cgroup.subtree_control
mkdir t/p t/p/q
# pids.events：新内核（CI 的）改成往上累加，6.1、6.8 的含义挪到了 pids.events.local
pev() { cat "$1/pids.events.local" 2>/dev/null || cat "$1/pids.events"; }
echo "p: max $(cat t/p/pids.max) current $(cat t/p/pids.current) $(pev t/p)"
[ -e t/p/q/pids.max ] || echo "q has no pids.max"
for v in -1 4194305 abc 4194304 max 3; do
    echo $v > t/p/pids.max && echo "pids.max $v ok"
done
# 僵尸进程算，线程算，超出时 pthread_create、fork 是 EAGAIN；q 里的算在 p 上
cat > /tmp/pids.c <<'C'
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static void *idle(void *a) { pause(); return a; }

static void current(const char *what)
{
    char b[32] = "";
    FILE *f = fopen("/sys/fs/cgroup/t/p/pids.current", "r");
    fgets(b, sizeof b, f);
    fclose(f);
    printf("%s: %s", what, b);
    fflush(stdout);
}

int main(void)
{
    current("start");
    pid_t c = fork();
    if (c == 0)
        _exit(0);
    usleep(200000);
    current("with a zombie");
    waitpid(c, NULL, 0);
    current("reaped");
    pthread_t t[3];
    for (int i = 0; i < 3; i++) {
        int e = pthread_create(&t[i], NULL, idle, NULL);
        printf("thread %d: %s\n", i, e ? strerror(e) : "ok");
    }
    current("threads");
    c = fork();
    printf("fork: %s\n", c < 0 ? strerror(errno) : "ok");
    if (c == 0)
        _exit(0);
    return 0;
}
C
gcc -pthread -o /tmp/pids /tmp/pids.c
sh -c 'echo $$ > /sys/fs/cgroup/t/p/q/cgroup.procs; /tmp/pids'
echo "events p: $(pev t/p), t: $(pev t)"
echo "p current after: $(cat t/p/pids.current)"
# 迁移进来不看上限
sleep 100 & s1=$!
sleep 100 & s2=$!
sleep 100 & s3=$!
sleep 100 & s4=$!
for s in $s1 $s2 $s3 $s4; do echo $s > t/p/cgroup.procs; done
echo "after migration: $(cat t/p/pids.current) max $(cat t/p/pids.max)"
kill $s1 $s2 $s3 $s4
wait 2>/dev/null
# shell 里 fork 不出来：ash 报错退出，留下两个 sleep
sh -c 'echo $$ > /sys/fs/cgroup/t/p/cgroup.procs; for i in 1 2 3; do sleep 100 & done; echo unreachable'
echo "shell status $?"
echo "events p: $(pev t/p)"
kill $(cat t/p/cgroup.procs)
# 关掉再开 pids：上限、事件回到默认
echo "-pids" > t/cgroup.subtree_control && echo "+pids" > t/cgroup.subtree_control
echo "p after re-enable: max $(cat t/p/pids.max) $(pev t/p)"

# memory.max：超出时结束进程（swap.max 设 0，有交换分区的宿主上也不换出）
cat > /tmp/eat.c <<'C'
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv)
{
    int mb = argc > 1 ? atoi(argv[1]) : 512;
    for (int i = 0; i < mb; i++)
        memset(malloc(1 << 20), 1, 1 << 20);
    // 有参数时分配完了等着被结束，最多等 10 秒（不结束时用例也不会卡住）
    if (argc > 1) {
        puts("allocated");
        fflush(stdout);
        sleep(10);
        puts("not killed");
        return 0;
    }
    puts("survived");
    return 0;
}
C
gcc -o /tmp/eat /tmp/eat.c
# 第二个参数是 exact 时比 oom 的次数，否则只比它是否大于 0（调低 memory.max 时 Linux 在
# 等进程退出的时候反复记）
events() {
    awk -v exact="${2:-}" '{ v[$1] = $2 } END {
        oom = exact ? v["oom"] : (v["oom"] > 0 ? ">0" : "0")
        printf "max>0 %d oom %s oom_kill %d oom_group_kill %d\n", (v["max"] > 0), oom, v["oom_kill"], v["oom_group_kill"]
    }' $1
}
mkdir t/m
echo "m: max $(cat t/m/memory.max) $(events t/m/memory.events exact)"
echo 32M > t/m/memory.max && echo 0 > t/m/memory.swap.max && echo "m max $(cat t/m/memory.max)"
sh -c 'echo $$ > /sys/fs/cgroup/t/m/cgroup.procs; exec /tmp/eat'
echo "eat status $?"
echo "m events: $(events t/m/memory.events exact)"
echo "m local: $(events t/m/memory.events.local exact)"
echo "t events: $(events t/memory.events)"
echo "t local: $(events t/memory.events.local)"
peak=$(cat t/m/memory.peak)
echo "m peak over 16M: $((peak > 16 * 1024 * 1024))"
# 调低 memory.max：已经超出的进程被结束
mkdir t/w
echo 0 > t/w/memory.swap.max
rm -f /tmp/ready
mkfifo /tmp/ready
sh -c 'echo $$ > /sys/fs/cgroup/t/w/cgroup.procs; exec /tmp/eat 48' > /tmp/ready &
w=$!
read line < /tmp/ready
echo "w: $line"
echo 16M > t/w/memory.max && echo "lowered"
# Linux 在写的时候就结束了它，appine 稍后：ash 打不打印 Killed 看等的时候它结束了没有
wait $w 2>/dev/null
echo "w status $?"
echo "w events: $(events t/w/memory.events)"
rm /tmp/ready
# memory.oom.group：整组结束，挑中的那个在 oom_kill 里算两次
mkdir t/g
echo 32M > t/g/memory.max
echo 0 > t/g/memory.swap.max
echo 1 > t/g/memory.oom.group
sh -c 'echo $$ > /sys/fs/cgroup/t/g/cgroup.procs; sleep 10 & /tmp/eat; echo "eat in group $?"; wait; echo "sleep $?"'
echo "group shell status $?"
echo "g events: $(events t/g/memory.events exact)"
echo "t events: $(events t/memory.events)"
echo "root events: $(events memory.events)"
