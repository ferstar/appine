# docker-run-options: --cap-add SYS_ADMIN --security-opt apparmor=unconfined
# overlayfs nodes of removed names: after thousands of files were made and removed through an
# overlay (enough for Appine to drop the nodes behind them), what still uses a removed name works
# as before: a file open after its unlink, an O_PATH descriptor and a cwd of removed directories,
# /proc/<pid>/exe of a program whose directory was removed, and an open of a FIFO that waits for
# a writer while its name is removed and the writer comes through another link. Not compared:
# the ` (deleted)` Linux adds to that exe link (Appine follows an overlay program by its path,
# kernel/vfs map)
cd /tmp
umount -R /tmp/ovs 2>/dev/null
rm -rf ovs
mkdir ovs
mount -t tmpfs tmpfs ovs
cd ovs
mkdir l u w m
echo lower > l/keep
cat > sweep.c <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

/* Makes and removes `n` files */
static void churn(int n)
{
    char name[32];
    for (int i = 0; i < n; i++) {
        snprintf(name, sizeof name, "tmp%d", i);
        close(open(name, O_CREAT | O_WRONLY, 0644));
        unlink(name);
    }
}

static const char *links(int dirfd, const char *path, int flags)
{
    static char buf[32];
    struct stat st;
    if (fstatat(dirfd, path, &st, flags))
        return strerror(errno);
    snprintf(buf, sizeof buf, "%lu links", (unsigned long)st.st_nlink);
    return buf;
}

static void *reader(void *arg)
{
    int fd = open("fifo", O_RDONLY);
    char buf[16] = "";
    if (fd < 0) {
        printf("fifo open: %s\n", strerror(errno));
        return arg;
    }
    printf("fifo open: %s\n", links(fd, "", AT_EMPTY_PATH));
    struct stat st;
    printf("fifo fchmod: %s", fchmod(fd, 0600) ? strerror(errno) : "ok");
    fstat(fd, &st);
    printf(", mode %o\n", st.st_mode & 07777);
    read(fd, buf, sizeof buf - 1);
    printf("fifo read: %s", buf);
    close(fd);
    return arg;
}

int main(int argc, char **argv)
{
    if (argc > 1) {
        /* The program run from a directory removed later: waits for its parent */
        char c;
        read(0, &c, 1);
        return 0;
    }
    setvbuf(stdout, NULL, _IONBF, 0);
    int held = open("held", O_CREAT | O_RDWR, 0644);
    write(held, "held\n", 5);
    unlink("held");
    mkdir("gone", 0755);
    int opath = open("gone", O_PATH | O_DIRECTORY);
    rmdir("gone");
    /* A child in a directory then removed, and one running a program from another */
    int cwd_pipe[2], exe_pipe[2], done[2];
    pipe(cwd_pipe);
    pipe(exe_pipe);
    pipe(done);
    mkdir("cwd", 0755);
    pid_t cwd_child = fork();
    if (cwd_child == 0) {
        chdir("cwd");
        close(cwd_pipe[0]);
        close(cwd_pipe[1]);
        char c;
        read(done[0], &c, 1);
        char buf[256];
        printf("child cwd: %s, %s\n", getcwd(buf, sizeof buf) ? buf : strerror(errno),
               links(AT_FDCWD, ".", 0));
        return 0;
    }
    mkdir("bin", 0755);
    int in = open("/proc/self/exe", O_RDONLY), out = open("bin/prog", O_CREAT | O_WRONLY, 0755);
    char buf[65536];
    ssize_t n;
    while ((n = read(in, buf, sizeof buf)) > 0)
        write(out, buf, n);
    close(in);
    close(out);
    pid_t exe_child = fork();
    if (exe_child == 0) {
        dup2(exe_pipe[0], 0);
        execl("bin/prog", "prog", "wait", (char *)NULL);
        _exit(127);
    }
    close(exe_pipe[0]);
    usleep(200000);
    unlink("bin/prog");
    rmdir("bin");
    rmdir("cwd");
    churn(3000);
    char path[64], target[256] = "";
    snprintf(path, sizeof path, "/proc/%d/exe", exe_child);
    n = readlink(path, target, sizeof target - 1);
    char *deleted = strstr(target, " (deleted)");
    if (deleted)
        *deleted = 0;
    printf("exe: %s\n", n < 0 ? strerror(errno) : target);
    char data[8] = "";
    pread(held, data, 5, 0);
    printf("held: %s, %s", links(held, "", AT_EMPTY_PATH), data);
    printf("O_PATH: %s\n", links(opath, "", AT_EMPTY_PATH));
    write(done[1], "x", 1);
    write(exe_pipe[1], "x", 1);
    waitpid(cwd_child, NULL, 0);
    waitpid(exe_child, NULL, 0);
    /* A reader waits in open while its name goes; the writer comes through another link */
    mkfifo("fifo", 0644);
    link("fifo", "fifo2");
    pthread_t t;
    pthread_create(&t, NULL, reader, NULL);
    usleep(200000);
    unlink("fifo");
    churn(3000);
    int w = open("fifo2", O_WRONLY);
    write(w, "fifo\n", 5);
    close(w);
    pthread_join(t, NULL);
    unlink("fifo2");
    printf("names left: ");
    fflush(stdout);
    return 0;
}
C
gcc -O1 -pthread -o sweep sweep.c
mount -t overlay overlay -o lowerdir=l,upperdir=u,workdir=w m
cd m
../sweep
ls
cd ..
umount m
cd /tmp
umount ovs
rm -rf ovs
