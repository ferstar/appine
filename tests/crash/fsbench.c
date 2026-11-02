// 落盘相关的开销（第 198 轮）：fsbench <目录> <个数>。静态链接，拷进仓库里运行，与原生比
//   create：新建 n 个 4 KB 的文件（不 fsync）
//   replace：再写 n 个临时文件，逐个改名覆盖前面的（apk 装文件的样子）
//   fsync：新建 n 个 4 KB 的文件，每个写完 fsync（dpkg、数据库的样子）
//   rm：删掉这些文件
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static void put(const char *path, int sync) {
    static char buf[4096];
    memset(buf, 'x', sizeof buf);
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0 || write(fd, buf, sizeof buf) != sizeof buf) {
        perror(path);
        exit(1);
    }
    if (sync && fsync(fd) < 0) {
        perror("fsync");
        exit(1);
    }
    close(fd);
}

int main(int argc, char **argv) {
    const char *dir = argv[1];
    int n = atoi(argv[2]);
    char a[512], b[512];
    mkdir(dir, 0755);
    double t = now();
    for (int i = 0; i < n; i++) {
        snprintf(a, sizeof a, "%s/f%d", dir, i);
        put(a, 0);
    }
    double create = now() - t;
    t = now();
    for (int i = 0; i < n; i++) {
        snprintf(a, sizeof a, "%s/t%d", dir, i);
        put(a, 0);
    }
    for (int i = 0; i < n; i++) {
        snprintf(a, sizeof a, "%s/t%d", dir, i);
        snprintf(b, sizeof b, "%s/f%d", dir, i);
        if (rename(a, b) < 0) {
            perror("rename");
            return 1;
        }
    }
    double replace = now() - t;
    t = now();
    for (int i = 0; i < n; i++) {
        snprintf(a, sizeof a, "%s/s%d", dir, i);
        put(a, 1);
    }
    double fs = now() - t;
    t = now();
    for (int i = 0; i < n; i++) {
        snprintf(a, sizeof a, "%s/f%d", dir, i);
        unlink(a);
        snprintf(a, sizeof a, "%s/s%d", dir, i);
        unlink(a);
    }
    rmdir(dir);
    double rm = now() - t;
    printf("create %.0f ms  replace %.0f ms  fsync %.0f ms  rm %.0f ms\n", create * 1e3,
           replace * 1e3, fs * 1e3, rm * 1e3);
    return 0;
}
