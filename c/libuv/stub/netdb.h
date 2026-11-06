#ifndef STUB_NETDB_H
#define STUB_NETDB_H
struct addrinfo { int ai_flags, ai_family, ai_socktype, ai_protocol; unsigned ai_addrlen; struct sockaddr* ai_addr; char* ai_canonname; struct addrinfo* ai_next; };
void freeaddrinfo(struct addrinfo*);
#endif
