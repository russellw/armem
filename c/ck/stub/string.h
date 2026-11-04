/* The parts of <string.h> that the ck headers use. There is no AArch64
   libc here, so a program that calls them defines them. */
#include <stddef.h>
void *memcpy(void *, const void *, size_t);
void *memset(void *, int, size_t);
