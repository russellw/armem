/* Stand-in for the C library's sys/wait.h. */
#ifndef STUB_SYS_WAIT_H
#define STUB_SYS_WAIT_H
#include <sys/types.h>
#include <signal.h>
typedef enum { P_ALL, P_PID, P_PGID, P_PIDFD } idtype_t;
#endif
