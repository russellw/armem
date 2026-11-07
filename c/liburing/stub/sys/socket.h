/* Stand-in for the C library's sys/socket.h (glibc, aarch64): the types and macros liburing uses. */
#ifndef STUB_SYS_SOCKET_H
#define STUB_SYS_SOCKET_H
#include <sys/types.h>
#include <sys/uio.h>
typedef unsigned int socklen_t;
struct sockaddr { unsigned short sa_family; char sa_data[14]; };
struct msghdr {
	void *msg_name;
	socklen_t msg_namelen;
	struct iovec *msg_iov;
	size_t msg_iovlen;
	void *msg_control;
	size_t msg_controllen;
	int msg_flags;
};
struct cmsghdr {
	size_t cmsg_len;
	int cmsg_level;
	int cmsg_type;
	unsigned char __cmsg_data[];
};
#define CMSG_ALIGN(len) (((len) + sizeof(size_t) - 1) & (size_t) ~(sizeof(size_t) - 1))
#endif
