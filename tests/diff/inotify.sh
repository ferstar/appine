# inotify：目录和文件上的事件及其顺序、rename 的 cookie、覆盖和交换、一次性监视、
# IN_IGNORED、错误码、FIONREAD、poll 和阻塞读；Java 的 WatchService 和 Node.js 的
# fs.watch 建立在它上面
set -e
mkdir -p /tmp/diff-inotify
cd /tmp/diff-inotify
cat > ino.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int in, wd_dir, wd_file, wd_once;
static unsigned last_cookie;

static const char *who(int wd)
{
    return wd == wd_dir ? "dir" : wd == wd_file ? "file" : wd == wd_once ? "once" : "?";
}

static void print_mask(unsigned m)
{
    static const char *names[] = {"ACCESS", "MODIFY", "ATTRIB", "CLOSE_WRITE", "CLOSE_NOWRITE",
                                  "OPEN", "MOVED_FROM", "MOVED_TO", "CREATE", "DELETE",
                                  "DELETE_SELF", "MOVE_SELF"};
    for (int i = 0; i < 12; i++)
        if (m & (1u << i))
            printf(" %s", names[i]);
    if (m & IN_IGNORED)
        printf(" IGNORED");
    if (m & IN_ISDIR)
        printf(" ISDIR");
    if (m & IN_Q_OVERFLOW)
        printf(" OVERFLOW");
}

/* 读出现在排队的所有事件；cookie 只打印和上一个 MOVED_FROM 是否相同 */
static void drain(const char *step)
{
    char buf[4096] __attribute__((aligned(8)));
    printf("%s:", step);
    for (;;) {
        ssize_t n = read(in, buf, sizeof buf);
        if (n < 0) {
            printf(errno == EAGAIN ? "\n" : " read error %d\n", errno);
            return;
        }
        for (char *p = buf; p < buf + n;) {
            struct inotify_event *e = (struct inotify_event *)p;
            printf(" [%s", who(e->wd));
            print_mask(e->mask);
            if (e->len)
                printf(" %s/%u", e->name, e->len);
            if (e->mask & IN_MOVED_FROM)
                last_cookie = e->cookie;
            if (e->mask & IN_MOVED_TO)
                printf(" cookie%s", e->cookie && e->cookie == last_cookie ? "=" : "!=");
            printf("]");
            p += sizeof *e + e->len;
        }
    }
}

int main(void)
{
    char buf[16];
    mkdir("d", 0755);
    in = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    wd_dir = inotify_add_watch(in, "d", IN_ALL_EVENTS);

    int fd = open("d/f", O_CREAT | O_WRONLY, 0644);
    drain("create");
    wd_file = inotify_add_watch(in, "d/f", IN_ALL_EVENTS);
    write(fd, "hello", 5);
    close(fd);
    drain("write+close");
    fd = open("d/f", O_RDONLY);
    read(fd, buf, sizeof buf);
    close(fd);
    drain("read");
    chmod("d/f", 0600);
    utimensat(AT_FDCWD, "d/f", NULL, 0);
    truncate("d/f", 2);
    drain("chmod+utimens+truncate");
    link("d/f", "d/g");
    rename("d/g", "d/h");
    unlink("d/h");
    drain("link+rename+unlink");
    mkdir("d/sub", 0755);
    rmdir("d/sub");
    symlink("f", "d/s");
    mkfifo("d/p", 0644);
    drain("mkdir+rmdir+symlink+mkfifo");
    rename("d/f", "d/f2");
    syscall(SYS_renameat2, AT_FDCWD, "d/s", AT_FDCWD, "d/p", 2 /* RENAME_EXCHANGE */);
    drain("rename+exchange");
    rename("d/p", "d/f2");
    drain("rename over the watched file");
    unlink("d/f2");
    unlink("d/s");
    drain("unlink");

    close(open("d/o", O_CREAT | O_WRONLY, 0644));
    wd_once = inotify_add_watch(in, "d/o", IN_OPEN | IN_ONESHOT);
    close(open("d/o", O_RDONLY));
    close(open("d/o", O_RDONLY));
    drain("oneshot");

    printf("onlydir %d %d\n", inotify_add_watch(in, "d/o", IN_MODIFY | IN_ONLYDIR), errno);
    printf("empty mask %d %d\n", inotify_add_watch(in, "d", 0), errno);
    printf("add+create %d %d\n", inotify_add_watch(in, "d", IN_MODIFY | IN_MASK_ADD | IN_MASK_CREATE), errno);
    printf("mask create %d %d\n", inotify_add_watch(in, "d", IN_MODIFY | IN_MASK_CREATE), errno);
    printf("same wd %d\n", inotify_add_watch(in, "d", IN_CREATE | IN_DELETE | IN_MASK_ADD) == wd_dir);
    printf("not inotify %d %d\n", inotify_add_watch(1, "d", IN_MODIFY), errno);
    printf("missing %d %d\n", inotify_add_watch(in, "d/missing", IN_MODIFY), errno);
    printf("bad wd %d %d\n", inotify_rm_watch(in, 12345), errno);

    unlink("d/o");
    int avail = 0;
    ioctl(in, FIONREAD, &avail);
    printf("fionread %d short read %zd %d\n", avail, read(in, buf, 8), errno);
    drain("after errors");

    /* 阻塞读：子进程过一会儿新建文件 */
    fcntl(in, F_SETFL, 0);
    pid_t pid = fork();
    if (pid == 0) {
        usleep(100000);
        close(open("d/late", O_CREAT | O_WRONLY, 0644));
        _exit(0);
    }
    struct pollfd p = {in, POLLIN, 0};
    printf("poll %d %d\n", poll(&p, 1, 5000), p.revents);
    char ev[256] __attribute__((aligned(8)));
    ssize_t n = read(in, ev, sizeof ev);
    struct inotify_event *e = (struct inotify_event *)ev;
    printf("blocking read %s", n > 0 ? e->name : "?");
    print_mask(e->mask);
    printf("\n");
    waitpid(pid, NULL, 0);
    fcntl(in, F_SETFL, O_NONBLOCK);
    unlink("d/late");
    inotify_rm_watch(in, wd_dir);
    drain("rm_watch");
    return 0;
}
C
gcc -O2 -o ino ino.c
./ino

cat > Watch.java <<'JAVA'
import java.nio.file.*;
import static java.nio.file.StandardWatchEventKinds.*;

public class Watch {
    public static void main(String[] args) throws Exception {
        Path dir = Files.createDirectories(Path.of("/tmp/diff-inotify/jw"));
        WatchService ws = FileSystems.getDefault().newWatchService();
        dir.register(ws, ENTRY_CREATE, ENTRY_MODIFY, ENTRY_DELETE);
        Path f = dir.resolve("a.txt");
        Files.writeString(f, "x");
        Files.delete(f);
        boolean deleted = false;
        while (!deleted) {
            WatchKey key = ws.take();
            for (WatchEvent<?> e : key.pollEvents()) {
                System.out.println("java " + e.kind() + " " + e.context());
                deleted |= e.kind() == ENTRY_DELETE;
            }
            key.reset();
        }
    }
}
JAVA
javac -d . Watch.java
java -cp . Watch

cat > watch.js <<'JS'
const fs = require('fs');
fs.mkdirSync('/tmp/diff-inotify/nw', { recursive: true });
const seen = [];
const w = fs.watch('/tmp/diff-inotify/nw', (event, name) => {
  seen.push(`${event} ${name}`);
  if (seen.length === 3) {
    console.log('node', seen.join(', '));
    w.close();
  }
});
fs.writeFileSync('/tmp/diff-inotify/nw/a.txt', 'x');
fs.unlinkSync('/tmp/diff-inotify/nw/a.txt');
JS
node watch.js
