/* Stand-in for QEMU's qemu/thread.h: the QemuEvent type and functions,
   as declared there (with HAVE_FUTEX, as on Linux). */
#ifndef QEMU_THREAD_H
#define QEMU_THREAD_H
#include "qemu/atomic.h"
#include "qemu/futex.h"

typedef struct QemuEvent {
#ifndef HAVE_FUTEX
    pthread_mutex_t lock;
    pthread_cond_t cond;
#endif
    unsigned value;
    bool initialized;
} QemuEvent;

void qemu_event_init(QemuEvent *ev, bool init);
void qemu_event_set(QemuEvent *ev);
void qemu_event_reset(QemuEvent *ev);
void qemu_event_wait(QemuEvent *ev);
void qemu_event_destroy(QemuEvent *ev);
#endif
