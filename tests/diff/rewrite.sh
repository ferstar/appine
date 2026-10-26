# 就地改写之后再执行的程序：同一个 inode 换了代码（cat >、dd conv=notrunc、可写的共享
# 映射），之后执行的必须是新代码。程序的 ctime 离现在超过两秒才会复用上一次的翻译、不再
# 核对代码段，所以改写前后都等一会儿
set -e
cd /tmp
for n in 42 43; do
    printf '#include <stdio.h>\nint main(void) { printf("%%d\\n", %s * 3 + 1); return 0; }\n' $n > v$n.c
    gcc -O2 -o v$n v$n.c
done
cat > patch.c <<'C'
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

/* 经由可写的共享映射把 argv[2] 的内容写进 argv[1]（两个文件一样长） */
int main(int argc, char **argv)
{
    int dst = open(argv[1], O_RDWR), src = open(argv[2], O_RDONLY);
    struct stat st;
    fstat(dst, &st);
    char *d = mmap(NULL, st.st_size, PROT_READ | PROT_WRITE, MAP_SHARED, dst, 0);
    char *s = mmap(NULL, st.st_size, PROT_READ, MAP_PRIVATE, src, 0);
    if (d == MAP_FAILED || s == MAP_FAILED)
        return 1;
    memcpy(d, s, st.st_size);
    munmap(d, st.st_size);
    close(dst);
    return 0;
}
C
gcc -O2 -o patch patch.c
[ "$(stat -c %s v42)" = "$(stat -c %s v43)" ] && echo same size
cp v42 prog
sleep 3
./prog; ./prog
cat v43 > prog
./prog
sleep 3
./prog; ./prog
dd if=v42 of=prog conv=notrunc 2>/dev/null
./prog
sleep 3
./prog; ./prog
./patch prog v43
./prog
sleep 3
./prog; ./prog
