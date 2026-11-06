#ifndef STUB_SYS_SOCKET_H
#define STUB_SYS_SOCKET_H
#include <sys/types.h>
typedef unsigned socklen_t;
typedef unsigned short sa_family_t;
struct sockaddr { sa_family_t sa_family; char sa_data[14]; };
struct sockaddr_storage { sa_family_t ss_family; char ss_data[126]; };
#define AF_UNSPEC 0
#define AF_UNIX 1
#define AF_INET 2
#define AF_INET6 10
#endif
#define SOL_SOCKET 1
#define SO_SNDBUF 7
#define SO_RCVBUF 8
struct msghdr;
int getsockopt(int, int, int, void*, socklen_t*);
int setsockopt(int, int, int, const void*, socklen_t);
