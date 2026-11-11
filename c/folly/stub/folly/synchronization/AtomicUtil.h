// Stand-in for folly/synchronization/AtomicUtil.h.
#pragma once
#include <atomic>
#include <cassert>
namespace folly {
template<typename T> struct type_t_ { using type = T; };
template<template<typename> class Atom, typename T>
bool atomic_compare_exchange_weak_explicit(Atom<T>* obj, typename type_t_<T>::type* expected,
		typename type_t_<T>::type desired, std::memory_order succ, std::memory_order fail) {
	return obj->compare_exchange_weak(*expected, desired, succ, fail);
}
}
