/*
 * The BKL watchdog (the kernel/process map): a hold of the BKL longer than the watchdog's
 * threshold is reported on the host's stderr while it lasts and in the kernel log once it has
 * ended. The hold is simulated with the test hooks of debug builds: appine-run must set
 * APPINE_TEST_HOOKS=1 and a threshold of 1 s (APPINE_BKL_WATCHDOG=1). Holds the BKL for 2.5 s,
 * then reads the kernel log until the watchdog's record appears and prints it.
 *
 * With the argument "hold" it only holds the BKL (for the Machine tests).
 */
#include <stdio.h>
#include <string.h>
#include <sys/klog.h>
#include <sys/syscall.h>
#include <unistd.h>

#define TEST_SYSCALL 0xa991
#define HOOK_HOLD_BKL 12 /* hold the BKL for the given milliseconds */
#define SYSLOG_ACTION_READ_ALL 3

static char log_buf[1 << 17];

int main(int argc, char **argv)
{
    if (syscall(TEST_SYSCALL, HOOK_HOLD_BKL, 2500) != 0) {
        perror("FAIL: holding the BKL");
        return 1;
    }
    if (argc > 1 && strcmp(argv[1], "hold") == 0)
        return 0;
    /* The record is written at the watchdog's first look after the hold ended */
    for (int i = 0; i < 50; i++) {
        int n = klogctl(SYSLOG_ACTION_READ_ALL, log_buf, sizeof log_buf - 1);
        if (n < 0) {
            perror("FAIL: klogctl");
            return 1;
        }
        log_buf[n] = 0;
        char *found = strstr(log_buf, " held the BKL for ");
        if (found) {
            char *start = found;
            while (start > log_buf && start[-1] != '\n')
                start--;
            char *end = strchr(found, '\n');
            if (end)
                *end = 0;
            printf("%s\nall ok\n", start);
            return 0;
        }
        usleep(100 * 1000);
    }
    printf("FAIL: no record of the hold in the kernel log\n");
    return 1;
}
