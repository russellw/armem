//
// Copyright (c) 2018-2020 Manuel Pöter.
// Licensed under the MIT License. See LICENSE file in the project root for full license information.
//

#ifndef XENIUM_UTILS_HPP
#define XENIUM_UTILS_HPP

#include <xenium/detail/port.hpp>

#include <cstdint>
#ifdef _M_AMD64
  #include <intrin.h>
#endif

namespace xenium::utils {
template <typename T>
constexpr bool is_power_of_two(T val) {
  return (val & (val - 1)) == 0;
}

template <typename T>
constexpr unsigned find_last_bit_set(T val) {
  unsigned result = 0;
  for (; val != 0; val >>= 1) {
    ++result;
  }
  return result;
}

template <typename T>
constexpr T next_power_of_two(T val) {
  if (is_power_of_two(val)) {
    return val;
  }

  return static_cast<T>(1) << find_last_bit_set(val);
}

template <typename T>
struct modulo {
  T operator()(T a, T b) { return a % b; }
};

// TODO - use intrinsics for rotate operation (if available)
template <uintptr_t C>
struct rotate {
  static uintptr_t left(uintptr_t v) {
    static_assert(C > 0, "should never happen!");
    return (v >> (64 - C)) | (v << C);
  }

  static uintptr_t right(uintptr_t v) {
    static_assert(C > 0, "should never happen!");
    return (v >> C) | (v << (64 - C));
  }
};

template <>
struct rotate<0> {
  static uintptr_t left(uintptr_t v) { return v; }
  static uintptr_t right(uintptr_t v) { return v; }
};

// armem stub: getticks() reads cntvct_el0, a system register armem doesn't
// model. The harness's segments hold one entry, so the random start index
// is always 0 whatever it returns.
static inline std::uint64_t getticks() {
  return 0;
}

inline std::uint64_t random() {
  return getticks() >> 4;
}
} // namespace xenium::utils
#endif
