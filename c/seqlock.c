/* A sequence lock (seqlock): one writer updates a two-word record, and a
   reader copies it without taking a lock. The writer makes the sequence
   number odd while it writes. The reader retries until it sees the same
   even sequence number before and after its copy.

   This is the correct C11 seqlock with fences, from Hans Boehm, "Can
   seqlocks get along with programming language memory models?" (MSPC
   2012). Each macro below names one ordering it needs; defining it as
   __ATOMIC_RELAXED (e.g. -DW_FENCE=__ATOMIC_RELAXED) removes it. Each such
   variant can return a torn record on ARM but not on x86, where TSO keeps
   the accesses in order anyway. */

typedef __UINT64_TYPE__ u64;

#ifndef W_FENCE
#define W_FENCE __ATOMIC_RELEASE /* writer: odd seq before the data */
#endif
#ifndef W_SEQ
#define W_SEQ __ATOMIC_RELEASE /* writer: data before the even seq */
#endif
#ifndef R_SEQ
#define R_SEQ __ATOMIC_ACQUIRE /* reader: first seq before the data */
#endif
#ifndef R_FENCE
#define R_FENCE __ATOMIC_ACQUIRE /* reader: data before the second seq */
#endif

unsigned seq;
u64 a, b;         /* the record; the writer changes it from (0,0) to (1,1) */
u64 out_a, out_b; /* the reader's copy */

void writer(void) {
  unsigned s = __atomic_load_n(&seq, __ATOMIC_RELAXED);
  __atomic_store_n(&seq, s + 1, __ATOMIC_RELAXED);
  __atomic_thread_fence(W_FENCE);
  __atomic_store_n(&a, 1, __ATOMIC_RELAXED);
  __atomic_store_n(&b, 1, __ATOMIC_RELAXED);
  __atomic_store_n(&seq, s + 2, W_SEQ);
}

void reader(void) {
  unsigned s1, s2;
  u64 x, y;
  do {
    s1 = __atomic_load_n(&seq, R_SEQ);
    x = __atomic_load_n(&a, __ATOMIC_RELAXED);
    y = __atomic_load_n(&b, __ATOMIC_RELAXED);
    __atomic_thread_fence(R_FENCE);
    s2 = __atomic_load_n(&seq, __ATOMIC_RELAXED);
  } while ((s1 & 1) || s1 != s2);
  out_a = x;
  out_b = y;
}
