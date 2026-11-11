// Stand-in for folly/concurrency/CacheLocality.h: folly/lang/Align.h's
// value on aarch64 when the standard library gives none.
#pragma once
#include <cstddef>
namespace folly {
constexpr std::size_t hardware_destructive_interference_size = 128;
}
