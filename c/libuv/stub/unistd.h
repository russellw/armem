/* Stand-in for the C library's unistd.h. read finds no data (the
   notification has been consumed), and write records the notification
   (the eventfd write) in uv_test_woken. */
#ifndef STUB_UNISTD_H
#define STUB_UNISTD_H
#include <sys/types.h>
#include <errno.h>
extern int uv_test_woken;
static inline ssize_t read(int fd, void* buf, size_t n) {
  (void) fd; (void) buf; (void) n;
  errno = EAGAIN;
  return -1;
}
static inline ssize_t write(int fd, const void* buf, size_t n) {
  (void) fd; (void) buf;
  __atomic_store_n(&uv_test_woken, 1, __ATOMIC_RELAXED);
  return n;
}
int close(int fd);
#endif
