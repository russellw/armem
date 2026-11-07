/* Stand-in for Linux's linux/types.h (aarch64). */
#ifndef STUB_LINUX_TYPES_H
#define STUB_LINUX_TYPES_H
typedef unsigned char __u8;
typedef signed char __s8;
typedef unsigned short __u16;
typedef short __s16;
typedef unsigned int __u32;
typedef int __s32;
typedef unsigned long long __u64;
typedef long long __s64;
typedef __u16 __le16;
typedef __u32 __le32;
typedef __u64 __le64;
typedef __u16 __be16;
typedef __u32 __be32;
typedef __u64 __be64;
#define __aligned_u64 __u64 __attribute__((aligned(8)))
typedef int __kernel_rwf_t;
#endif
