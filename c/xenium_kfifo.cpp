/* cflags: -std=c++17 -ffreestanding -nostdlibinc -fno-exceptions -fno-rtti -DNDEBUG -Dthrow= -Ixenium/stub -Ixenium/include */
/* xenium's kirsch_kfifo_queue (xenium/kirsch_kfifo_queue.hpp, MIT, see
   xenium/LICENSE) from main 1c449ae (July 2025, the latest release
   branch) and, with -DFIX, from the unmerged branch fix/kirsch-kfifo-queue
   (048e278, October 2026, "Fix potential race in kirsch kfifo queue").
   Stand-ins for the standard library and for utils::getticks() (which
   reads cntvct_el0) are in xenium/stub.

   The queue is a linked list of segments of k slots, with head_ and tail_
   pointing to segments. push() takes tail_ (acquire), scans its segment
   for an empty slot with relaxed loads (find_index), and fills one with a
   release compare-and-swap, or, if the segment is full, advances tail_
   (advance_tail(), which links a new segment if there is none) and
   retries. pop() (do_pop()) takes head_ (acquire), scans its segment for
   a full slot with relaxed loads, checks head_ again (relaxed), and loads
   tail_ (acquire). If it found an item it empties the slot with an
   acquire compare-and-swap. If it found none, the queue is empty when
   head_ and tail_ are the same segment and tail_ hasn't changed since (a
   relaxed load); otherwise advance_head() moves head_ to the next segment
   and pop() retries. advance_head() loads head->next (acquire) and, if
   head and tail_ are the same segment, loads tail->next (acquire) again
   and returns if it is null, but it never checks the first load.

   Segments have one slot (k = 1), so the scan has one slot and the
   random start index doesn't matter. In setup, A (1) is pushed, so the
   queue is one full segment S. t1 pushes B (2): S is full, so it links a
   new segment S2, moves tail_ to it and puts B there. Then t1 pops,
   which takes A from S. t0 pops once. The queue holds A until after B is
   in, so it is never empty, and the condition is t0's pop finding it
   empty.

   On ARM it can, as 048e278 says: the slot load is relaxed, and an
   acquire load orders only what comes after it, so t0's scan can see
   the slot t1 emptied while its loads of tail_ see S, from before t1
   moved tail_. The same reordering can also crash the queue. If the
   relaxed reread of tail_ sees S2 while the acquire load before it saw
   S, the empty check fails and t0 calls advance_head(), whose load of
   S->next (acquire, but after the relaxed load only in program order)
   can still see null while its second load of it sees S2. advance_head()
   then moves head_ to null, and t0's next pass faults loading null->k.
   Neither can happen under SC or TSO: seeing the slot empty means t1
   had linked S2.

   048e278 adds an acquire fence after a scan that finds nothing, and
   makes the compare-and-swaps that empty a slot release (acq_rel in
   pop()), so that a slot found empty brings the S->next and tail_ that
   were current when it was emptied. It closes both. -DFENCE_ONLY is main
   with just the fence: it holds on ARM too, because there t1's pop
   (LDAXR) can't come before its release compare-and-swaps in push()
   (STLXR). C++ doesn't promise that: an acquire compare-and-swap doesn't
   order the stores before it. -DTAIL_ACQUIRE
   is main with the reread of tail_ acquire, which closes the crash
   (S->next is read after it) but not the false empty.

   To keep the search small, t0's pop is do_pop() copied here, stopping
   where it finds an item (the found branch, which neither bug needs, and
   which with k = 1 links a segment of its own), with the fence, and the
   reread's order, as in the variant being run. t1's push() starts with
   its first pass copied here (finding S full and calling advance_tail()),
   with S2 allocated in setup, since exploring the stores that construct
   it is too slow (t0 reaches S2 only through the release
   compare-and-swaps that publish them). It then calls push(), whose next
   pass puts B in S2, and pop().

   The reclaimer is a stand-in that never frees: guard_ptr::acquire() is
   a plain load with the given order and reclaim() does nothing.
   Reclamation is a separate protocol (and xenium's reclaimers keep
   thread-local state, which armem doesn't model), and nothing is freed
   in these runs. operator new hands out memory from a fixed pool. */
#include <new>
#include <atomic>
#include <xenium/reclamation/detail/concurrent_ptr.hpp>
#include <xenium/reclamation/detail/guard_ptr.hpp>

namespace {
struct leak_reclaimer {
	template<class T, std::size_t N = 0, class Deleter = void>
	struct enable_concurrent_ptr {
		virtual ~enable_concurrent_ptr() = default;
	};
	template<class T, class MarkedPtr> class guard_ptr;
	template<class T, std::size_t N = 0>
	using concurrent_ptr = xenium::reclamation::detail::concurrent_ptr<T, N, guard_ptr>;
};

template<class T, class MarkedPtr>
class leak_reclaimer::guard_ptr
	: public xenium::reclamation::detail::guard_ptr<T, MarkedPtr, guard_ptr<T, MarkedPtr>> {
	using base = xenium::reclamation::detail::guard_ptr<T, MarkedPtr, guard_ptr<T, MarkedPtr>>;
public:
	explicit guard_ptr(const MarkedPtr& p = MarkedPtr()) noexcept : base(p) {}
	template<class Ptr> void acquire(const Ptr& p, std::memory_order order) noexcept { this->ptr = p.load(order); }
	void reset() noexcept { this->ptr.reset(); }
	void reclaim() noexcept { reset(); }
};
}

// The excerpts below use the queue's private members.
#define private public
#ifdef FIX
#include "xenium/fix/xenium/kirsch_kfifo_queue.hpp"
#else
#include <xenium/kirsch_kfifo_queue.hpp>
#endif
#undef private

#if defined(FIX) || defined(FENCE_ONLY)
#define SCAN_FENCE 1
#endif
#ifdef TAIL_ACQUIRE
#define TAIL_REREAD std::memory_order_acquire
#else
#define TAIL_REREAD std::memory_order_relaxed
#endif

alignas(16) static char pool[4096];
static std::size_t pool_used;

void* operator new(std::size_t n) {
	n = (n + 15) & ~static_cast<std::size_t>(15);
	void* p = pool + pool_used;
	pool_used += n;
	return p;
}
void operator delete(void*) noexcept {}
void operator delete(void*, std::size_t) noexcept {}
extern "C" void __cxa_pure_virtual() { __builtin_trap(); }

using queue = xenium::kirsch_kfifo_queue<int*, xenium::policy::reclaimer<leak_reclaimer>>;
using marked_value = queue::marked_value;

alignas(queue) static char queue_memory[sizeof(queue)];
static queue* const q = reinterpret_cast<queue*>(queue_memory);
static queue::segment* s2;
static int a = 1, b = 2;

extern "C" {
int found0, empty0, got1;

void setup(void) {
	new (queue_memory) queue(1);
	q->push(&a);
	s2 = q->alloc_segment();
}

// do_pop() up to finding an item.
void t0(void) {
	queue::guard_ptr head_old;
	for (;;) {
		head_old.acquire(q->head_, std::memory_order_acquire);
		uint64_t idx = 0;
		marked_value old_value;
		bool found_idx = q->find_index<false>(head_old, idx, old_value);
		if (head_old != q->head_.load(std::memory_order_relaxed))
			continue;
#ifdef SCAN_FENCE
		if (!found_idx)
			std::atomic_thread_fence(std::memory_order_acquire);
#endif
		queue::marked_ptr tail_old = q->tail_.load(std::memory_order_acquire);
		if (found_idx) {
			found0 = 1;
			return;
		}
		if (head_old.get() == tail_old.get() && tail_old == q->tail_.load(TAIL_REREAD)) {
			empty0 = 1;
			return;
		}
		q->advance_head(head_old, tail_old);
	}
}

// push()'s first pass, with advance_tail() linking the S2 from setup.
static void push_first_pass(void) {
	queue::guard_ptr tail_old;
	tail_old.acquire(q->tail_, std::memory_order_acquire);
	uint64_t idx = 0;
	marked_value old_value;
	bool found_idx = q->find_index<true>(tail_old, idx, old_value);
	if (tail_old != q->tail_.load(std::memory_order_relaxed) || found_idx)
		return;

	// advance_tail(tail_old)
	queue::marked_ptr tail_current = tail_old;
	queue::marked_ptr next_segment = tail_current->next.load(std::memory_order_acquire);
	if (tail_current != q->tail_.load(std::memory_order_relaxed))
		return;
	if (next_segment.get() != nullptr) {
		queue::marked_ptr new_tail(next_segment.get(), next_segment.mark() + 1);
		q->tail_.compare_exchange_strong(tail_current, new_tail, std::memory_order_release, std::memory_order_relaxed);
	} else {
		const queue::marked_ptr new_segment(s2, next_segment.mark() + 1);
		if (tail_current->next.compare_exchange_strong(
				next_segment, new_segment, std::memory_order_release, std::memory_order_relaxed)) {
			queue::marked_ptr new_tail(s2, tail_current.mark() + 1);
			q->tail_.compare_exchange_strong(tail_current, new_tail, std::memory_order_release, std::memory_order_relaxed);
		}
	}
}

void t1(void) {
	push_first_pass();
	q->push(&b);
	int* v = nullptr;
	if (q->try_pop(v))
		got1 = *v;
}
}
