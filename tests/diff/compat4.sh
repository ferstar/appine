# The sequel to compat3.sh (difftest passes the whole script as one argument of sh -c, which must
# not exceed 128 KB): MADV_DONTNEED on private file mappings
set -e
cd /tmp
# MADV_DONTNEED on a private file mapping drops the written copies: the pages read the file again
# (zeros past its end in the last page), also for a mapping made writable after being read-only, in
# a forked child, and for a range starting inside a page run
cat > dontneedfile.c <<'C'
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>
#include <fcntl.h>

static void show(const char *what, const unsigned char *p, size_t len) {
    size_t a = 0, b = 0, z = 0, other = 0;
    for (size_t i = 0; i < len; i++) {
        if (p[i] == 'a') a++;
        else if (p[i] == 'b') b++;
        else if (p[i] == 0) z++;
        else other++;
    }
    printf("%s: a=%zu b=%zu zero=%zu other=%zu\n", what, a, b, z, other);
}

int main(void) {
    // 3 pages and a bit: the last page holds 100 bytes of the file
    size_t flen = 3 * 4096 + 100, mlen = 4 * 4096;
    int fd = open("dnfile", O_RDWR | O_CREAT | O_TRUNC, 0644);
    unsigned char *buf = malloc(flen);
    memset(buf, 'a', flen);
    if (write(fd, buf, flen) != (ssize_t)flen) return 1;
    // Old enough that a read-only mapping may share a cached host mapping
    sleep(3);

    unsigned char *p = mmap(NULL, mlen, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    memset(p, 'b', mlen);
    show("written", p, mlen);
    printf("dontneed %d\n", madvise(p, mlen, MADV_DONTNEED));
    show("after", p, mlen);
    // Only a middle part
    memset(p, 'b', mlen);
    printf("dontneed middle %d\n", madvise(p + 4096, 4096, MADV_DONTNEED));
    show("page 0", p, 4096);
    show("page 1", p + 4096, 4096);
    show("page 2", p + 8192, 4096);

    // Read-only first, then writable
    unsigned char *q = mmap(NULL, mlen, PROT_READ, MAP_PRIVATE, fd, 0);
    show("read-only", q, mlen);
    mprotect(q, mlen, PROT_READ | PROT_WRITE);
    memset(q, 'b', mlen);
    printf("dontneed %d\n", madvise(q, mlen, MADV_DONTNEED));
    show("made writable", q, mlen);

    // In a forked child, the child's copies go and the parent's stay
    memset(p, 'b', mlen);
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        printf("child dontneed %d\n", madvise(p, mlen, MADV_DONTNEED));
        show("child", p, mlen);
        fflush(stdout);
        _exit(0);
    }
    waitpid(pid, NULL, 0);
    show("parent", p, mlen);

    // The file changed: the dropped pages show the new contents
    memset(p, 'b', mlen);
    pwrite(fd, "bbbb", 4, 0);
    madvise(p, 4096, MADV_DONTNEED);
    printf("first bytes %.6s\n", p);
    return 0;
}
C
gcc -O1 -o dontneedfile dontneedfile.c && ./dontneedfile
