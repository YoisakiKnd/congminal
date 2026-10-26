/* 测量夹具：固定公开假答案，绝不能作为产品认证 helper。没有 IPC 或通用 UI。 */
#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc >= 4 && strcmp(argv[1], "--launch") == 0) {
        if (argv[0][0] != '/' || argv[3][0] != '/') return 2;
        if (setenv("SSH_ASKPASS", argv[0], 1) || setenv("SSH_ASKPASS_REQUIRE", "force", 1) || setenv("CONGMINAL_ASKPASS_TICKET", argv[2], 1)) return 2;
        execv(argv[3], &argv[3]);
        return 2;
    }
    if (argc != 2) return 2;
    const char *wait = getenv("CONGMINAL_BENCH_AUTH_WAIT_MS");
    if (!wait || (strcmp(wait, "0") && strcmp(wait, "1000"))) return 2;
    struct timespec delay = { strcmp(wait, "1000") == 0 ? 1 : 0, 0 };
    while (nanosleep(&delay, &delay)) if (errno != EINTR) return 2;
    const char answer[] = "fixture only";
    size_t offset = 0;
    while (offset < sizeof(answer) - 1) {
        ssize_t count = write(STDOUT_FILENO, answer + offset, sizeof(answer) - 1 - offset);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) return 2;
        offset += (size_t) count;
    }
    return 0;
}
