// Stand-in for folly/IndexedMemPool.h: a fixed array of records, each
// constructed when first allocated and kept after it is recycled (as
// IndexedMemPoolTraitsLazyRecycle does). Index 0 is never allocated. The
// tests allocate their records in setup, so the pool's own
// synchronization is not modeled.
#pragma once
#include <atomic>
#include <cstdint>
#include <new>
namespace folly {
template<typename T> struct IndexedMemPoolTraitsLazyRecycle {};
template<typename T, uint32_t NumLocalLists, uint32_t LocalListLimit,
		template<typename> class Atom, typename Traits>
class IndexedMemPool {
	static constexpr uint32_t kSlots = 2;
	alignas(T) unsigned char slots[kSlots][sizeof(T)];
	uint32_t next = 0;
	uint32_t free[kSlots];
	uint32_t nfree = 0;
public:
	explicit IndexedMemPool(uint32_t) {}
	uint32_t allocIndex() {
		if (nfree)
			return free[--nfree];
		if (next == kSlots)
			return 0;
		new (slots[next]) T();
		return ++next;
	}
	void recycleIndex(uint32_t i) { free[nfree++] = i; }
	T& operator[](uint32_t i) { return *reinterpret_cast<T*>(slots[i - 1]); }
};
}
