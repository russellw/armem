/* Stand-in for the C library's stdlib.h. */
#ifndef STUB_STDLIB_H
#define STUB_STDLIB_H
#include <stddef.h>
void *malloc(size_t size);
void free(void *ptr);
#endif
