/* What DPDK's build configures for aarch64 (meson sets
   RTE_USE_C11_MEM_MODEL on Arm). Define RTE_GENERIC_RING to use the
   ring's non-C11 code instead (rte_ring_gcc_pvt.h). */
#ifndef _RTE_CONFIG_H_
#define _RTE_CONFIG_H_
#define RTE_CACHE_LINE_SIZE 64
#define RTE_ARCH_ARM64 1
#ifndef RTE_GENERIC_RING
#define RTE_USE_C11_MEM_MODEL 1
#endif
#endif
