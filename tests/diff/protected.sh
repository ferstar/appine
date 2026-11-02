# docker-run-options: --cap-add SYS_ADMIN
# fs.protected_symlinks、fs.protected_hardlinks（Linux 的 may_follow_link、may_linkat）和
# 随机数设备的 ioctl（第 185 轮）。原生的虚拟机上两个 sysctl 都是 1；容器里 /proc/sys 是只读
# 的，写入不比（rootfs_programs 的 sysctl 用例在 appine 里测）。随机数的 ioctl 要
# CAP_SYS_ADMIN：加进熵池、重新播种对虚拟机没有坏处
cd /tmp
rm -rf prot && mkdir prot && chmod 755 prot
echo hi > prot/target && chmod 644 prot/target

echo "== sysctl"
for f in fs/protected_symlinks fs/protected_hardlinks kernel/unprivileged_bpf_disabled; do
    echo "$f: $(cat /proc/sys/$f) $(stat -c %a /proc/sys/$f)"
done
# kernel.panic 的值是宿主的配置（CI 的 runner 是 -1），只比权限和格式
echo "kernel/panic: $(stat -c %a /proc/sys/kernel/panic) $(grep -cxE -- '-?[0-9]+' /proc/sys/kernel/panic)"
su -s /bin/sh nobody -c 'cat /proc/sys/fs/protected_symlinks' 2>&1 | sed 's/^cat: //'

echo "== protected_symlinks"
# /tmp 是粘滞位、所有人可写、属于 root 的目录；nobody 的链接
rm -f /tmp/plink /tmp/rlink /tmp/pdir /tmp/pdangle /tmp/prot-new
su -s /bin/sh nobody -c 'ln -s /tmp/prot/target /tmp/plink; ln -s /tmp/prot /tmp/pdir; ln -s /tmp/prot-new /tmp/pdangle'
ln -s /tmp/prot/target /tmp/rlink
cat /tmp/plink 2>&1 | sed 's/^cat: //'
cat /tmp/pdir/target 2>&1 | sed 's/^cat: //'
cat /tmp/rlink
readlink /tmp/plink
ls -l /tmp/plink | awk '{print $1, $3}'
stat -L -c %s /tmp/plink 2>&1 | sed 's/^stat: //'
echo x > /tmp/pdangle 2>&1 | sed 's/^.*: //'
[ -e /tmp/prot-new ] && echo "created through the dangling link" || echo "nothing created"
su -s /bin/sh nobody -c 'cat /tmp/plink; cat /tmp/rlink'
# 不是粘滞位的目录里照常跟随
su -s /bin/sh nobody -c 'mkdir /tmp/prot-open; chmod 777 /tmp/prot-open; ln -s /tmp/prot/target /tmp/prot-open/l'
cat /tmp/prot-open/l
# 符号链接的属主与目录的属主相同时也跟随
mkdir /tmp/prot-sticky && chmod 1777 /tmp/prot-sticky && chown nobody /tmp/prot-sticky
su -s /bin/sh nobody -c 'ln -s /tmp/prot/target /tmp/prot-sticky/l'
cat /tmp/prot-sticky/l
chown root /tmp/prot-sticky
cat /tmp/prot-sticky/l 2>&1 | sed 's/^cat: //'
rm -rf /tmp/plink /tmp/rlink /tmp/pdir /tmp/pdangle /tmp/prot-new /tmp/prot-open /tmp/prot-sticky

echo "== protected_hardlinks"
mkdir -m 1777 /tmp/prot/links
echo secret > /tmp/prot/private && chmod 600 /tmp/prot/private
echo shared > /tmp/prot/shared && chmod 666 /tmp/prot/shared
cp /bin/busybox /tmp/prot/suid && chmod 4755 /tmp/prot/suid
cp /bin/busybox /tmp/prot/sgid && chmod 2755 /tmp/prot/sgid
echo data > /tmp/prot/sgid-noexec && chmod 2666 /tmp/prot/sgid-noexec
mknod /tmp/prot/null c 1 3 && chmod 666 /tmp/prot/null
mkfifo -m 666 /tmp/prot/fifo
for f in private shared suid sgid sgid-noexec null fifo target; do
    su -s /bin/sh nobody -c "ln /tmp/prot/$f /tmp/prot/links/$f" 2>&1 | sed 's/^ln: //'
done
ls /tmp/prot/links
# 属主可以链接自己的任何文件
su -s /bin/sh nobody -c 'echo mine > /tmp/prot/links/mine; chmod 0 /tmp/prot/links/mine; ln /tmp/prot/links/mine /tmp/prot/links/mine2 && echo "owner: ok"'
ln /tmp/prot/private /tmp/prot/links/root-private && echo "root: ok"

echo "== random ioctls"
python3 - <<'EOF'
import fcntl, os, struct, ctypes
fd = os.open("/dev/urandom", os.O_RDWR)
def call(name, req, arg):
    try:
        r = fcntl.ioctl(fd, req, arg)
        print(name, "ok", struct.unpack("i", r)[0] if isinstance(r, bytes) and len(r) == 4 else "")
    except OSError as e:
        print(name, os.strerror(e.errno))
call("RNDGETENTCNT", 0x80045200, b"\0" * 4)
call("RNDADDTOENTCNT", 0x40045201, struct.pack("i", 0))
call("RNDADDTOENTCNT -1", 0x40045201, struct.pack("i", -1))
call("RNDADDENTROPY", 0x40085203, struct.pack("ii", 64, 8) + b"12345678")
call("RNDADDENTROPY -1", 0x40085203, struct.pack("ii", -1, 8) + b"12345678")
call("RNDADDENTROPY len -1", 0x40085203, struct.pack("ii", 0, -1))
call("RNDZAPENTCNT", 0x5204, 0)
call("RNDCLEARPOOL", 0x5206, 0)
call("RNDRESEEDCRNG", 0x5207, 0)
call("RNDGETPOOL", 0x80085202, b"\0" * 8)
call("TCGETS", 0x5401, b"\0" * 60)
# 数据伸进没有映射的页是 EFAULT
libc = ctypes.CDLL(None, use_errno=True)
libc.mmap.restype = ctypes.c_void_p
libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_long]
p = libc.mmap(None, 8192, 3, 0x22, -1, 0)
libc.munmap(ctypes.c_void_p(p + 4096), ctypes.c_size_t(4096))
ctypes.memmove(p + 4096 - 12, struct.pack("ii", 0, 16), 8)
r = libc.ioctl(fd, ctypes.c_ulong(0x40085203), ctypes.c_void_p(p + 4096 - 12))
print("RNDADDENTROPY into a hole", r, os.strerror(ctypes.get_errno()) if r else "")
EOF
su -s /bin/sh nobody -c 'python3 -c "
import fcntl, os, struct
fd = os.open(\"/dev/urandom\", os.O_RDONLY)
print(struct.unpack(\"i\", fcntl.ioctl(fd, 0x80045200, b\"\0\" * 4))[0])
for req, arg in ((0x40045201, struct.pack(\"i\", 0)), (0x40085203, struct.pack(\"ii\", 0, 0)), (0x5207, 0)):
    try:
        fcntl.ioctl(fd, req, arg)
        print(hex(req), \"ok\")
    except OSError as e:
        print(hex(req), os.strerror(e.errno))
"'
rm -rf /tmp/prot
