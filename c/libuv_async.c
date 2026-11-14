/* cflags: -ffreestanding -nostdlibinc -Ilibuv/stub -Ilibuv/include -Ilibuv/src */
/* libuv's async handle (libuv/src/uv-common.c and libuv/src/unix/async.c,
   unchanged from v1.53.0): another thread wakes the event loop with
   uv_async_send, and libuv's documentation promises that the callback
   then sees everything the thread did before the call, even when libuv
   coalesces the call with an earlier one that has not yet been handled.

   The handle is pending, as an earlier uv_async_send leaves it before
   the loop has handled it. t0 runs the loop's uv__async_io, which
   clears the pending flag and runs the callback, cb, which reads data.
   t1 stores to data and calls uv_async_send. The stub write (libuv/stub)
   records the notification in uv_test_woken. If t1's call was coalesced
   (sent no notification), the callback t0 runs is the one that answers
   it, so it must see t1's store.

   -DOLD_SEND uses libuv/old/src/uv-common.c, the current file with
   libuv commit cdc632c6 ("async: make seq_cst the default", June 2026)
   undone. Its uv_async_send first makes a relaxed load of the pending
   flag, and returns if it is set. -DDATA=__ATOMIC_RELEASE etc. give t1's
   store to data an ordering, and -DSEE=__ATOMIC_ACQUIRE etc. the
   callback's load of it (both relaxed by default). */
#ifdef OLD_SEND
#include "libuv/old/src/uv-common.c"
#else
#include "libuv/src/uv-common.c"
#endif
#include "libuv/src/unix/async.c"
#include "externs.h"

#ifndef DATA
#define DATA __ATOMIC_RELAXED
#endif
#ifndef SEE
#define SEE __ATOMIC_RELAXED
#endif

int data;
int seen;
int uv_test_woken;
int uv_test_fd;
int errno;

static void cb(uv_async_t* handle) {
	(void) handle;
	seen = __atomic_load_n(&data, SEE) + 1;
}

extern uv_async_t async;

uv_loop_t loop = {
	.async_handles = { &async.queue, &async.queue },
	.async_io_watcher = { .fd = 3 },
	.async_wfd = -1,
};
uv_async_t async = {
	.loop = &loop,
	.type = UV_ASYNC,
	.async_cb = cb,
	.queue = { &loop.async_handles, &loop.async_handles },
	.pending = 1,
};

void t0(void) {
	uv__async_io(&loop, &loop.async_io_watcher, POLLIN);
}

void t1(void) {
	__atomic_store_n(&data, 1, DATA);
	uv_async_send(&async);
}
