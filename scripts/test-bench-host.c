/* Correctness-only integration tests. Build with:
 *   cc -std=c11 -O2 -Wall -Wextra -Werror scripts/test-bench-host.c -o /tmp/test-bench-host
 * Run: /tmp/test-bench-host target/release/bench-host
 * Uses real raw PTYs, bounded I/O deadlines, and no benchmark workloads.
 */
#define _XOPEN_SOURCE 600
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

enum { PANES = 3, TIMEOUT_MS = 5000 };
static int masters[PANES] = { -1, -1, -1 };
static int peers[PANES] = { -1, -1, -1 };
static int output = -1;
static pid_t child = -1;
static const char *binary;

static void cleanup(void)
{
    if (child > 0) {
        kill(child, SIGKILL);
        while (waitpid(child, NULL, 0) < 0 && errno == EINTR) {}
        child = -1;
    }
    for (int i = 0; i < PANES; ++i) {
        if (masters[i] >= 0) close(masters[i]);
        if (peers[i] >= 0) close(peers[i]);
        masters[i] = peers[i] = -1;
    }
    if (output >= 0) close(output);
    output = -1;
}

static void fail(const char *message, int line)
{
    fprintf(stderr, "test-bench-host:%d: %s (errno=%d: %s)\n",
            line, message, errno, strerror(errno));
    cleanup();
    exit(1);
}
#define CHECK(condition) do { if (!(condition)) fail(#condition, __LINE__); } while (0)

static long long now_ms(void)
{
    struct timespec ts;
    CHECK(clock_gettime(CLOCK_MONOTONIC, &ts) == 0);
    return (long long)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static short wait_io(int fd, short events, long long deadline)
{
    for (;;) {
        long long left = deadline - now_ms();
        CHECK(left > 0);
        struct pollfd pfd = { .fd = fd, .events = events };
        int rc = poll(&pfd, 1, (int)left);
        if (rc < 0 && errno == EINTR) continue;
        CHECK(rc > 0 && !(pfd.revents & POLLNVAL));
        return pfd.revents;
    }
}

static void send_bytes(int fd, const char *data, size_t len)
{
    long long deadline = now_ms() + TIMEOUT_MS;
    while (len) {
        wait_io(fd, POLLOUT, deadline);
        ssize_t n = write(fd, data, len);
        if (n < 0 && (errno == EINTR || errno == EAGAIN)) continue;
        CHECK(n > 0);
        data += n;
        len -= (size_t)n;
    }
}

static void send_text(int fd, const char *text)
{
    send_bytes(fd, text, strlen(text));
}

static void expect(int fd, const char *text)
{
    long long deadline = now_ms() + TIMEOUT_MS;
    for (size_t i = 0; text[i];) {
        wait_io(fd, POLLIN, deadline);
        char c;
        ssize_t n = read(fd, &c, 1);
        if (n < 0 && (errno == EINTR || errno == EAGAIN)) continue;
        CHECK(n == 1);
        if (c != text[i]) {
            fprintf(stderr, "expected byte %zu of %s: %02x, got %02x\n",
                    i, text, (unsigned char)text[i], (unsigned char)c);
            fail("unexpected output", __LINE__);
        }
        ++i;
    }
}

static void marker(int pane, const char *text)
{
    send_text(peers[pane], "\x1b[4;1H\x1b[2K");
    send_text(peers[pane], text);
    expect(output, text);
    expect(output, "\n");
}

static void dsr(int pane, int row, int col)
{
    char query[64], reply[64];
    snprintf(query, sizeof(query), "\x1b[%d;%dH\x1b[6n\x1b[5n", row, col);
    snprintf(reply, sizeof(reply), "\x1b[%d;%dR\x1b[0n", row, col);
    send_text(peers[pane], query);
    expect(peers[pane], reply);
}

/* mode: 0 legacy, 1 multi, 2 duplicate fd, 3 invalid fd, 4 invalid geometry,
 * 5 nonblocking fd, 6 zero descriptors. */
static void start(int panes, int mode)
{
    int pipefd[2];
    CHECK(pipe(pipefd) == 0);
    output = pipefd[0];
    for (int i = 0; i < panes; ++i) {
        masters[i] = posix_openpt(O_RDWR | O_NOCTTY);
        CHECK(masters[i] > 2);
        CHECK(grantpt(masters[i]) == 0 && unlockpt(masters[i]) == 0);
        char *name = ptsname(masters[i]);
        CHECK(name != NULL);
        peers[i] = open(name, O_RDWR | O_NOCTTY | O_NONBLOCK);
        CHECK(peers[i] > 2);
        struct termios attrs;
        CHECK(tcgetattr(peers[i], &attrs) == 0);
        attrs.c_iflag &= ~(IGNBRK | BRKINT | PARMRK | ISTRIP | INLCR | IGNCR | ICRNL | IXON);
        attrs.c_oflag &= ~OPOST;
        attrs.c_lflag &= ~(ECHO | ECHONL | ICANON | ISIG | IEXTEN);
        attrs.c_cflag = (attrs.c_cflag & ~(CSIZE | PARENB)) | CS8;
        attrs.c_cc[VMIN] = 1;
        attrs.c_cc[VTIME] = 0;
        CHECK(tcsetattr(peers[i], TCSANOW, &attrs) == 0);
    }
    if (mode == 5)
        CHECK(fcntl(masters[0], F_SETFL, O_RDWR | O_NONBLOCK) == 0);
    child = fork();
    CHECK(child >= 0);
    if (!child) {
        close(pipefd[0]);
        if (dup2(pipefd[1], STDOUT_FILENO) < 0) _exit(126);
        close(pipefd[1]);
        for (int i = 0; i < panes; ++i) close(peers[i]);
        char fds[PANES][32];
        for (int i = 0; i < panes; ++i)
            snprintf(fds[i], sizeof(fds[i]), "%d", masters[i]);
        char *args[PANES + 6] = { (char *)binary, "--multi", "28", "3" };
        for (int i = 0; i < panes; ++i) args[4 + i] = fds[i];
        if (mode == 0) {
            args[1] = fds[0];
            args[4] = NULL;
        } else if (mode == 2) {
            args[5] = fds[0];
        } else if (mode == 3) {
            args[5] = "2147483647";
        } else if (mode == 4) {
            args[3] = "28";
        }
        execv(binary, args);
        _exit(127);
    }
    close(pipefd[1]);
    for (int i = 0; i < panes; ++i) {
        close(masters[i]);
        masters[i] = -1;
    }
}

static void finish(int success)
{
    /* Waiting for stdout EOF has a deadline, unlike waitpid alone. */
    wait_io(output, POLLIN, now_ms() + TIMEOUT_MS);
    char c;
    CHECK(read(output, &c, 1) == 0);
    int status;
    pid_t reaped;
    do { reaped = waitpid(child, &status, 0); } while (reaped < 0 && errno == EINTR);
    CHECK(reaped == child);
    child = -1;
    CHECK(WIFEXITED(status));
    CHECK(success ? WEXITSTATUS(status) == 0 : WEXITSTATUS(status) == 1);
    cleanup();
}

static void multi_test(void)
{
    start(PANES, 1);
    /* Even identical markers must be deduplicated per terminal, not globally. */
    for (int i = 0; i < PANES; ++i)
        marker(i, "TPB_READY_0000000001_028x100_END");
    marker(1, "TPB_READY_0000000002_028x100_END");
    marker(2, "TPB_READY_0000000003_028x100_END");
    /* Ordinary output and redraws must not reset a terminal's dedupe cache. */
    send_text(peers[0], "\x1b[4;1H\x1b[2Kordinary output");
    send_text(peers[0], "\x1b[4;1H\x1b[2KTPB_READY_0000000001_028x100_END");
    dsr(0, 7, 11); /* Roundtrip barrier: the duplicate has been parsed. */
    marker(1, "TPB_ACK_0000000002_00000001_END");

    /* Fragmented parser and sync-output state must not leak between peers. */
    send_text(peers[0], "\x1b[?2026h\x1b[4;1H\x1b[2KTPB_DONE_0000000001_00000001_END\x1b[");
    dsr(1, 8, 12);
    marker(1, "TPB_DONE_0000000002_00000001_END");
    send_text(peers[0], "?2026l");
    expect(output, "TPB_DONE_0000000001_00000001_END\n");
    for (int i = 0; i < PANES; ++i) dsr(i, 10 + i, 20 + i);
    /* Cursor position is independently retained, not just query-local. */
    for (int i = 0; i < PANES; ++i) {
        char reply[32];
        snprintf(reply, sizeof(reply), "\x1b[%d;%dR", 10 + i, 20 + i);
        send_text(peers[i], "\x1b[6n");
        expect(peers[i], reply);
    }

    /* Generate 192 KiB of CPR replies without reading peer 0. This exceeds
     * PTY buffering but stays below its bounded 1 MiB host reply queue. */
    char queries[4 * 24000];
    for (size_t i = 0; i < sizeof(queries); i += 4)
        memcpy(queries + i, "\x1b[6n", 4);
    send_bytes(peers[0], queries, sizeof(queries));
    marker(0, "TPB_ACK_0000000001_00000002_END");
    dsr(1, 15, 21);
    marker(1, "TPB_ACK_0000000002_00000002_END");
    dsr(2, 16, 22);
    marker(2, "TPB_ACK_0000000003_00000002_END");

    /* Closure with pending replies must neither abort nor hang other peers. */
    close(peers[0]); peers[0] = -1;
    dsr(1, 18, 25);
    marker(1, "TPB_DONE_0000000002_00000002_END");
    close(peers[1]); peers[1] = -1;
    dsr(2, 19, 26);
    marker(2, "TPB_DONE_0000000003_00000002_END");
    close(peers[2]); peers[2] = -1;
    finish(1);
}

int main(int argc, char **argv)
{
    CHECK(argc == 2);
    binary = argv[1];
    CHECK(signal(SIGPIPE, SIG_IGN) != SIG_ERR);
    /* Legacy invocation and --multi with N=1 are both supported. */
    for (int mode = 0; mode <= 1; ++mode) {
        start(1, mode);
        marker(0, "TPB_READY_0000000001_028x100_END");
        dsr(0, 7, 11);
        close(peers[0]); peers[0] = -1;
        finish(1);
    }
    multi_test();
    for (int mode = 2; mode <= 6; ++mode) {
        start(mode == 6 ? 0 : mode <= 3 ? 2 : 1, mode);
        finish(0);
    }
    fprintf(stderr, "test-bench-host: passed (legacy/multi CLI, independent parser/screen/dedupe/DSR, blocked replies, peer closure, invalid arguments)\n");
    return 0;
}
