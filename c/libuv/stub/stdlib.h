/* Stand-in for the C library's stdlib.h: what libuv's files use. */
#ifndef STUB_STDLIB_H
#define STUB_STDLIB_H
#include <stddef.h>
void abort(void) __attribute__((noreturn));
void free(void* p);
void* malloc(size_t n);
void* calloc(size_t n, size_t size);
void* realloc(void* p, size_t n);
char* getenv(const char* name);
#endif
