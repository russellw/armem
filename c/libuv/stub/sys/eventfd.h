#ifndef STUB_SYS_EVENTFD_H
#define STUB_SYS_EVENTFD_H
#define EFD_CLOEXEC 02000000
#define EFD_NONBLOCK 04000
int eventfd(unsigned initval, int flags);
#endif
