/* Stand-in for the C library's unistd.h. read finds no data (the
   notification has been consumed), and write records the notification
   (the eventfd write) in uv_test_woken, and the fd it wrote to in
   uv_test_fd. */
#ifndef STUB_UNISTD_H
#define STUB_UNISTD_H
#include <sys/types.h>
#include <errno.h>
extern int uv_test_woken;
extern int uv_test_fd;
static inline ssize_t read(int fd, void* buf, size_t n) {
  (void) fd; (void) buf; (void) n;
  errno = EAGAIN;
  return -1;
}
static inline ssize_t write(int fd, const void* buf, size_t n) {
  (void) buf;
  __atomic_store_n(&uv_test_fd, fd, __ATOMIC_RELAXED);
  __atomic_store_n(&uv_test_woken, 1, __ATOMIC_RELAXED);
  return n;
}
int close(int fd);
#endif
