/* cflags: -ffreestanding -nostdlibinc -Ilibuv/stub -Ilibuv/include -Ilibuv/src -DUV_TEST_TEARDOWN */
/* libuv's async handle torn down while another thread sends on it
   (libuv/src/uv-common.c and libuv/src/unix/async.c, unchanged from
   v1.53.0). uv_async_send sets the pending flag and adds 2 to a busy
   counter in one compare-and-swap, writes to the loop's eventfd (loading
   handle->loop and then loop->async_io_watcher.fd), and then subtracts 2.
   uv__async_spin, which uv_close and loop teardown run, sets the pending
   flag and waits until the counter is 0, after which nothing may touch
   the handle or the fd: uv__async_stop closes the fd and sets it to -1,
   and the program may then free or reuse the handle's memory.

   In v1.53.0 the subtraction and the spin's load are relaxed. libuv
   commit 067c56e3 ("async: restore release/acquire on the busy counter",
   September 2026, #5307, after v1.53.0) makes them a release and an
   acquire, for ThreadSanitizer's report of a race between close and
   write (issue #5297). -DFIXED uses libuv/fixed/src/uv-common.c, the
   v1.53.0 file with that commit applied.

   t0 runs uv__async_stop on the loop, which spins on the handle and
   closes the fd, and then reuses the handle's memory, storing a pointer
   to another loop (whose fd is -2) in handle->loop. t1 calls
   uv_async_send. The stub write records the fd it was given in
   uv_test_fd. The condition is that t1 wrote to an fd other than 3: the
   closed fd's -1, or the reused memory's -2. That needs t1's loads to be
   satisfied after its subtraction is visible to t0 (load buffering),
   which ARM allows of relaxed accesses. In the kernel a write to a
   closed fd fails, or goes to whatever file was opened in its place. */
#ifdef FIXED
#include "libuv/fixed/src/uv-common.c"
#else
#include "libuv/src/uv-common.c"
#endif
#include "libuv/src/unix/async.c"
#include "externs.h"

int uv_test_woken;
int uv_test_fd;
int uv_test_closed;
int errno;

void uv__io_stop(uv_loop_t* loop, uv__io_t* w, unsigned int events) {
	(void) loop; (void) w; (void) events;
}

int uv__close(int fd) {
	__atomic_store_n(&uv_test_closed, fd, __ATOMIC_RELAXED);
	return 0;
}

extern uv_async_t async;

uv_loop_t loop = {
	.async_handles = { &async.queue, &async.queue },
	.async_io_watcher = { .fd = 3 },
	.async_wfd = -1,
};
uv_loop_t other_loop = {
	.async_io_watcher = { .fd = -2 },
	.async_wfd = -1,
};
uv_async_t async = {
	.loop = &loop,
	.type = UV_ASYNC,
	.queue = { &loop.async_handles, &loop.async_handles },
	.pending = 0,
};

void t0(void) {
	uv__async_stop(&loop);
	__atomic_store_n(&async.loop, &other_loop, __ATOMIC_RELAXED);
}

void t1(void) {
	uv_async_send(&async);
}
