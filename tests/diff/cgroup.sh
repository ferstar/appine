# cgroup v2 的视图：/proc/<pid>/cgroup、/proc/cgroups、挂载、/sys/fs/cgroup 里文件的权限和
# 内容，只读，以及 JVM、Node.js 看到的内存上限与 memory.max 一致。原生一侧是 docker 的容器
# （cgroup 命名空间、没有 --memory），memory.max 等取决于宿主的值只比它们是否自洽
cd /sys/fs/cgroup
echo "self: $(cat /proc/self/cgroup)"
echo "shell: $(cat /proc/$$/cgroup)"
# /proc/cgroups 列出哪些控制器取决于内核的配置（新内核不列没有 v1 支持的 cpuset、memory），
# 只比都在 v2 的层级（0）上、有 cpu
head -1 /proc/cgroups
awk 'NR > 1 && $2 != 0 { v1 = 1 } $1 == "cpu" { cpu = 1 }
    END { print (v1 ? "a v1 hierarchy" : "all on the v2 hierarchy"), (cpu ? "with cpu" : "without cpu") }' \
    /proc/cgroups
grep ' /sys/fs/cgroup ' /proc/self/mountinfo | cut -d ' ' -f 4-
grep ' /sys/fs/cgroup ' /proc/mounts
echo "fs type: $(stat -f -c %t /sys/fs/cgroup)"
stat -c '%A %n' /sys/fs/cgroup
for c in cpuset cpu memory pids; do
    grep -qw $c cgroup.controllers && echo "controller $c"
done
# 内容与宿主无关的文件。appine 报告 6.1，新内核在 cgroup.stat、memory.events、cpu.stat、
# pids.events 里多了项（nr_subsys_*、sock_throttled、nice_usec……），这几个只比 6.1 就有的
for f in cgroup.events cgroup.freeze cgroup.max.depth cgroup.max.descendants \
    cgroup.subtree_control cgroup.type cpu.idle cpu.max cpu.max.burst cpu.weight \
    cpu.weight.nice cpuset.cpus cpuset.cpus.effective cpuset.cpus.partition cpuset.mems \
    cpuset.mems.effective memory.high memory.low memory.min memory.oom.group \
    memory.swap.current memory.swap.high; do
    echo "$(stat -c %A $f) $f: $(tr '\n' '|' < $f)"
done
echo "$(stat -c %A cgroup.stat) cgroup.stat: $(grep -E '^nr_(dying_)?descendants ' cgroup.stat | tr '\n' '|')"
for f in memory.events memory.events.local; do
    echo "$(stat -c %A $f) $f: $(grep -E '^(low|high|max|oom|oom_kill|oom_group_kill) ' $f | tr '\n' '|')"
done
echo "$(stat -c %A memory.swap.events) memory.swap.events: $(grep -E '^(high|max|fail) ' memory.swap.events | tr '\n' '|')"
echo "$(stat -c %A pids.events) pids.events: $(grep -E '^max ' pids.events | tr '\n' '|')"
for k in usage_usec user_usec system_usec nr_periods nr_throttled throttled_usec nr_bursts \
    burst_usec; do
    grep -q "^$k " cpu.stat && printf '%s ' $k
done
echo
# memory.stat：新内核加了项，有没有 swapcached、*_thp、zswap 又取决于内核的配置，只比 6.1
# 就有、不取决于配置的项的顺序，以及值都是数字
echo "$(stat -c %A memory.stat) memory.stat: $(grep -oE '^[a-z_]+' memory.stat | grep -xE \
    'anon|file|kernel|kernel_stack|pagetables|sec_pagetables|percpu|sock|vmalloc|shmem|file_mapped|file_dirty|file_writeback|inactive_anon|active_anon|inactive_file|active_file|unevictable|slab_reclaimable|slab_unreclaimable|slab|workingset_[a-z_]+|pgscan|pgsteal|pgscan_kswapd|pgscan_direct|pgsteal_kswapd|pgsteal_direct|pgfault|pgmajfault|pgrefill|pgactivate|pgdeactivate|pglazyfree|pglazyfreed' |
    tr '\n' ' ')"
grep -vqE '^[a-z_]+ [0-9]+$' memory.stat || echo "memory.stat has a line that is not a name and a number"
# 取决于宿主和当时的状态的：只比格式（6.12 起 memory.peak 可写，不比它的权限；容器的
# pids.max 可能有限制）
for f in cgroup.controllers cgroup.procs cgroup.threads memory.current memory.max \
    memory.swap.max pids.current pids.max cpu.stat; do
    echo "$(stat -c %A $f) $f"
done
for f in memory.current memory.peak pids.current; do
    grep -qxE '[0-9]+' $f && echo "$f is a number"
done
for f in memory.max pids.max; do
    grep -qxE '[0-9]+|max' $f && echo "$f is a number or max"
done
grep -qx $$ cgroup.procs && echo "the shell is in cgroup.procs"
grep -qx $$ cgroup.threads && echo "the shell is in cgroup.threads"
# 只读的挂载：写文件、建文件和子 cgroup、删除、改权限都是 EROFS
echo 1 > cgroup.freeze
echo 1 > memory.current
touch new
mkdir sub
rm -f memory.max
chmod 600 memory.max
# 运行时看到的内存上限：memory.max 是 max 时就是物理内存
limit=$(cat memory.max)
total=$(($(awk '/^MemTotal:/ { print $2 }' /proc/meminfo) * 1024))
cat > /tmp/Mem.java <<'J'
public class Mem {
    public static void main(String[] args) {
        var os = (com.sun.management.OperatingSystemMXBean)
            java.lang.management.ManagementFactory.getOperatingSystemMXBean();
        System.out.println(os.getTotalMemorySize());
    }
}
J
jvm=$(java /tmp/Mem.java)
if [ "$limit" = max ]; then want=$total; else want=$limit; fi
[ "$jvm" = "$want" ] && echo "the JVM sees the limit" || echo "the JVM sees $jvm, not $want"
java -XshowSettings:system -version 2>&1 | grep -E 'Provider|CPU Period|CPU Quota|CPU Shares'
node -e '
const m = require("fs").readFileSync("/sys/fs/cgroup/memory.max", "utf8").trim();
const c = process.constrainedMemory();
const ok = m === "max" ? c >= 2 ** 64 - 1 : c === Number(m);
console.log(ok ? "node sees the limit" : `node sees ${c}, memory.max is ${m}`);
'
rm -f /tmp/Mem.java
