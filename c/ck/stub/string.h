/* The only part of <string.h> that ck_ring.h uses. There is no AArch64
   libc here, so each program that needs memcpy defines it. */
#include <stddef.h>
void *memcpy(void *, const void *, size_t);
