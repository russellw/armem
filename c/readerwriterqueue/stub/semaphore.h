/* Stand-in for semaphore.h: what atomicops.h's semaphore uses. */
#ifndef STUB_SEMAPHORE_H
#define STUB_SEMAPHORE_H
#include <ctime>
typedef struct { long data[4]; } sem_t;
extern "C" {
int sem_init(sem_t* s, int pshared, unsigned value);
int sem_destroy(sem_t* s);
int sem_wait(sem_t* s);
int sem_trywait(sem_t* s);
int sem_timedwait(sem_t* s, const struct timespec* t);
int sem_post(sem_t* s);
}
#endif
