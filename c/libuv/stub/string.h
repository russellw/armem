#ifndef STUB_STRING_H
#define STUB_STRING_H
#include <stddef.h>
void* memcpy(void*, const void*, size_t);
void* memset(void*, int, size_t);
size_t strlen(const char*);
char* strrchr(const char*, int);
int strcmp(const char*, const char*);
char* strerror(int);
char* strchr(const char*, int);
#endif
