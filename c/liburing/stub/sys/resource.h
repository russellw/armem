/* Stand-in for the C library's sys/resource.h. */
#ifndef STUB_SYS_RESOURCE_H
#define STUB_SYS_RESOURCE_H
struct rlimit { unsigned long rlim_cur, rlim_max; };
#endif
