/* Stand-in for liburcu's generated urcu/config.h, as configure sets it
   for aarch64 Linux with the default options. (Not CONFIG_RCU_TLS: the
   tests' stub urcu/tls-compat.h replaces thread-local storage.) */
#ifndef _URCU_CONFIG_H
#define _URCU_CONFIG_H
#define CONFIG_RCU_SMP 1
#define CONFIG_RCU_HAVE_MULTIFLAVOR 1
#define CONFIG_RCU_HAVE_CLOCK_GETTIME 1
#endif
