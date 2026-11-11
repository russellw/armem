// Stand-in for folly/detail/MemoryIdler.h: a futex wait. If the futex
// still holds the expected value, the thread would sleep: the wait
// records folly_test_blocked and ends the thread by branching to
// folly_test_exit, the address its thread function returns to.
// Otherwise it fails at once, as the system call does.
#pragma once
#include <folly/detail/Futex.h>
namespace folly::detail {
struct MemoryIdler {
	template<typename F, typename Deadline>
	static FutexResult futexWaitUntil(F& fut, std::uint32_t expected, const Deadline&) {
		if (fut.load(std::memory_order_relaxed) == expected) {
			__atomic_store_n(&folly_test_blocked, 1, __ATOMIC_RELAXED);
			asm volatile("br %0" :: "r"(folly_test_exit));
			__builtin_unreachable();
		}
		return FutexResult::VALUE_CHANGED;
	}
};
}
