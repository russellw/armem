/* Stand-in for the C library's unistd.h. */
#ifndef STUB_UNISTD_H
#define STUB_UNISTD_H
#include <sys/types.h>
#define _SC_PAGESIZE 30
long sysconf(int name);
#endif
