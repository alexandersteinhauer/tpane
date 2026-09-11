/* Headless benchmark host: direct libghostty-vt C API, no renderer or wrappers.
 * Usage: bench-host PTY_FD ROWS BODY_TOP
 *        bench-host --multi ROWS BODY_TOP FD... (zero-based body row, 100 columns).
 * The caller supplies an initially blocking outer master PTY descriptor; this
 * process owns its inherited copy and switches the shared open-file description
 * to nonblocking. The caller must handle EAGAIN on its own input writes too.
 * Link the installed lib/libghostty-vt.a explicitly, not the shared library.
 */
#define _POSIX_C_SOURCE 200809L
#include <ghostty/vt.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

enum { COLS = 100, MARKER_COLS = 80, IO_QUANTUM = 16 * 1024,
       REPLY_CAPACITY = 1024 * 1024 };

struct host {
    GhosttyTerminal terminal;
    int fd;
    int reply_error;
    uint8_t replies[REPLY_CAPACITY];
    size_t reply_head, reply_len;
    uint32_t top;
    char previous[MARKER_COLS + 1];
};

static int api_check(GhosttyResult result, const char *operation)
{
    if (result == GHOSTTY_SUCCESS)
        return 0;
    fprintf(stderr, "bench-host: %s: libghostty result %d\n", operation, (int)result);
    return -1;
}

static void write_pty(GhosttyTerminal terminal, void *userdata,
                      const uint8_t *data, size_t len)
{
    (void)terminal;
    struct host *host = userdata;
    /* Never write inside the parser callback: a client can be blocked writing
     * queries and not yet reading replies (e.g. Zellij's palette discovery). */
    if (host->reply_error || !len)
        return;
    if (len > REPLY_CAPACITY - host->reply_len) {
        host->reply_error = ENOBUFS;
        return;
    }
    size_t tail = (host->reply_head + host->reply_len) % REPLY_CAPACITY;
    size_t first = REPLY_CAPACITY - tail;
    if (first > len)
        first = len;
    memcpy(host->replies + tail, data, first);
    memcpy(host->replies, data + first, len - first);
    host->reply_len += len;
}

/* One nonblocking send per poll iteration preserves read/write fairness. */
static void flush_replies(struct host *host)
{
    if (!host->reply_len || host->reply_error)
        return;
    size_t len = REPLY_CAPACITY - host->reply_head;
    if (len > host->reply_len)
        len = host->reply_len;
    if (len > IO_QUANTUM)
        len = IO_QUANTUM;
    ssize_t count;
    do {
        count = write(host->fd, host->replies + host->reply_head, len);
    } while (count < 0 && errno == EINTR);
    if (count < 0 && (errno == EAGAIN || errno == EWOULDBLOCK))
        return;
    if (count <= 0) {
        host->reply_error = count < 0 ? errno : EIO;
        return;
    }
    host->reply_head = (host->reply_head + (size_t)count) % REPLY_CAPACITY;
    host->reply_len -= (size_t)count;
}

static int check_reply_error(struct host *host)
{
    if (!host->reply_error)
        return 0;
    if (host->reply_error == ENOBUFS)
        fprintf(stderr, "bench-host: terminal reply queue exceeded %d bytes\n",
                REPLY_CAPACITY);
    else
        fprintf(stderr, "bench-host: reply to terminal query: %s\n",
                strerror(host->reply_error));
    return -1;
}

static GhosttyString xtversion(GhosttyTerminal terminal, void *userdata)
{
    (void)terminal;
    (void)userdata;
    static const uint8_t version[] = "ghostty-bench";
    return (GhosttyString){ .ptr = version, .len = sizeof(version) - 1 };
}

static bool device_attributes(GhosttyTerminal terminal, void *userdata,
                              GhosttyDeviceAttributes *attributes)
{
    (void)terminal;
    (void)userdata;
    *attributes = (GhosttyDeviceAttributes){
        .primary = {
            .conformance_level = GHOSTTY_DA_CONFORMANCE_VT220,
            .features = { GHOSTTY_DA_FEATURE_ANSI_COLOR },
            .num_features = 1,
        },
        .secondary = {
            .device_type = GHOSTTY_DA_DEVICE_TYPE_VT220,
            .firmware_version = 1,
        },
    };
    return true;
}

static bool terminal_size(GhosttyTerminal terminal, void *userdata,
                          GhosttySizeReportSize *size)
{
    (void)userdata;
    *size = (GhosttySizeReportSize){0};
    return ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_ROWS,
                               &size->rows) == GHOSTTY_SUCCESS &&
           ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_COLS,
                               &size->columns) == GHOSTTY_SUCCESS;
}

static int host_init(struct host *host, int fd, uint16_t rows, uint32_t top)
{
    memset(host, 0, sizeof(*host));
    host->fd = fd;
    host->top = top;
    GhosttyTerminalOptions options = {
        .cols = COLS, .rows = rows, .max_scrollback = 10000,
    };
    if (api_check(ghostty_terminal_new(NULL, &host->terminal, options),
                  "create terminal") < 0)
        return -1;
    /* Pointer-valued options take the pointer itself, not its address. */
    return api_check(ghostty_terminal_set(host->terminal,
                         GHOSTTY_TERMINAL_OPT_USERDATA, host), "set userdata") ||
           api_check(ghostty_terminal_set(host->terminal,
                         GHOSTTY_TERMINAL_OPT_WRITE_PTY, (const void *)write_pty),
                     "set PTY callback") ||
           api_check(ghostty_terminal_set(host->terminal,
                         GHOSTTY_TERMINAL_OPT_XTVERSION, (const void *)xtversion),
                     "set XTVERSION callback") ||
           api_check(ghostty_terminal_set(host->terminal,
                         GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES,
                         (const void *)device_attributes), "set DA callback") ||
           api_check(ghostty_terminal_set(host->terminal,
                         GHOSTTY_TERMINAL_OPT_SIZE, (const void *)terminal_size),
                     "set size callback");
}

static int codepoint(struct host *host, uint16_t x, uint32_t *out)
{
    GhosttyPoint point = {
        .tag = GHOSTTY_POINT_TAG_ACTIVE,
        .value.coordinate = { .x = x, .y = host->top },
    };
    GhosttyGridRef ref = GHOSTTY_INIT_SIZED(GhosttyGridRef);
    GhosttyCell cell;
    if (api_check(ghostty_terminal_grid_ref(host->terminal, point, &ref),
                  "locate marker cell") < 0 ||
        api_check(ghostty_grid_ref_cell(&ref, &cell), "read marker cell") < 0 ||
        api_check(ghostty_cell_get(cell, GHOSTTY_CELL_DATA_CODEPOINT, out),
                  "read marker codepoint") < 0)
        return -1;
    return 0;
}

/* Returns 1 for a complete new marker, 0 for no change, -1 on API error.
 * Read parsed active-screen cells only, never search raw escape bytes.
 * Dedupe against the last published marker even across ordinary output.
 */
static int next_marker(struct host *host, char text[MARKER_COLS + 1])
{
    bool synchronized;
    if (api_check(ghostty_terminal_mode_get(host->terminal,
                      GHOSTTY_MODE_SYNC_OUTPUT, &synchronized),
                  "read synchronized-output mode") < 0)
        return -1;
    if (synchronized)
        return 0;
    uint32_t cp;
    if (codepoint(host, 0, &cp) < 0)
        return -1;
    if (cp != 'T')
        return 0;
    for (uint16_t x = 0; x < MARKER_COLS; ++x) {
        if (x && codepoint(host, x, &cp) < 0)
            return -1;
        /* Markers are ASCII; blanks or other glyphs cannot complete one. */
        if (cp < '!' || cp > '~')
            return 0;
        text[x] = (char)cp;
        text[x + 1] = '\0';
        if (x == 3 && memcmp(text, "TPB_", 4) != 0)
            return 0;
        if (x >= 7 && memcmp(text + x - 3, "_END", 4) == 0) {
            if (strcmp(text, host->previous) == 0)
                return 0;
            strcpy(host->previous, text);
            return 1;
        }
    }
    return 0;
}

static void host_destroy(struct host *host)
{
    if (host->terminal)
        ghostty_terminal_free(host->terminal);
    host->terminal = NULL;
    if (host->fd >= 0)
        close(host->fd);
    host->fd = -1;
}

static int run(struct host *hosts, size_t count)
{
    struct pollfd *pfds = calloc(count, sizeof(*pfds));
    if (!pfds) {
        perror("bench-host: allocate poll descriptors");
        return -1;
    }
    int status = -1;
    size_t active = count;
    uint8_t buffer[IO_QUANTUM];
    while (active) {
        for (size_t i = 0; i < count; ++i) {
            pfds[i].fd = hosts[i].fd;
            pfds[i].events = POLLIN | (hosts[i].reply_len ? POLLOUT : 0);
        }
        int ready = poll(pfds, (nfds_t)count, -1);
        if (ready < 0 && errno == EINTR)
            continue;
        if (ready < 0) {
            perror("bench-host: poll outer PTYs");
            goto done;
        }
        /* Each ready terminal gets at most one 16 KiB read and write per
         * sweep. A blocked reply consumer cannot stall any other terminal. */
        for (size_t i = 0; i < count; ++i) {
            struct host *host = &hosts[i];
            short events = pfds[i].revents;
            if (!events || host->fd < 0)
                continue;
            if (events & POLLNVAL) {
                fprintf(stderr, "bench-host: invalid PTY fd %d\n", host->fd);
                goto done;
            }
            if (events & POLLOUT)
                flush_replies(host);
            /* A closing peer may reject queued replies before its final
             * output has been drained. Still parse that output until EOF. */
            if ((events & (POLLHUP | POLLERR)) &&
                (host->reply_error == EIO || host->reply_error == EPIPE)) {
                host->reply_error = 0;
                host->reply_head = host->reply_len = 0;
            }
            if (check_reply_error(host) < 0)
                goto done;
            if (!(events & (POLLIN | POLLHUP | POLLERR)))
                continue;
            ssize_t n = read(host->fd, buffer, sizeof(buffer));
            if (n < 0 && (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK))
                continue;
            /* Linux master PTYs signal slave closure with EIO, not EOF. */
            if (n == 0 || (n < 0 && errno == EIO)) {
                host_destroy(host);
                --active;
                continue;
            }
            if (n < 0) {
                perror("bench-host: read outer PTY");
                goto done;
            }
            ghostty_terminal_vt_write(host->terminal, buffer, (size_t)n);
            if (check_reply_error(host) < 0)
                goto done;
            char text[MARKER_COLS + 1];
            int found = next_marker(host, text);
            if (found < 0)
                goto done;
            if (found && (puts(text) == EOF || fflush(stdout) == EOF)) {
                perror("bench-host: publish marker");
                goto done;
            }
        }
    }
    status = 0;
done:
    free(pfds);
    return status;
}

static int reply_self_test(struct host *host)
{
    int sockets[2] = { -1, -1 };
    int status = -1;
    uint8_t block[4096];
    for (size_t i = 0; i < sizeof(block); ++i)
        block[i] = (uint8_t)(i % 251);
#define REPLY_TEST(condition) do { \
    if (!(condition)) { \
        fprintf(stderr, "bench-host: reply self-test failed at line %d\n", __LINE__); \
        goto done; \
    } \
} while (0)
    host->reply_head = REPLY_CAPACITY - 37;
    write_pty(NULL, host, block, sizeof(block));
    REPLY_TEST(host->reply_len == sizeof(block) && !host->reply_error);
    REPLY_TEST(memcmp(host->replies + host->reply_head, block, 37) == 0);
    REPLY_TEST(memcmp(host->replies, block + 37, sizeof(block) - 37) == 0);
    host->reply_head = host->reply_len = 0;
    for (size_t i = 0; i < REPLY_CAPACITY / sizeof(block); ++i)
        write_pty(NULL, host, block, sizeof(block));
    REPLY_TEST(host->reply_len == REPLY_CAPACITY && !host->reply_error);
    write_pty(NULL, host, block, 1);
    REPLY_TEST(host->reply_len == REPLY_CAPACITY && host->reply_error == ENOBUFS);
    host->reply_error = 0;

    REPLY_TEST(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) == 0);
    int send_buffer = 4096;
    REPLY_TEST(setsockopt(sockets[0], SOL_SOCKET, SO_SNDBUF,
                         &send_buffer, sizeof(send_buffer)) == 0);
    for (size_t i = 0; i < 2; ++i) {
        int flags = fcntl(sockets[i], F_GETFL);
        REPLY_TEST(flags >= 0 && fcntl(sockets[i], F_SETFL, flags | O_NONBLOCK) == 0);
    }
    host->fd = sockets[0];
    flush_replies(host);
    REPLY_TEST(!host->reply_error && host->reply_len > 0 &&
               host->reply_len < REPLY_CAPACITY); /* A partial write. */
    for (;;) {
        size_t before = host->reply_len;
        flush_replies(host);
        REPLY_TEST(!host->reply_error && host->reply_len > 0);
        if (before == host->reply_len)
            break; /* EAGAIN must preserve the unsent bytes. */
    }
    size_t received = 0;
    while (received < REPLY_CAPACITY) {
        uint8_t actual[8192];
        ssize_t count = read(sockets[1], actual, sizeof(actual));
        if (count < 0 && errno == EINTR)
            continue;
        REPLY_TEST(count > 0);
        for (ssize_t i = 0; i < count; ++i)
            REPLY_TEST(actual[i] == block[(received + (size_t)i) % sizeof(block)]);
        received += (size_t)count;
        flush_replies(host);
        REPLY_TEST(!host->reply_error);
    }
    REPLY_TEST(!host->reply_len && host->reply_head == 0);
    host->fd = -1;
    write_pty(NULL, host, block, 1);
    flush_replies(host);
    REPLY_TEST(host->reply_error == EBADF && host->reply_len == 1);
    status = 0;
done:
    for (size_t i = 0; i < 2; ++i)
        if (sockets[i] >= 0)
            close(sockets[i]);
    host->fd = -1;
    host->reply_head = host->reply_len = 0;
    host->reply_error = 0;
#undef REPLY_TEST
    return status;
}

static int self_test(void)
{
    struct test {
        const char *input;
        const char *expected;
    };
    static const struct test tests[] = {
        { "\x1b[2J\x1b[Hordinary log line", NULL },
        { "\x1b[HTPB_READY_0000000001_028x100_END", NULL }, /* Wrong row. */
        { "\x1b[4;1HTPB_RE", NULL },
        { "AD\x1b[3", NULL }, /* Fragmented CSI inside the marker. */
        { "1mY_0000000001_028x100_EN", NULL },
        { "D", "TPB_READY_0000000001_028x100_END" },
        { "\x1b[0m", NULL }, /* Same parsed marker, different raw output. */
        { "\x1b[4;1H\x1b[2Kordinary log line", NULL },
        { "\x1b[4;1HTPB_READY_0000000001_028x100_END", NULL },
        { "\x1b[4;1H\x1b[2KTPB_ACK_0000000001_00000001_", NULL },
        { "END", "TPB_ACK_0000000001_00000001_END" },
        { "\x1b[?2026h\x1b[4;1H\x1b[2KTPB_DO", NULL },
        { "\x1b[32mNE_0000000001_00000001_END", NULL },
        { "\x1b[?2026", NULL }, /* Incomplete sync release must not publish. */
        { "l", "TPB_DONE_0000000001_00000001_END" },
        { "\x1b[4;1H\x1b[2KThis is ordinary text_END", NULL },
        { "\x1b[?1049h\x1b[4;1HTPB_ACK_0000000001_00000002_END",
          "TPB_ACK_0000000001_00000002_END" }, /* Active alternate screen. */
    };
    struct host *host = calloc(1, sizeof(*host));
    if (!host) {
        perror("bench-host: allocate self-test terminal");
        return 1;
    }
    int status = 1;
    if (host_init(host, -1, 28, 3) != 0)
        goto done;
    for (size_t i = 0; i < sizeof(tests) / sizeof(tests[0]); ++i) {
        ghostty_terminal_vt_write(host->terminal, (const uint8_t *)tests[i].input,
                                 strlen(tests[i].input));
        char text[MARKER_COLS + 1];
        int found = next_marker(host, text);
        if (found != (tests[i].expected != NULL) ||
            (found == 1 && strcmp(text, tests[i].expected) != 0)) {
            fprintf(stderr, "bench-host: self-test case %zu failed\n", i + 1);
            goto done;
        }
    }
    if (reply_self_test(host) < 0)
        goto done;
    status = 0;
    fprintf(stderr, "bench-host: self-test passed (markers, reply queue, partial send, EAGAIN)\n");
done:
    host_destroy(host);
    free(host);
    return status;
}

static int parse_number(const char *text, unsigned long max, unsigned long *out)
{
    if (*text < '0' || *text > '9')
        return -1;
    char *end;
    errno = 0;
    unsigned long value = strtoul(text, &end, 10);
    if (errno || *end || value > max)
        return -1;
    *out = value;
    return 0;
}

int main(int argc, char **argv)
{
    if (argc == 2 && strcmp(argv[1], "--self-test") == 0)
        return self_test();
    bool multi = argc > 1 && strcmp(argv[1], "--multi") == 0;
    if ((multi && argc < 5) || (!multi && argc != 4))
        goto usage;
    size_t count = multi ? (size_t)argc - 4 : 1;
    struct host *hosts = calloc(count, sizeof(*hosts));
    if (!hosts) {
        perror("bench-host: allocate terminals");
        return 1;
    }
    for (size_t i = 0; i < count; ++i)
        hosts[i].fd = -1;
    int status = 1;
    bool invalid = false;
    /* Collect all owned descriptors before validating/initializing, so every
     * failure path releases them, including failures in a later terminal. */
    for (size_t i = 0; i < count; ++i) {
        unsigned long fd;
        if (parse_number(argv[multi ? i + 4 : 1], INT_MAX, &fd) || fd <= 2) {
            invalid = true;
            continue;
        }
        bool duplicate = false;
        for (size_t j = 0; j < i; ++j)
            if (hosts[j].fd == (int)fd)
                duplicate = true;
        if (duplicate) {
            invalid = true;
            continue;
        }
        hosts[i].fd = (int)fd;
    }
    unsigned long rows, top;
    if (parse_number(argv[2], UINT16_MAX, &rows) || rows == 0 ||
        parse_number(argv[3], UINT32_MAX, &top) || top >= rows)
        invalid = true;
    if (invalid)
        goto done;
    /* Validate every fd before changing any shared open-file description. */
    for (size_t i = 0; i < count; ++i) {
        int fd = hosts[i].fd;
        int flags = fcntl(fd, F_GETFL);
        if (flags < 0) {
            fprintf(stderr, "bench-host: inspect PTY fd %d: %s\n", fd, strerror(errno));
            goto done;
        }
        if ((flags & O_NONBLOCK) || (flags & O_ACCMODE) != O_RDWR || !isatty(fd)) {
            fprintf(stderr, "bench-host: fd %d must be an initially blocking read/write PTY\n", fd);
            goto done;
        }
    }
    if (signal(SIGPIPE, SIG_IGN) == SIG_ERR) {
        perror("bench-host: ignore SIGPIPE");
        goto done;
    }
    for (size_t i = 0; i < count; ++i) {
        int fd = hosts[i].fd;
        int flags = fcntl(fd, F_GETFL);
        if (flags < 0 || fcntl(fd, F_SETFL, flags | O_NONBLOCK) < 0) {
            perror("bench-host: make PTY nonblocking");
            goto done;
        }
        if (host_init(&hosts[i], fd, (uint16_t)rows, (uint32_t)top) != 0)
            goto done;
    }
    status = run(hosts, count) == 0 ? 0 : 1;
done:
    for (size_t i = 0; i < count; ++i)
        host_destroy(&hosts[i]);
    free(hosts);
    if (!invalid)
        return status;
usage:
    fprintf(stderr, "usage: bench-host PTY_FD ROWS BODY_TOP | --multi ROWS BODY_TOP FD... | --self-test\n"
                    "  unique FDs > 2, 1 <= ROWS <= 65535, 0 <= BODY_TOP < ROWS\n");
    return 1;
}
