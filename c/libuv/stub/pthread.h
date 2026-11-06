#ifndef STUB_PTHREAD_H
#define STUB_PTHREAD_H
typedef unsigned long pthread_t;
typedef struct { long l[6]; } pthread_mutex_t;
typedef struct { long l[7]; } pthread_rwlock_t;
typedef struct { long l[6]; } pthread_cond_t;
typedef int pthread_once_t;
typedef unsigned pthread_key_t;
#define PTHREAD_ONCE_INIT 0
#endif
