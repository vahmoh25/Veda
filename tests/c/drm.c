/* drm — checks of the GPU's render node, /dev/dri/renderD128: Linux's i915
 * interface as Veda's POSIX layer carries it out (lib/posix/src/drm.rs),
 * the way Mesa's iris uses it. They run against gemsim, a stand-in GPU
 * that runs nothing and completes at once whatever it is given; `systest`
 * starts it first. The structures and requests are Linux's
 * (include/uapi/drm/drm.h and i915_drm.h), written out here.
 *
 * Each check prints "ok - NAME" or "not ok - NAME: REASON"; the program
 * exits with the number of failed checks (0: all passed). */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <time.h>
#include <unistd.h>

static int failures;

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

#define CHECK(name, cond, ...)                  \
	do {                                    \
		int ok_ = (cond);               \
		report(name, ok_, __VA_ARGS__); \
	} while (0)

/* ---- Linux's structures and requests ------------------------------------ */

struct drm_version {
	int major, minor, patch;
	size_t name_len;
	char *name;
	size_t date_len;
	char *date;
	size_t desc_len;
	char *desc;
};
struct drm_handle {
	uint32_t handle, pad;
};
struct drm_syncobj_create {
	uint32_t handle, flags;
};
struct drm_syncobj_wait {
	uint64_t handles;
	int64_t timeout_nsec;
	uint32_t count_handles, flags, first_signaled, pad;
	uint64_t deadline_nsec;
};
struct drm_i915_getparam {
	int32_t param;
	int *value;
};
struct drm_i915_gem_create {
	uint64_t size;
	uint32_t handle, pad;
};
struct drm_i915_gem_mmap_offset {
	uint32_t handle, pad;
	uint64_t offset, flags, extensions;
};
struct drm_i915_gem_busy {
	uint32_t handle, busy;
};
struct drm_i915_gem_wait {
	uint32_t bo_handle, flags;
	int64_t timeout_ns;
};
struct i915_user_extension {
	uint64_t next_extension;
	uint32_t name, flags;
	uint32_t rsvd[4];
};
struct drm_i915_gem_context_create_ext {
	uint32_t ctx_id, flags;
	uint64_t extensions;
};
struct drm_i915_gem_context_param {
	uint32_t ctx_id, size;
	uint64_t param, value;
};
struct drm_i915_gem_context_create_ext_setparam {
	struct i915_user_extension base;
	struct drm_i915_gem_context_param param;
};
struct engine_map {
	uint64_t extensions;
	uint16_t engines[2][2];
} __attribute__((packed));
struct drm_i915_gem_exec_object2 {
	uint32_t handle, relocation_count;
	uint64_t relocs_ptr, alignment, offset, flags, rsvd1, rsvd2;
};
struct drm_i915_gem_exec_fence {
	uint32_t handle, flags;
};
struct drm_i915_gem_execbuffer2 {
	uint64_t buffers_ptr;
	uint32_t buffer_count, batch_start_offset, batch_len, DR1, DR4, num_cliprects;
	uint64_t cliprects_ptr, flags, rsvd1, rsvd2;
};
struct drm_i915_query_item {
	uint64_t query_id;
	int32_t length;
	uint32_t flags;
	uint64_t data_ptr;
};
struct drm_i915_query {
	uint32_t num_items, flags;
	uint64_t items_ptr;
};

#define DRM_IOWR_(nr, type) _IOWR('d', nr, type)
#define DRM_IOW_(nr, type) _IOW('d', nr, type)
#define I915(nr) (0x40 + (nr))

#define DRM_IOCTL_VERSION DRM_IOWR_(0x00, struct drm_version)
#define DRM_IOCTL_GEM_CLOSE DRM_IOW_(0x09, struct drm_handle)
#define DRM_IOCTL_SYNCOBJ_CREATE DRM_IOWR_(0xBF, struct drm_syncobj_create)
#define DRM_IOCTL_SYNCOBJ_DESTROY DRM_IOWR_(0xC0, struct drm_handle)
#define DRM_IOCTL_SYNCOBJ_WAIT DRM_IOWR_(0xC3, struct drm_syncobj_wait)
#define DRM_IOCTL_I915_GETPARAM DRM_IOWR_(I915(0x06), struct drm_i915_getparam)
#define DRM_IOCTL_I915_GEM_BUSY DRM_IOWR_(I915(0x17), struct drm_i915_gem_busy)
#define DRM_IOCTL_I915_GEM_CREATE DRM_IOWR_(I915(0x1b), struct drm_i915_gem_create)
#define DRM_IOCTL_I915_GEM_MMAP_OFFSET DRM_IOWR_(I915(0x24), struct drm_i915_gem_mmap_offset)
#define DRM_IOCTL_I915_GEM_EXECBUFFER2 DRM_IOW_(I915(0x29), struct drm_i915_gem_execbuffer2)
#define DRM_IOCTL_I915_GEM_WAIT DRM_IOWR_(I915(0x2c), struct drm_i915_gem_wait)
#define DRM_IOCTL_I915_GEM_CONTEXT_CREATE_EXT DRM_IOWR_(I915(0x2d), struct drm_i915_gem_context_create_ext)
#define DRM_IOCTL_I915_GEM_CONTEXT_DESTROY DRM_IOW_(I915(0x2e), struct drm_handle)
#define DRM_IOCTL_I915_GEM_CONTEXT_GETPARAM DRM_IOWR_(I915(0x34), struct drm_i915_gem_context_param)
#define DRM_IOCTL_I915_QUERY DRM_IOWR_(I915(0x39), struct drm_i915_query)

#define I915_PARAM_CHIPSET_ID 4
#define I915_PARAM_CS_TIMESTAMP_FREQUENCY 51
#define I915_MMAP_OFFSET_WB 2
#define I915_CONTEXT_CREATE_FLAGS_USE_EXTENSIONS 1
#define I915_CONTEXT_PARAM_VM 0x9
#define I915_CONTEXT_PARAM_ENGINES 0xa
#define I915_QUERY_ENGINE_INFO 2
#define I915_EXEC_NO_RELOC (1 << 11)
#define I915_EXEC_HANDLE_LUT (1 << 12)
#define I915_EXEC_BATCH_FIRST (1 << 18)
#define I915_EXEC_FENCE_ARRAY (1 << 19)
#define I915_EXEC_FENCE_WAIT 1
#define I915_EXEC_FENCE_SIGNAL 2
#define EXEC_OBJECT_WRITE (1 << 2)
#define EXEC_OBJECT_SUPPORTS_48B_ADDRESS (1 << 3)
#define EXEC_OBJECT_PINNED (1 << 4)
#define DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL 1
#define DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT 2
/* MI_BATCH_BUFFER_END, then an MI_NOOP. */
#define MI_BATCH_BUFFER_END (0xA << 23)
#ifndef F_DUPFD_QUERY
#define F_DUPFD_QUERY 1027
#endif

static int fd;

static int io(unsigned long request, void *arg)
{
	int r;
	do {
		r = ioctl(fd, request, arg);
	} while (r == -1 && (errno == EINTR || errno == EAGAIN));
	return r;
}

static int64_t now_ns(void)
{
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return (int64_t)t.tv_sec * 1000000000 + t.tv_nsec;
}

static uint32_t gem_create(uint64_t size, uint64_t *got)
{
	struct drm_i915_gem_create c = {.size = size};
	if (io(DRM_IOCTL_I915_GEM_CREATE, &c))
		return 0;
	if (got)
		*got = c.size;
	return c.handle;
}

static void *gem_map(uint32_t handle, size_t size)
{
	struct drm_i915_gem_mmap_offset m = {.handle = handle, .flags = I915_MMAP_OFFSET_WB};
	if (io(DRM_IOCTL_I915_GEM_MMAP_OFFSET, &m))
		return MAP_FAILED;
	return mmap(0, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, (off_t)m.offset);
}

static int gem_close(uint32_t handle)
{
	struct drm_handle c = {.handle = handle};
	return io(DRM_IOCTL_GEM_CLOSE, &c);
}

static uint32_t syncobj(void)
{
	struct drm_syncobj_create c = {0};
	return io(DRM_IOCTL_SYNCOBJ_CREATE, &c) ? 0 : c.handle;
}

/* Waits for syncobj `h` until `timeout` (absolute); 0 or -errno. */
static int syncobj_wait(uint32_t h, int64_t timeout, uint32_t flags)
{
	struct drm_syncobj_wait w = {
		.handles = (uintptr_t)&h, .timeout_nsec = timeout, .count_handles = 1, .flags = flags};
	return io(DRM_IOCTL_SYNCOBJ_WAIT, &w) ? -errno : 0;
}

/* ---- The checks ---------------------------------------------------------- */

static void check_device(void)
{
	struct stat st;
	CHECK("a character device", fstat(fd, &st) == 0 && S_ISCHR(st.st_mode) && major(st.st_rdev) == 226 &&
	      minor(st.st_rdev) == 128, "mode %o", st.st_mode);
	char name[16] = {0};
	struct drm_version v = {0};
	int r = io(DRM_IOCTL_VERSION, &v);
	size_t len = v.name_len;
	v.name = name;
	v.name_len = sizeof name - 1;
	v.date_len = v.desc_len = 0;
	CHECK("version", r == 0 && len == 4 && io(DRM_IOCTL_VERSION, &v) == 0 && !strcmp(name, "i915"),
	      "name '%s' (%zu)", name, len);
	int id = 0, hz = 0;
	struct drm_i915_getparam gp = {.param = I915_PARAM_CHIPSET_ID, .value = &id};
	r = io(DRM_IOCTL_I915_GETPARAM, &gp);
	gp = (struct drm_i915_getparam){.param = I915_PARAM_CS_TIMESTAMP_FREQUENCY, .value = &hz};
	CHECK("parameters", r == 0 && id == 0x46A6 && io(DRM_IOCTL_I915_GETPARAM, &gp) == 0 && hz == 19200000,
	      "device %x, %d Hz", id, hz);
	gp = (struct drm_i915_getparam){.param = 9999, .value = &id};
	CHECK("an unknown parameter", io(DRM_IOCTL_I915_GETPARAM, &gp) == -1 && errno == EINVAL, "accepted");

	/* The engines: how many bytes first, then them. */
	struct drm_i915_query_item item = {.query_id = I915_QUERY_ENGINE_INFO};
	struct drm_i915_query q = {.num_items = 1, .items_ptr = (uintptr_t)&item};
	r = io(DRM_IOCTL_I915_QUERY, &q);
	uint8_t info[256] = {0};
	int need = item.length;
	item.data_ptr = (uintptr_t)info;
	CHECK("engines", r == 0 && need == 16 + 2 * 56 && io(DRM_IOCTL_I915_QUERY, &q) == 0 && info[0] == 2 &&
	      info[16] == 0 && info[16 + 56] == 1, "%d bytes, %d engines", need, info[0]);
}

static void check_buffers(void)
{
	uint64_t size = 0;
	uint32_t a = gem_create(10000, &size);
	CHECK("a buffer", a != 0 && size == 12288, "handle %u, %llu bytes", a, (unsigned long long)size);
	uint32_t *p = gem_map(a, size);
	uint32_t *q = gem_map(a, size);
	int shared = p != MAP_FAILED && q != MAP_FAILED && p != q;
	if (shared) {
		for (int i = 0; i < 3072; i++)
			p[i] = 0xC0DE0000u + i;
		shared = q[0] == 0xC0DE0000u && q[3071] == 0xC0DE0000u + 3071;
	}
	CHECK("its mappings share its memory", shared, "%p %p", (void *)p, (void *)q);
	if (q != MAP_FAILED)
		munmap(q, size);
	/* What was written stays, mapped again. */
	q = gem_map(a, size);
	CHECK("its memory stays", q != MAP_FAILED && q[100] == 0xC0DE0000u + 100, "lost");
	if (q != MAP_FAILED)
		munmap(q, size);
	CHECK("a private mapping is refused",
	      mmap(0, 4096, PROT_READ, MAP_PRIVATE, fd, 0x1ull << 32) == MAP_FAILED && errno == EINVAL, "mapped");
	if (p != MAP_FAILED)
		munmap(p, size);
	/* Handles are reused lowest first, as Linux's. */
	uint32_t b = gem_create(4096, 0);
	CHECK("closing", gem_close(b) == 0 && gem_close(b) == -1 && errno == EINVAL, "closed twice");
	uint32_t c = gem_create(4096, 0);
	CHECK("handles are reused", c == b, "%u after %u", c, b);
	gem_close(c);
	gem_close(a);
}

/* A context on the render and copy engines, in the default context's
 * address space. */
static uint32_t context(void)
{
	struct drm_i915_gem_context_param vm = {.ctx_id = 0, .param = I915_CONTEXT_PARAM_VM};
	if (io(DRM_IOCTL_I915_GEM_CONTEXT_GETPARAM, &vm))
		return 0;
	struct engine_map map = {.engines = {{0, 0}, {1, 0}}};
	struct drm_i915_gem_context_create_ext_setparam set_vm = {
		.param = {.param = I915_CONTEXT_PARAM_VM, .value = vm.value}};
	struct drm_i915_gem_context_create_ext_setparam set_engines = {
		.base = {.next_extension = (uintptr_t)&set_vm},
		.param = {.param = I915_CONTEXT_PARAM_ENGINES, .size = sizeof map, .value = (uintptr_t)&map}};
	struct drm_i915_gem_context_create_ext c = {
		.flags = I915_CONTEXT_CREATE_FLAGS_USE_EXTENSIONS, .extensions = (uintptr_t)&set_engines};
	return io(DRM_IOCTL_I915_GEM_CONTEXT_CREATE_EXT, &c) ? 0 : c.ctx_id;
}

/* Submits `batch` (at 1 MiB) and `target` (at 2 MiB, written) on `engine`
 * of `ctx`, with `fences`; 0 or -errno. */
static int submit(uint32_t ctx, unsigned engine, uint32_t batch, uint32_t target, uint64_t target_at,
		  struct drm_i915_gem_exec_fence *fences, unsigned count)
{
	struct drm_i915_gem_exec_object2 objects[2] = {
		{.handle = batch, .offset = 1 << 20, .flags = EXEC_OBJECT_PINNED | EXEC_OBJECT_SUPPORTS_48B_ADDRESS},
		{.handle = target,
		 .offset = target_at,
		 .flags = EXEC_OBJECT_PINNED | EXEC_OBJECT_SUPPORTS_48B_ADDRESS | EXEC_OBJECT_WRITE},
	};
	struct drm_i915_gem_execbuffer2 e = {
		.buffers_ptr = (uintptr_t)objects,
		.buffer_count = target ? 2 : 1,
		.batch_len = 8,
		.flags = engine | I915_EXEC_NO_RELOC | I915_EXEC_HANDLE_LUT | I915_EXEC_BATCH_FIRST |
			 (count ? I915_EXEC_FENCE_ARRAY : 0),
		.num_cliprects = count,
		.cliprects_ptr = (uintptr_t)fences,
		.rsvd1 = ctx,
	};
	return io(DRM_IOCTL_I915_GEM_EXECBUFFER2, &e) ? -errno : 0;
}

struct waiter {
	uint32_t syncobj;
	int result;
};

static void *wait_for_submission(void *arg)
{
	struct waiter *w = arg;
	w->result = syncobj_wait(w->syncobj, now_ns() + 20000000000ll, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT);
	return 0;
}

static void check_submissions(void)
{
	uint32_t ctx = context();
	CHECK("a context with an engine map", ctx != 0, "refused");
	uint32_t batch = gem_create(4096, 0), target = gem_create(8192, 0);
	uint32_t *b = gem_map(batch, 4096);
	if (b != MAP_FAILED) {
		b[0] = MI_BATCH_BUFFER_END;
		b[1] = 0;
		munmap(b, 4096);
	}
	uint32_t done = syncobj();
	struct drm_i915_gem_exec_fence signal = {.handle = done, .flags = I915_EXEC_FENCE_SIGNAL};
	int r = submit(ctx, 0, batch, target, 2 << 20, &signal, 1);
	CHECK("a submission", r == 0, "%d", r);
	r = syncobj_wait(done, now_ns() + 5000000000ll, 0);
	CHECK("its syncobj signals", r == 0, "%d", r);
	struct drm_i915_gem_wait gw = {.bo_handle = target, .timeout_ns = 5000000000ll};
	struct drm_i915_gem_busy busy = {.handle = target};
	CHECK("its buffers are idle", io(DRM_IOCTL_I915_GEM_WAIT, &gw) == 0 &&
	      io(DRM_IOCTL_I915_GEM_BUSY, &busy) == 0 && busy.busy == 0, "busy %x", busy.busy);
	/* On the copy engine, after the render engine's. */
	struct drm_i915_gem_exec_fence then[2] = {{.handle = done, .flags = I915_EXEC_FENCE_WAIT},
						  {.handle = done, .flags = I915_EXEC_FENCE_SIGNAL}};
	r = submit(ctx, 1, batch, target, 2 << 20, then, 2);
	CHECK("a submission waiting for another", r == 0 && syncobj_wait(done, now_ns() + 5000000000ll, 0) == 0,
	      "%d", r);

	/* Refused: an unknown buffer, buffers that overlap, an engine the
	 * context has not, waiting for a syncobj nothing was submitted for. */
	CHECK("an unknown buffer", submit(ctx, 0, batch, 999, 2 << 20, 0, 0) == -ENOENT, "accepted");
	CHECK("overlapping buffers", submit(ctx, 0, batch, target, 1 << 20, 0, 0) == -EINVAL, "accepted");
	CHECK("an engine the context has not", submit(ctx, 2, batch, 0, 0, 0, 0) == -EINVAL, "accepted");
	uint32_t never = syncobj();
	struct drm_i915_gem_exec_fence wait_never = {.handle = never, .flags = I915_EXEC_FENCE_WAIT};
	CHECK("waiting for nothing submitted", submit(ctx, 0, batch, 0, 0, &wait_never, 1) == -EINVAL, "accepted");

	/* Waiting for a syncobj with no submission: refused, or with
	 * WAIT_FOR_SUBMIT until one comes (from another thread here). */
	CHECK("no submission yet", syncobj_wait(never, now_ns(), 0) == -EINVAL, "waited");
	CHECK("no submission in time",
	      syncobj_wait(never, now_ns() + 1000000, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT) == -ETIME, "waited");
	struct waiter w = {.syncobj = never, .result = 1};
	pthread_t t;
	r = pthread_create(&t, 0, wait_for_submission, &w);
	struct timespec pause = {0, 50000000};
	nanosleep(&pause, 0);
	struct drm_i915_gem_exec_fence signal_never = {.handle = never, .flags = I915_EXEC_FENCE_SIGNAL};
	int s = submit(ctx, 0, batch, 0, 0, &signal_never, 1);
	if (r == 0)
		pthread_join(t, 0);
	CHECK("a wait for a submission that comes", r == 0 && s == 0 && w.result == 0, "%d %d %d", r, s, w.result);

	/* The same file through another descriptor. */
	int copy = fcntl(fd, F_DUPFD_CLOEXEC, 3);
	struct drm_i915_gem_busy busy2 = {.handle = target};
	CHECK("a duplicated descriptor", copy >= 0 && fcntl(fd, F_DUPFD_QUERY, copy) == 1 &&
	      ioctl(copy, DRM_IOCTL_I915_GEM_BUSY, &busy2) == 0, "%d", copy);
	if (copy >= 0)
		close(copy);

	struct drm_handle d = {.handle = never};
	io(DRM_IOCTL_SYNCOBJ_DESTROY, &d);
	d.handle = done;
	io(DRM_IOCTL_SYNCOBJ_DESTROY, &d);
	d.handle = ctx;
	CHECK("destroying the context", io(DRM_IOCTL_I915_GEM_CONTEXT_DESTROY, &d) == 0, "refused");
	gem_close(batch);
	gem_close(target);
}

int main(void)
{
	setvbuf(stdout, 0, _IOLBF, 0);
	/* The GPU service may still be starting: no device until it is. */
	for (int tries = 0; tries < 1000; tries++) {
		fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
		if (fd >= 0 || errno != ENOENT)
			break;
		struct timespec pause = {0, 10000000};
		nanosleep(&pause, 0);
	}
	CHECK("opening the render node", fd >= 0, "no GPU service");
	if (fd >= 0) {
		check_device();
		check_buffers();
		check_submissions();
		close(fd);
	}
	printf("%s: %d failed\n", failures ? "FAIL" : "PASS", failures);
	return failures;
}
