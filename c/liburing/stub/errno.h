/* Stand-in for the C library's errno.h: the values liburing uses. */
#ifndef STUB_ERRNO_H
#define STUB_ERRNO_H
extern int errno;
#define EAGAIN 11
#define EINVAL 22
#define ETIME 62
#define EOPNOTSUPP 95
#endif
