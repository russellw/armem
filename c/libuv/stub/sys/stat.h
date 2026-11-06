#ifndef STUB_SYS_STAT_H
#define STUB_SYS_STAT_H
#include <sys/types.h>
struct stat { unsigned long st_dev, st_ino; mode_t st_mode; long st_size; };
int stat(const char*, struct stat*);
int lstat(const char*, struct stat*);
int fstat(int, struct stat*);
#endif
