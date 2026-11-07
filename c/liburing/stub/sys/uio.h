/* Stand-in for the C library's sys/uio.h. */
#ifndef STUB_SYS_UIO_H
#define STUB_SYS_UIO_H
#include <stddef.h>
struct iovec { void *iov_base; size_t iov_len; };
#endif
