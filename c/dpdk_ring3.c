/* cflags: -ffreestanding -Idpdk/include -Idpdk/stub */
/* dpdk_ring.c with the enqueue of 2 in a third thread, t2, so that a
   consumer may receive the producer's item through the other consumer
   (the shape of DPDK commit a4ad0eba9d's bug). */
#define PRODUCER
#include "dpdk_ring.c"
