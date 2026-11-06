#ifndef STUB_STDIO_H
#define STUB_STDIO_H
#include <stddef.h>
#include <stdarg.h>
typedef struct FILE FILE;
extern FILE* stderr;
int fprintf(FILE*, const char*, ...);
int snprintf(char*, size_t, const char*, ...);
int vsnprintf(char*, size_t, const char*, va_list);
#endif
