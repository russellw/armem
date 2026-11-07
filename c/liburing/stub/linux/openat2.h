/* Stand-in for Linux's linux/openat2.h. */
#ifndef STUB_LINUX_OPENAT2_H
#define STUB_LINUX_OPENAT2_H
#include <linux/types.h>
struct open_how { __u64 flags; __u64 mode; __u64 resolve; };
#endif
