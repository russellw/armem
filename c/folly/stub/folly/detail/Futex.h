// Stand-in for folly/detail/Futex.h. A wake records folly_test_woken
// (the system call is not modeled). Waiting is in MemoryIdler.h.
#pragma once
#include <atomic>
#include <cstdint>
extern "C" unsigned long folly_test_woken, folly_test_blocked;
extern "C" void* folly_test_exit;
namespace folly::detail {
template<template<typename> class Atom = std::atomic> using Futex = Atom<std::uint32_t>;
enum class FutexResult { VALUE_CHANGED, AWOKEN, INTERRUPTED, TIMEDOUT };
template<typename F> int futexWake(const F*, int = 0x7fffffff, std::uint32_t = ~0u) {
	__atomic_store_n(&folly_test_woken, 1, __ATOMIC_RELAXED);
	return 0;
}
}
