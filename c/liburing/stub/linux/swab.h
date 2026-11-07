/* Stand-in for Linux's linux/swab.h. */
#ifndef STUB_LINUX_SWAB_H
#define STUB_LINUX_SWAB_H
#include <linux/types.h>
static inline __u32 __swahw32(__u32 x) { return x << 16 | x >> 16; }
#endif
