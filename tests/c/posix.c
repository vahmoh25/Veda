/* posix — checks of Veda's POSIX layer through the C library.
 *
 * Each check prints "ok - NAME" or "not ok - NAME: REASON"; the program
 * exits with the number of failed checks (0: all passed). `systest` runs it
 * inside Veda in a scratch directory, with its standard output on a pipe.
 *
 * `posix child MODE` is the program run by the spawning checks. */

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <spawn.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

extern char **environ;

static int failures;
static char program[4096];

static void report(const char *name, int ok, const char *fmt, ...)
{
	if (ok) {
		printf("ok - %s\n", name);
		return;
	}
	failures++;
	printf("not ok - %s: ", name);
	va_list ap;
	va_start(ap, fmt);
	vprintf(fmt, ap);
	va_end(ap);
	printf(" (errno %d: %s)\n", errno, strerror(errno));
}

/* The condition first: the message may show what it computed. */
#define CHECK(name, cond, ...)                  \
	do {                                    \
		int ok_ = (cond);               \
		report(name, ok_, __VA_ARGS__); \
	} while (0)

static void check_stdio_and_strings(void)
{
	char buf[128];
	int n = snprintf(buf, sizeof buf, "%d %s %.3f %x %c", -42, "veda", 3.14159, 255, 'Z');
	CHECK("snprintf", n == 19 && !strcmp(buf, "-42 veda 3.142 ff Z"), "got '%s' (%d)", buf, n);
	double d = strtod("2.5e3", 0);
	CHECK("strtod", d == 2500.0, "got %f", d);
	long l = strtol("-0x7f", 0, 16);
	CHECK("strtol", l == -127, "got %ld", l);
	int v[] = { 5, 3, 9, 1, 7 };
	int (*cmp)(const void *, const void *) = 0;
	(void)cmp;
	for (int i = 0; i < 5; i++)
		for (int j = i + 1; j < 5; j++)
			if (v[j] < v[i]) { int t = v[i]; v[i] = v[j]; v[j] = t; }
	CHECK("arrays", v[0] == 1 && v[4] == 9, "unsorted");
	struct utsname u;
	CHECK("uname", uname(&u) == 0 && !strcmp(u.sysname, "Veda") && !strcmp(u.machine, "x86_64"),
	      "sysname '%s' machine '%s'", u.sysname, u.machine);
}

static void check_heap(void)
{
	void *small[1000];
	for (int i = 0; i < 1000; i++) {
		small[i] = malloc(16 + i);
		memset(small[i], i, 16 + i);
	}
	int ok = 1;
	for (int i = 0; i < 1000; i++) {
		if (((unsigned char *)small[i])[15 + i] != (unsigned char)i)
			ok = 0;
		free(small[i]);
	}
	CHECK("malloc small blocks", ok, "contents changed");
	char *big = malloc(64 << 20);
	CHECK("malloc 64 MiB", big != 0, "malloc failed");
	if (big) {
		big[0] = 1;
		big[(64 << 20) - 1] = 2;
		char *more = realloc(big, 96 << 20);
		CHECK("realloc", more && more[0] == 1 && more[(64 << 20) - 1] == 2, "contents lost");
		free(more ? more : big);
	}
	int *z = calloc(1000, sizeof(int));
	int zero = 1;
	for (int i = 0; z && i < 1000; i++)
		if (z[i])
			zero = 0;
	CHECK("calloc", z && zero, "not zero");
	free(z);
}

extern void _start(void);

/* What the loader put on the stack beside the arguments and environment. */
static void check_startup(void)
{
	const char *execfn = (const char *)getauxval(AT_EXECFN);
	const char *platform = (const char *)getauxval(AT_PLATFORM);
	CHECK("auxiliary vector", getauxval(AT_PAGESZ) == 4096 && sysconf(_SC_PAGESIZE) == 4096 &&
	      getauxval(AT_PHDR) && getauxval(AT_PHNUM) && getauxval(AT_RANDOM) && !getauxval(AT_SECURE) &&
	      execfn && *execfn == '/' && platform && !strcmp(platform, "x86_64"), "missing or wrong entries");
	/* Where the program was loaded: right for this program's own code. */
	CHECK("entry point", getauxval(AT_ENTRY) == (unsigned long)_start, "AT_ENTRY %#lx, _start %p",
	      getauxval(AT_ENTRY), (void *)_start);
}

static void check_mmap(void)
{
	size_t len = 3 * 4096;
	unsigned char *p = mmap(0, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	CHECK("mmap anonymous", p != MAP_FAILED && p[0] == 0 && p[len - 1] == 0, "map failed or not zero");
	if (p == MAP_FAILED)
		return;
	memset(p, 0xab, len);
	CHECK("mprotect part of a mapping", mprotect(p + 4096, 4096, PROT_READ) == 0, "mprotect failed");
	CHECK("munmap part of a mapping", munmap(p + 8192, 4096) == 0 && p[0] == 0xab, "munmap failed");
	CHECK("munmap", munmap(p, 8192) == 0, "munmap failed");
	void *none = mmap(0, 1 << 20, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	CHECK("mmap reservation", none != MAP_FAILED && mprotect(none, 4096, PROT_READ | PROT_WRITE) == 0,
	      "reserve then commit failed");
	if (none != MAP_FAILED) {
		char *r = none;
		r[100] = 7;
		/* Giving pages back, as allocators do: they read as zeros. */
		CHECK("madvise MADV_DONTNEED", madvise(r, 4096, MADV_DONTNEED) == 0 && r[100] == 0, "not dropped");
		CHECK("decommit a reservation",
		      mprotect(r, 4096, PROT_NONE) == 0 && madvise(r, 1 << 20, MADV_DONTNEED) == 0,
		      "madvise failed");
		munmap(none, 1 << 20);
	}
	/* Shared memory has nowhere to take its contents back from. */
	char *shared = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
	CHECK("mmap shared", shared != MAP_FAILED, "map failed");
	if (shared != MAP_FAILED) {
		shared[0] = 5;
		errno = 0;
		CHECK("MADV_DONTNEED keeps shared memory",
		      madvise(shared, 4096, MADV_DONTNEED) == -1 && errno == EINVAL && shared[0] == 5,
		      "contents dropped");
		munmap(shared, 4096);
	}
}

static void check_files(void)
{
	FILE *f = fopen("notes.txt", "w");
	CHECK("fopen for writing", f != 0, "fopen failed");
	if (!f)
		return;
	for (int i = 0; i < 1000; i++)
		fprintf(f, "line %d\n", i);
	CHECK("fclose", fclose(f) == 0, "fclose failed");

	struct stat st;
	CHECK("stat", stat("notes.txt", &st) == 0 && S_ISREG(st.st_mode) && st.st_size == 8890,
	      "size %lld", (long long)st.st_size);
	f = fopen("notes.txt", "r");
	char line[64];
	int lines = 0, last = -1;
	while (f && fgets(line, sizeof line, f)) {
		lines++;
		sscanf(line, "line %d", &last);
	}
	CHECK("fgets", lines == 1000 && last == 999, "%d lines, last %d", lines, last);
	CHECK("fseek and ftell", f && fseek(f, 7, SEEK_SET) == 0 && ftell(f) == 7 && fgetc(f) == 'l',
	      "position wrong");
	if (f)
		fclose(f);

	int fd = open("raw.bin", O_RDWR | O_CREAT | O_TRUNC, 0644);
	CHECK("open", fd >= 0, "open failed");
	char block[100000];
	for (size_t i = 0; i < sizeof block; i++)
		block[i] = (char)(i * 7);
	CHECK("write large", write(fd, block, sizeof block) == (ssize_t)sizeof block, "short write");
	CHECK("lseek end", lseek(fd, 0, SEEK_END) == (off_t)sizeof block, "wrong size");
	char back[100000];
	CHECK("pread", pread(fd, back, sizeof back, 0) == (ssize_t)sizeof back && !memcmp(back, block, sizeof back),
	      "contents differ");
	CHECK("ftruncate", ftruncate(fd, 10) == 0 && fstat(fd, &st) == 0 && st.st_size == 10, "size %lld",
	      (long long)st.st_size);
	int fd2 = dup(fd);
	lseek(fd, 3, SEEK_SET);
	CHECK("dup shares the offset", lseek(fd2, 0, SEEK_CUR) == 3, "offset %lld", (long long)lseek(fd2, 0, SEEK_CUR));
	close(fd2);

	int ex = open("raw.bin", O_WRONLY | O_CREAT | O_EXCL, 0644);
	CHECK("O_EXCL", ex < 0 && errno == EEXIST, "opened an existing file");
	int ap = open("raw.bin", O_WRONLY | O_APPEND);
	write(ap, "tail", 4);
	close(ap);
	CHECK("O_APPEND", fstat(fd, &st) == 0 && st.st_size == 14, "size %lld", (long long)st.st_size);

	/* A file removed while open stays readable. */
	CHECK("unlink while open", unlink("raw.bin") == 0 && access("raw.bin", F_OK) != 0, "still there");
	char head[4] = { 0 };
	CHECK("read after unlink", pread(fd, head, 3, 0) == 3 && head[1] == block[1], "unreadable");
	close(fd);

	CHECK("missing file", open("no-such-file", O_RDONLY) < 0 && errno == ENOENT, "wrong error");

	f = fopen("a.txt", "w");
	fputs("A", f);
	fclose(f);
	f = fopen("b.txt", "w");
	fputs("B", f);
	fclose(f);
	CHECK("rename over a file", rename("a.txt", "b.txt") == 0, "rename failed");
	f = fopen("b.txt", "r");
	CHECK("renamed contents", f && fgetc(f) == 'A' && access("a.txt", F_OK) != 0, "old contents");
	if (f)
		fclose(f);

	struct stat prog;
	CHECK("programs are executable", stat(program, &prog) == 0 && (prog.st_mode & S_IXUSR) &&
	      access(program, X_OK) == 0, "mode %o", prog.st_mode);
	CHECK("text is not executable", access("b.txt", X_OK) != 0, "b.txt executable");
	CHECK("inodes differ", stat("b.txt", &st) == 0 && st.st_ino != prog.st_ino, "same inode");

	fd = open("/dev/null", O_RDWR);
	CHECK("/dev/null", fd >= 0 && write(fd, "x", 1) == 1 && read(fd, line, 1) == 0, "not a null device");
	close(fd);
	fd = open("/dev/urandom", O_RDONLY);
	unsigned char r[32] = { 0 };
	int nonzero = 0;
	if (fd >= 0 && read(fd, r, sizeof r) == sizeof r)
		for (int i = 0; i < 32; i++)
			nonzero |= r[i];
	CHECK("/dev/urandom", nonzero, "no random bytes");
	close(fd);
}

static void check_directories(void)
{
	char cwd[4096];
	CHECK("mkdir", mkdir("sub", 0755) == 0 && mkdir("sub/deeper", 0755) == 0, "mkdir failed");
	CHECK("mkdir existing", mkdir("sub", 0755) != 0 && errno == EEXIST, "made it twice");
	FILE *f = fopen("sub/one", "w");
	fclose(f);
	DIR *d = opendir("sub");
	int seen = 0, dot = 0;
	struct dirent *e;
	while (d && (e = readdir(d))) {
		if (!strcmp(e->d_name, "one") && e->d_type == DT_REG)
			seen |= 1;
		if (!strcmp(e->d_name, "deeper") && e->d_type == DT_DIR)
			seen |= 2;
		if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, ".."))
			dot++;
	}
	if (d)
		closedir(d);
	CHECK("readdir", seen == 3 && dot == 2, "seen %d, dots %d", seen, dot);
	char *before = getcwd(cwd, sizeof cwd);
	CHECK("chdir", before && chdir("sub/deeper") == 0, "chdir failed");
	char now[4096];
	CHECK("getcwd", getcwd(now, sizeof now) && strlen(now) > strlen(cwd) &&
	      !strcmp(now + strlen(now) - 10, "sub/deeper"), "now '%s'", now);
	CHECK("relative ..", access("../one", F_OK) == 0 && chdir("../..") == 0, "can't go back");
	CHECK("rmdir non-empty", rmdir("sub") != 0 && errno == ENOTEMPTY, "removed");
	CHECK("rmdir", unlink("sub/one") == 0 && rmdir("sub/deeper") == 0 && rmdir("sub") == 0, "rmdir failed");
}

static void check_pipes(void)
{
	int p[2];
	CHECK("pipe", pipe(p) == 0, "pipe failed");
	const char msg[] = "through the pipe";
	CHECK("pipe write", write(p[1], msg, sizeof msg) == sizeof msg, "short write");
	char buf[64];
	CHECK("pipe read", read(p[0], buf, sizeof buf) == sizeof msg && !strcmp(buf, msg), "got '%s'", buf);
	close(p[1]);
	CHECK("pipe end of file", read(p[0], buf, sizeof buf) == 0, "no EOF");
	close(p[0]);

	CHECK("pipe2 nonblocking", pipe2(p, O_NONBLOCK | O_CLOEXEC) == 0 && read(p[0], buf, 1) < 0 && errno == EAGAIN,
	      "read did not fail with EAGAIN");
	CHECK("close-on-exec", fcntl(p[0], F_GETFD) == FD_CLOEXEC, "flag missing");
	signal(SIGPIPE, SIG_IGN);
	close(p[0]);
	CHECK("EPIPE", write(p[1], "x", 1) < 0 && errno == EPIPE, "write to a closed pipe worked");
	close(p[1]);
	signal(SIGPIPE, SIG_DFL);
}

static __thread int tls_value = 5;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static int counter, ready;
static atomic_int atomic_counter;

static void *worker(void *arg)
{
	tls_value += (int)(intptr_t)arg;
	for (int i = 0; i < 10000; i++) {
		pthread_mutex_lock(&lock);
		counter++;
		pthread_mutex_unlock(&lock);
		atomic_fetch_add(&atomic_counter, 1);
	}
	pthread_mutex_lock(&lock);
	ready++;
	pthread_cond_signal(&cond);
	pthread_mutex_unlock(&lock);
	return (void *)(intptr_t)tls_value;
}

static void check_threads(void)
{
	pthread_t t[8];
	int started = 1;
	for (int i = 0; i < 8; i++)
		if (pthread_create(&t[i], 0, worker, (void *)(intptr_t)(i + 1)))
			started = 0;
	CHECK("pthread_create", started, "creation failed");
	pthread_mutex_lock(&lock);
	while (ready < 8)
		pthread_cond_wait(&cond, &lock);
	pthread_mutex_unlock(&lock);
	int tls_ok = 1;
	for (int i = 0; i < 8; i++) {
		void *r;
		pthread_join(t[i], &r);
		if ((intptr_t)r != 5 + i + 1)
			tls_ok = 0;
	}
	CHECK("mutexes", counter == 80000, "counter %d", counter);
	CHECK("atomics", atomic_counter == 80000, "counter %d", atomic_counter);
	CHECK("thread-local storage", tls_ok && tls_value == 5, "values mixed up");
	pthread_t d;
	CHECK("detached thread", pthread_create(&d, 0, worker, 0) == 0 && pthread_detach(d) == 0, "failed");
	struct timespec ts = { 0, 50 * 1000 * 1000 };
	nanosleep(&ts, 0);
}

static void check_time(void)
{
	struct timespec a, b;
	clock_gettime(CLOCK_MONOTONIC, &a);
	struct timespec ts = { 0, 20 * 1000 * 1000 };
	nanosleep(&ts, 0);
	clock_gettime(CLOCK_MONOTONIC, &b);
	long ms = (b.tv_sec - a.tv_sec) * 1000 + (b.tv_nsec - a.tv_nsec) / 1000000;
	CHECK("nanosleep", ms >= 19 && ms < 2000, "slept %ld ms", ms);
	time_t now = time(0);
	CHECK("time", now > 1700000000, "time %lld", (long long)now);
	struct timeval tv;
	CHECK("gettimeofday", gettimeofday(&tv, 0) == 0 && tv.tv_sec >= now - 2, "behind time()");
}

static volatile sig_atomic_t got_signal;
static jmp_buf jump;

static void on_signal(int s)
{
	got_signal = s;
}

static void check_signals(void)
{
	signal(SIGUSR1, on_signal);
	raise(SIGUSR1);
	CHECK("raise", got_signal == SIGUSR1, "handler not run");
	sigset_t set, old;
	sigemptyset(&set);
	sigaddset(&set, SIGUSR2);
	signal(SIGUSR2, on_signal);
	got_signal = 0;
	sigprocmask(SIG_BLOCK, &set, &old);
	raise(SIGUSR2);
	int blocked = got_signal == 0;
	sigprocmask(SIG_SETMASK, &old, 0);
	CHECK("blocked signals wait", blocked && got_signal == SIGUSR2, "delivered at the wrong time");
	volatile int jumped = 0;
	if (setjmp(jump) == 0)
		longjmp(jump, 1);
	else
		jumped = 1;
	CHECK("setjmp/longjmp", jumped, "no jump");
}

/* Starts `program child MODE` with its output on a pipe; returns what it
   wrote and stores its wait status. */
static int spawn_child(const char *mode, char *out, size_t cap, int *status)
{
	int p[2];
	if (pipe(p))
		return -1;
	posix_spawn_file_actions_t fa;
	posix_spawn_file_actions_init(&fa);
	posix_spawn_file_actions_adddup2(&fa, p[1], 1);
	posix_spawn_file_actions_addclose(&fa, p[0]);
	char *argv[] = { "posix", "child", (char *)mode, 0 };
	char *envp[] = { "VEDA_TEST=from-parent", 0 };
	pid_t pid;
	int r = posix_spawn(&pid, program, &fa, 0, argv, envp);
	posix_spawn_file_actions_destroy(&fa);
	close(p[1]);
	if (r) {
		close(p[0]);
		errno = r;
		return -1;
	}
	size_t n = 0;
	ssize_t got;
	while (n + 1 < cap && (got = read(p[0], out + n, cap - 1 - n)) > 0)
		n += got;
	out[n] = 0;
	close(p[0]);
	return waitpid(pid, status, 0) == pid ? 0 : -1;
}

static void check_processes(void)
{
	char out[4096];
	int status = 0;
	int r = spawn_child("echo", out, sizeof out, &status);
	CHECK("posix_spawn", r == 0, "spawn failed");
	CHECK("child output", !strcmp(out, "child says hello from-parent\n"), "got '%s'", out);
	CHECK("child exit status", WIFEXITED(status) && WEXITSTATUS(status) == 7, "status %#x", status);
	r = spawn_child("abort", out, sizeof out, &status);
	CHECK("child killed by a signal", r == 0 && WIFSIGNALED(status) && WTERMSIG(status) == SIGABRT,
	      "status %#x", status);
	CHECK("child sees the directory", spawn_child("cwd", out, sizeof out, &status) == 0 &&
	      WEXITSTATUS(status) == 0, "child status %#x: '%s'", status, out);
	FILE *p = popen("missing-program", "r");
	CHECK("popen of a missing shell", p == 0 || pclose(p) != 0, "succeeded");
	CHECK("waitpid without children", waitpid(-1, &status, WNOHANG) < 0 && errno == ECHILD, "no ECHILD");
	char *e = getenv("HOME");
	CHECK("environment", e && *e == '/', "HOME is '%s'", e ? e : "(unset)");
}

static int child(const char *mode)
{
	if (!strcmp(mode, "echo")) {
		printf("child says hello %s\n", getenv("VEDA_TEST") ? getenv("VEDA_TEST") : "(no env)");
		return 7;
	}
	if (!strcmp(mode, "abort"))
		abort();
	if (!strcmp(mode, "cwd")) {
		/* The parent's scratch directory, where notes.txt is. */
		return access("notes.txt", R_OK) == 0 ? 0 : 1;
	}
	return 99;
}

int main(int argc, char **argv)
{
	if (argc >= 3 && !strcmp(argv[1], "child"))
		return child(argv[2]);
	/* Where this program is, for the checks that start it again. */
	const char *self = (const char *)getauxval(AT_EXECFN);
	snprintf(program, sizeof program, "%s", self && *self == '/' ? self : "/system/tests/c/posix");
	setvbuf(stdout, 0, _IOLBF, 0);
	printf("posix: %s, tty %d\n", program, isatty(1));
	check_startup();
	check_stdio_and_strings();
	check_heap();
	check_mmap();
	check_files();
	check_directories();
	check_pipes();
	check_threads();
	check_time();
	check_signals();
	check_processes();
	printf("%s: %d failed\n", failures ? "FAIL" : "PASS", failures);
	return failures;
}
