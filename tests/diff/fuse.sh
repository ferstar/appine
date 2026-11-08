# docker-run-options: --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined
# FUSE through fuse-overlayfs (libfuse 3.17, which reads requests with splice and writes data
# replies through vmsplice and splice): the mount and its line in mountinfo, lookups, stat,
# readdir, symlinks, reads and writes (one larger than a request), O_APPEND (two processes appending
# at once) and O_TRUNC, creating,
# renaming and removing files and directories, links, chmod, truncate and times, xattrs, statfs,
# running a script and a program from the mount, a private mapping, the errors the daemon and the
# kernel return, the daemon leaving when the mount goes, ENOTCONN once the daemon is killed, and
# the errors of mount options and of /dev/fuse before a mount. The layers are on one tmpfs on
# both sides. Not compared: inode numbers (fuse-overlayfs takes them from the layers' files) and
# times
cd /tmp
umount -l /tmp/fuse/m 2>/dev/null
umount -l /tmp/fuse 2>/dev/null
rm -rf fuse
mkdir fuse
mount -t tmpfs tmpfs fuse
cd fuse
mkdir -p l/sub u w m
echo hello > l/a.txt
echo deep > l/sub/b
ln -s a.txt l/link
printf '#!/bin/sh\necho script ran "$@"\n' > l/s.sh
chmod 755 l/s.sh
cp /bin/busybox l/busybox
fuse-overlayfs -o lowerdir=/tmp/fuse/l,upperdir=/tmp/fuse/u,workdir=/tmp/fuse/w m || exit 1
echo "-- mountinfo"
awk '$5 == "/tmp/fuse/m" { for (i = 1; $i != "-"; i++); print $6, $(i + 1), $(i + 2), $(i + 3) }' \
    /proc/self/mountinfo
echo "-- read"
ls -a m | sort
ls -l m | awk 'NR > 1 { print $1, $3, $4, ($1 ~ /^d/ ? "-" : $5), $9, $10, $11 }' | sort -k5
cat m/a.txt m/sub/b m/link
readlink m/link
stat -c '%n %F %a %h %u %g' m m/a.txt m/sub m/link
stat -c '%n %s' m/a.txt m/link
stat -f -c '%t %b %l' m | awk '{ print $1, ($2 > 0), $3 }'
test -e m/missing || echo "missing: no"
cat m/missing 2>&1
cat m/a.txt/x 2>&1
echo "-- write"
echo one > m/new
echo two >> m/new
cat m/new
printf abcdefghij > m/trunc
: > m/trunc
stat -c '%n %s' m/trunc
printf 0123456789 > m/t2
truncate -s 4 m/t2
cat m/t2; echo
truncate -s 8 m/t2
od -An -c m/t2
dd if=/dev/urandom of=/tmp/fuse/big bs=1024 count=700 2>/dev/null
cp /tmp/fuse/big m/big
cmp /tmp/fuse/big m/big && echo "big: same"
stat -c '%n %s' m/big
python3 -c '
import os
fd = os.open("/tmp/fuse/m/new", os.O_RDWR)
os.lseek(fd, 1, 0)
os.write(fd, b"X")
print(os.pread(fd, 3, 0), os.lseek(fd, 0, 2), os.fstat(fd).st_size)
os.close(fd)
fd = os.open("/tmp/fuse/m/a.txt", os.O_WRONLY | os.O_APPEND)
os.write(fd, b"more\n")
os.close(fd)
'
cat m/a.txt
cat l/a.txt
echo "-- namespace"
mkdir m/d m/d/e
mkdir m/d 2>&1
echo f > m/d/f
mv m/d/f m/d/g
mv m/new m/d/
ls m/d | sort
rmdir m/d 2>&1
ln m/d/g m/d/h
stat -c '%n %h' m/d/g
ln -s ../a.txt m/d/sl
cat m/d/sl
rm m/d/g m/d/sl
rmdir m/d/e
rm m/link
ls m m/d | sort
mv m/sub m/sub2
cat m/sub2/b
cd m/sub2 && pwd && cd /tmp/fuse
python3 -c '
import os
os.chdir("/tmp/fuse/m/d")
print(os.getcwd())
os.rename("/tmp/fuse/m/d", "/tmp/fuse/m/d2")
print(os.getcwd())
for f, n in [(os.rename, ("/tmp/fuse/m/d2", "/tmp/fuse/m/d2/x")), (os.rmdir, ("/tmp/fuse/m",)),
             (os.unlink, ("/tmp/fuse/m/sub2",)), (os.mkdir, ("/tmp/fuse/m/no/x",))]:
    try:
        f(*n)
    except OSError as e:
        print(f.__name__, e.strerror)
'
echo "-- attributes"
chmod 640 m/a.txt
stat -c '%n %a' m/a.txt
chown 1:2 m/trunc
stat -c '%n %u %g' m/trunc
touch -d '2001-02-03 04:05:06' m/trunc
stat -c '%n %Y' m/trunc
python3 -c '
import os
p = "/tmp/fuse/m/trunc"
os.setxattr(p, "user.note", b"kept")
print(os.getxattr(p, "user.note"), "user.note" in os.listxattr(p))
os.removexattr(p, "user.note")
try:
    os.getxattr(p, "user.note")
except OSError as e:
    print("getxattr", e.strerror)
'
python3 -c '
import fcntl, os, resource, signal
p = "/tmp/fuse/m/new2"
fd = os.open(p, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
os.write(fd, b"ab")
fcntl.fcntl(fd, fcntl.F_SETFL, os.O_APPEND)
os.lseek(fd, 0, 0)
os.write(fd, b"cd")
fcntl.fcntl(fd, fcntl.F_SETFL, 0)
os.lseek(fd, 0, 0)
os.write(fd, b"X")
os.close(fd)
print(open(p, "rb").read())
signal.signal(signal.SIGXFSZ, signal.SIG_IGN)
hard = resource.getrlimit(resource.RLIMIT_FSIZE)[1]
resource.setrlimit(resource.RLIMIT_FSIZE, (6, hard))
fd = os.open(p, os.O_WRONLY)
print("write past the limit:", os.pwrite(fd, b"123456", 2))
try:
    os.pwrite(fd, b"7", 6)
except OSError as e:
    print("write at the limit:", e.strerror)
os.close(fd)
resource.setrlimit(resource.RLIMIT_FSIZE, (hard, hard))
fd = os.open(p, os.O_RDWR)
os.posix_fallocate(fd, 0, 100)
print("fallocated:", os.fstat(fd).st_size)
os.close(fd)
'
# Two processes appending at once: no line lands on another
for i in 1 2; do
    (for j in $(seq 200); do echo "writer $i line $j" >> m/log; done) &
done
wait
echo "appended: $(wc -l < m/log) lines, $(sort -u m/log | wc -l) distinct"
echo "-- programs and mappings"
m/s.sh arg
m/busybox echo "busybox ran"
printf '#!/tmp/fuse/m/busybox sh\necho "interpreter on the mount"\n' > m/i.sh
chmod 755 m/i.sh
m/i.sh
cp m/busybox m/sleep
m/sleep 2 &
sleep 0.5
python3 -c '
import os
try:
    os.open("/tmp/fuse/m/sleep", os.O_WRONLY)
except OSError as e:
    print("writing a running program:", e.strerror)
fd = os.open("/tmp/fuse/m/s.sh", os.O_WRONLY)
try:
    os.execv("/tmp/fuse/m/s.sh", ["s.sh"])
except OSError as e:
    print("running a file open for writing:", e.strerror)
'
wait
python3 -c '
import mmap
with open("/tmp/fuse/m/sub2/b", "rb") as f:
    m = mmap.mmap(f.fileno(), 0, prot=mmap.PROT_READ)
    print(m[:4])
    for flags, prot in [(mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE),
                        (mmap.MAP_PRIVATE, mmap.PROT_READ | mmap.PROT_EXEC)]:
        try:
            mmap.mmap(f.fileno(), 0, flags=flags, prot=prot)
            print("mapped")
        except OSError as e:
            print("mmap:", e.strerror)
'
mount -o remount,noexec m
python3 -c '
import mmap
with open("/tmp/fuse/m/sub2/b", "rb") as f:
    try:
        mmap.mmap(f.fileno(), 0, flags=mmap.MAP_PRIVATE, prot=mmap.PROT_READ | mmap.PROT_EXEC)
    except OSError as e:
        print("noexec mmap:", e.strerror)
'
mount -o remount,exec m
echo "-- upper layer"
# Without RENAME_WHITEOUT (Appine) fuse-overlayfs leaves .wh. files instead of whiteout devices
find u ! -type c ! -name '.wh.*' | sort | sed 's/^/  /'
echo "-- unmount"
umount m && echo unmounted
sleep 1
pgrep -x fuse-overlayfs || echo "daemon gone"
echo "-- daemon killed"
fuse-overlayfs -o lowerdir=/tmp/fuse/l,upperdir=/tmp/fuse/u,workdir=/tmp/fuse/w m
cat m/a.txt
stat -c %F m
pkill -9 -x fuse-overlayfs
sleep 1
ls m 2>&1
stat m 2>&1 | head -1
umount m && echo unmounted
echo "-- mount errors"
python3 -c '
import ctypes, errno, os
libc = ctypes.CDLL(None, use_errno=True)
def mount(data, fstype=b"fuse"):
    r = libc.mount(b"none", b"/tmp/fuse/m", fstype, 0, data)
    print(fstype.decode(), data.decode(), "->", "ok" if r == 0 else errno.errorcode[ctypes.get_errno()])
    if r == 0:
        libc.umount2(b"/tmp/fuse/m", 2)
fd = os.open("/dev/fuse", os.O_RDWR)
for call in [lambda: os.read(fd, 8192), lambda: os.write(fd, bytes(16))]:
    try:
        call()
    except OSError as e:
        print("unattached /dev/fuse:", errno.errorcode[e.errno])
mount(b"rootmode=40000,user_id=0,group_id=0")
mount(b"fd=%d,user_id=0,group_id=0" % fd)
mount(b"fd=%d,rootmode=40000,user_id=0,group_id=0,bogus" % fd)
mount(b"fd=%d,rootmode=170000,user_id=0,group_id=0" % fd)
mount(b"fd=0,rootmode=40000,user_id=0,group_id=0")
mount(b"fd=99,rootmode=40000,user_id=0,group_id=0")
mount(b"fd=%d,rootmode=40000,user_id=0,group_id=0" % fd, b"fuse.")
mount(b"fd=%d,rootmode=40000,user_id=0,group_id=0,blksize=4096" % fd)
mount(b"fd=%d,rootmode=40000,user_id=0,group_id=0,subtype=x" % fd, b"fuse.test")
import fcntl, struct
other = os.open("/dev/fuse", os.O_RDWR)
for old in [99, fd]:
    try:
        fcntl.ioctl(other, 0x8004e500, struct.pack("I", old))
    except OSError as e:
        print("clone of", "a bad fd:" if old == 99 else "an unattached one:", errno.errorcode[e.errno])
mount(b"fd=%d,rootmode=40000,user_id=0,group_id=0" % fd, b"fuse.test")
mount(b"fd=%d" % fd)
try:
    os.read(fd, 100)
except OSError as e:
    print("short read:", errno.errorcode[e.errno])
'
cd /tmp
umount fuse
