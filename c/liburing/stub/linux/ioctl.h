/* Stand-in for Linux's linux/ioctl.h (asm-generic). */
#ifndef STUB_LINUX_IOCTL_H
#define STUB_LINUX_IOCTL_H
#define _IOC(dir, type, nr, size) (((dir) << 30) | ((size) << 16) | ((type) << 8) | (nr))
#define _IO(type, nr) _IOC(0U, (type), (nr), 0U)
#endif
