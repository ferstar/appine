# 断电测试的负载（guest 或原生的 busybox sh）：$1 是数据目录。
# 每一轮 v 把 f1..f20 用"写临时文件、改名覆盖"换成第 v 版（apk 就是这样装文件的）；
# 每 5 轮写一个 s$v，fsync 它和目录之后打印 "synced v"（断电之后它必须完整），再停一下。
d=$1
mkdir -p "$d"
cd "$d" || exit 1
lines() { echo $(( ($1 * 7 + $2 * 13) % 40 * 50 + 50 )); }
v=0
while :; do
    v=$((v + 1))
    i=1
    while [ $i -le 20 ]; do
        yes "$v $i" | head -n "$(lines $v $i)" > f$i.tmp
        mv f$i.tmp f$i
        i=$((i + 1))
    done
    if [ $((v % 5)) -eq 0 ]; then
        yes "s $v" | head -n 500 > s$v
        fsync s$v . && echo "synced $v"
        # 停一下：appine 的后台线程在写入停下之后做完检查点，WAL 之后从头再用
        sleep 0.3
    fi
done
