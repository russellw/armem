/* Stand-in for the C library's sys/syscall.h: the asm-generic system call numbers aarch64 uses. */
#ifndef STUB_SYS_SYSCALL_H
#define STUB_SYS_SYSCALL_H
#define __NR_openat 56
#define __NR_close 57
#define __NR_read 63
#define __NR_munmap 215
#define __NR_mmap 222
#define __NR_madvise 233
#define __NR_prlimit64 261
#define __NR_io_uring_setup 425
#define __NR_io_uring_enter 426
#define __NR_io_uring_register 427
#endif
