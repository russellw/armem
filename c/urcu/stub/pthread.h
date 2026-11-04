/* Stand-in for the C library's pthread.h: just the types liburcu's
   headers name. */
#ifndef _PTHREAD_H
#define _PTHREAD_H
typedef unsigned long pthread_t;
typedef struct { int lock; } pthread_mutex_t;
#endif
