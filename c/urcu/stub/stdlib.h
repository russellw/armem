/* Stand-in for the C library's stdlib.h: what liburcu's headers use. */
#ifndef _STDLIB_H
#define _STDLIB_H
#include <stddef.h>
void abort(void) __attribute__((noreturn));
void free(void *p);
void *malloc(size_t n);
#endif
