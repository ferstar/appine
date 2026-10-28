/*
 * 线程频繁开始和退出的 guest 测试程序：4 个进程，每个进程里 4 个线程不停地创建、join
 * 短命的线程，跑 argv[1] 秒（默认 5）。线程退出时在 BKL 外丢掉 Rc 的弱引用，会和别的
 * 线程开始时的 Rc::downgrade 一起改同一个计数，几秒之内宿主的堆就被破坏（docs/rounds/
 * 71-thread-exit-race.md）。用 musl-gcc -static 编译，在 appine-run 里直接运行。全部
 * 正常时最后打印 "all ok"。
 */
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

static atomic_int stop;

static void *leaf(void *arg)
{
    return arg;
}

static void *spawner(void *arg)
{
    long n = 0;
    (void)arg;
    while (!atomic_load(&stop)) {
        pthread_t t;
        if (pthread_create(&t, NULL, leaf, NULL) == 0) {
            pthread_join(t, NULL);
            n++;
        }
    }
    return (void *)n;
}

int main(int argc, char **argv)
{
    int seconds = argc > 1 ? atoi(argv[1]) : 5;
    for (int p = 0; p < 4; p++) {
        pid_t pid = fork();
        if (pid < 0) {
            perror("fork");
            return 1;
        }
        if (pid == 0) {
            pthread_t t[4];
            long total = 0;
            for (int i = 0; i < 4; i++)
                pthread_create(&t[i], NULL, spawner, NULL);
            sleep(seconds);
            atomic_store(&stop, 1);
            for (int i = 0; i < 4; i++) {
                void *r;
                pthread_join(t[i], &r);
                total += (long)r;
            }
            _exit(total > 0 ? 0 : 1);
        }
    }
    int status, failed = 0;
    while (wait(&status) > 0)
        failed += !(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    if (failed) {
        printf("%d processes failed\n", failed);
        return 1;
    }
    printf("all ok\n");
    return 0;
}
