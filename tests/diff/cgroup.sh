# cgroup v2 的视图：/proc/<pid>/cgroup、/proc/cgroups、挂载、/sys/fs/cgroup 里文件的权限和
# 内容，只读，以及 JVM、Node.js 看到的内存上限与 memory.max 一致。原生一侧是 docker 的容器
# （cgroup 命名空间、没有 --memory），memory.max 等取决于宿主的值只比它们是否自洽
cd /sys/fs/cgroup
echo "self: $(cat /proc/self/cgroup)"
echo "shell: $(cat /proc/$$/cgroup)"
head -1 /proc/cgroups
awk '$1 ~ /^(cpuset|cpu|cpuacct|memory|pids)$/ { print $1, $2, $4 }' /proc/cgroups
grep ' /sys/fs/cgroup ' /proc/self/mountinfo | cut -d ' ' -f 4-
grep ' /sys/fs/cgroup ' /proc/mounts
echo "fs type: $(stat -f -c %t /sys/fs/cgroup)"
stat -c '%A %n' /sys/fs/cgroup
for c in cpuset cpu memory pids; do
    grep -qw $c cgroup.controllers && echo "controller $c"
done
# 内容与宿主无关的文件
for f in cgroup.events cgroup.freeze cgroup.max.depth cgroup.max.descendants cgroup.stat \
    cgroup.subtree_control cgroup.type cpu.idle cpu.max cpu.max.burst cpu.weight \
    cpu.weight.nice cpuset.cpus cpuset.cpus.effective cpuset.cpus.partition cpuset.mems \
    cpuset.mems.effective memory.events memory.events.local memory.high memory.low memory.min \
    memory.oom.group memory.swap.current memory.swap.events memory.swap.high pids.events \
    pids.max; do
    echo "$(stat -c %A $f) $f: $(tr '\n' '|' < $f)"
done
# 取决于宿主和当时的状态的：只比格式
for f in cgroup.controllers cgroup.procs cgroup.threads memory.current memory.max memory.peak \
    memory.swap.max pids.current cpu.stat; do
    echo "$(stat -c %A $f) $f"
done
for f in memory.current memory.peak pids.current; do
    grep -qxE '[0-9]+' $f && echo "$f is a number"
done
grep -qxE '[0-9]+|max' memory.max && echo "memory.max is a number or max"
grep -qx $$ cgroup.procs && echo "the shell is in cgroup.procs"
grep -qx $$ cgroup.threads && echo "the shell is in cgroup.threads"
grep -v core_sched cpu.stat | cut -d ' ' -f 1 | tr '\n' ' '
echo
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
java -XshowSettings:system -version 2>&1 | grep -E 'Provider|CPU Period|CPU Quota|CPU Shares|Maximum Processes'
node -e '
const m = require("fs").readFileSync("/sys/fs/cgroup/memory.max", "utf8").trim();
const c = process.constrainedMemory();
const ok = m === "max" ? c >= 2 ** 64 - 1 : c === Number(m);
console.log(ok ? "node sees the limit" : `node sees ${c}, memory.max is ${m}`);
'
rm -f /tmp/Mem.java
