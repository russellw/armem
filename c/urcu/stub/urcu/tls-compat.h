/* Stand-in for liburcu's urcu/tls-compat.h. The tests run one thread that
   uses each thread-local variable, so a variable can be an ordinary
   global, which other threads can reach by name as the grace-period
   code reaches it through the registry of readers. */
#ifndef _URCU_TLS_COMPAT_H
#define _URCU_TLS_COMPAT_H
#define DECLARE_URCU_TLS(type, name) type name
#define DEFINE_URCU_TLS(type, name) type name
#define DEFINE_URCU_TLS_INIT(type, name, init) type name = (init)
#define URCU_TLS(name) (name)
#endif
