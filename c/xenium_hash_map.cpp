/* cflags: -std=c++17 -ffreestanding -nostdlibinc -fno-exceptions -fno-rtti -DNDEBUG -Dthrow= -Dtry=if(true) -Dcatch(x)=if(false) -Ixenium/stub -Ixenium/include */
/* xenium's vyukov_hash_map (xenium/vyukov_hash_map.hpp and
   xenium/impl/vyukov_hash_map*.hpp, MIT, see xenium/LICENSE) from main
   1c449ae (July 2025, the squashed merge of PR #43, "Fix memory order
   issues in vyukov_hash_map"), with -DPRE from e427a4e (main just before
   it), and with -DNO_27E22F6 from main with 27e22f6 ("Fix more memory
   orders in vyukov_hash_map", one of #43's commits) reverted. The two
   older copies are in xenium/e427a4e and xenium/no-27e22f6, with their
   includes of the impl files changed to quoted ones so that they find
   their own. Stand-ins for the standard library are in xenium/stub.

   A bucket holds up to three items in arrays key[] and value[], and
   further items in a list of extension items (head), which come from
   shared free lists. Its state word holds a lock bit, the item count, a
   delete marker and a version. Writers (emplace, erase) lock the bucket
   with a compare-and-swap on state; erase bumps the version, and marks
   the slot it is overwriting while it moves an item into it (the last
   array item, or the first extension item, whose slot is then freed).
   try_get_value() takes no lock: it loads state (acquire), compares each
   key, loads the value (acquire), and then rereads state (relaxed); if
   the version changed it retries, and if the slot is marked it skips it.

   The keys are ints, hashed to themselves, and all go to bucket 0; the
   values are ints. Each scenario has t1 change the bucket while t0 looks
   a key up once. The condition is t0 getting a value that was never
   stored with its key, or, in the default scenario, not finding a key
   that is in the map throughout.

   Default: J=128 (10) and K=256 (20) fill slots 0 and 1. t1 erases J,
   which moves K into slot 0. t0 looks up K. In e427a4e, erase stored the
   moved key (relaxed) before the value (release), and try_get_value()
   loaded keys relaxed, so on ARM t0 can read K's key in slot 0 late
   (after its reread of state, which is ordered only after the value
   load), with slot 0's old value and the unmarked state: it gets 10 for
   K. 89393e0 (part of #43) makes the key load acquire and stores the key
   after the value, with release.

   -DREUSE: the same J and K. t1 erases K (the last slot, so no move),
   and emplaces L=384 (30), which goes into K's old slot 1. t0 looks up
   L. In e427a4e it can read L's key late, with K's value and the old
   state: it gets 20 for L. 27e22f6 also makes emplace's stores into the
   array release, but on ARM that isn't needed once the key load is
   acquire (89393e0): emplace stores after its acquire compare-and-swap
   on state, which comes after erase's release of the new version, so a
   t0 that sees L's key or value also sees that version and retries. In
   C++ the relaxed stores give no such edge.

   -DEXT: 128 buckets (the fewest that have an extension bucket, at one
   per 128), J1..J3 in the array and K=512 (20) in an extension item E.
   t1 erases K, which unlinks E and frees it, and emplaces L=640 (30):
   the array is full, so it takes E back from the free list (LIFO),
   stores L into it and links it. t0 looks up L. Before 27e22f6 the store
   of an extension item's key and value were relaxed, and so was
   try_get_value()'s load of its key, so a t0 that got to E through the
   old head can read L's key late, with K's value and the old state: it
   gets 20 for L. This one is ARM-only even with 89393e0, so 27e22f6's
   acquire load of an extension item's key (44) is needed on ARM.

   The reclaimer is a stand-in that never frees: guard_ptr::acquire() is
   a plain load with the given order and reclaim() does nothing; nothing
   reclaimable is freed in these runs (extension items go back to the
   map's own free lists). operator new hands out memory from a fixed
   pool. */
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

struct identity_hash {
	std::size_t operator()(int k) const noexcept { return static_cast<std::size_t>(k); }
};
}

#if defined(PRE)
#include "xenium/e427a4e/vyukov_hash_map.hpp"
#elif defined(NO_27E22F6)
#include "xenium/no-27e22f6/vyukov_hash_map.hpp"
#else
#include <xenium/vyukov_hash_map.hpp>
#endif

alignas(64) static char pool[16384];
static std::size_t pool_used;

void* operator new(std::size_t n) {
	n = (n + 63) & ~static_cast<std::size_t>(63);
	void* p = pool + pool_used;
	pool_used += n;
	return p;
}
void* operator new(std::size_t n, std::align_val_t, const std::nothrow_t&) noexcept {
	return operator new(n);
}
void operator delete(void*) noexcept {}
void operator delete(void*, std::size_t) noexcept {}
void operator delete(void*, std::align_val_t) noexcept {}
extern "C" void __cxa_pure_virtual() { __builtin_trap(); }
extern "C" void* memset(void* d, int c, std::size_t n) {
	char* p = static_cast<char*>(d);
	while (n--)
		*p++ = static_cast<char>(c);
	return d;
}

using map = xenium::vyukov_hash_map<int, int, xenium::policy::reclaimer<leak_reclaimer>,
	xenium::policy::hash<identity_hash>>;

alignas(map) static char map_memory[sizeof(map)];
static map* const m = reinterpret_cast<map*>(map_memory);

#ifdef EXT
#define CAPACITY 128
#else
#define CAPACITY 1
#endif

enum { J = 128, K = 256, L = 384 };

extern "C" {
int found0, value0, bad0;

void setup(void) {
	new (map_memory) map(CAPACITY);
#ifdef EXT
	m->emplace(128, 1);
	m->emplace(256, 2);
	m->emplace(384, 3);
	m->emplace(512, 20);
#else
	m->emplace(J, 10);
	m->emplace(K, 20);
#endif
}

void t0(void) {
	map::accessor acc;
#if defined(EXT)
	found0 = m->try_get_value(640, acc);
#elif defined(REUSE)
	found0 = m->try_get_value(L, acc);
#else
	found0 = m->try_get_value(K, acc);
#endif
	if (found0)
		value0 = *acc;
#if defined(EXT) || defined(REUSE)
	bad0 = found0 && value0 != 30;
#else
	bad0 = !found0 || value0 != 20;
#endif
}

void t1(void) {
#if defined(EXT)
	m->erase(512);
	m->emplace(640, 30);
#elif defined(REUSE)
	m->erase(K);
	m->emplace(L, 30);
#else
	m->erase(J);
#endif
}
}
