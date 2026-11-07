/* Stand-in for the C library's string.h. */
#ifndef STUB_STRING_H
#define STUB_STRING_H
#include <stddef.h>
void *memset(void *s, int c, size_t n);
void *memcpy(void *dest, const void *src, size_t n);
#endif
