/* Stand-in for liburing's src/arch/aarch64/syscall.h, whose system calls
   are SVC instructions. Here each is a call to uring_test_syscall, which
   the test defines. src/arch/syscall-defs.h (unchanged) builds liburing's
   wrappers from these. */
#ifndef STUB_ARCH_AARCH64_SYSCALL_H
#define STUB_ARCH_AARCH64_SYSCALL_H

long uring_test_syscall(long n, long a, long b, long c, long d, long e,
			long f);

#define __do_syscall0(n) uring_test_syscall(n, 0, 0, 0, 0, 0, 0)
#define __do_syscall1(n, a) uring_test_syscall(n, (long) (a), 0, 0, 0, 0, 0)
#define __do_syscall2(n, a, b)						\
	uring_test_syscall(n, (long) (a), (long) (b), 0, 0, 0, 0)
#define __do_syscall3(n, a, b, c)					\
	uring_test_syscall(n, (long) (a), (long) (b), (long) (c), 0, 0, 0)
#define __do_syscall4(n, a, b, c, d)					\
	uring_test_syscall(n, (long) (a), (long) (b), (long) (c),	\
			   (long) (d), 0, 0)
#define __do_syscall5(n, a, b, c, d, e)					\
	uring_test_syscall(n, (long) (a), (long) (b), (long) (c),	\
			   (long) (d), (long) (e), 0)
#define __do_syscall6(n, a, b, c, d, e, f)				\
	uring_test_syscall(n, (long) (a), (long) (b), (long) (c),	\
			   (long) (d), (long) (e), (long) (f))

#include "../../../src/arch/syscall-defs.h"

#endif
