// Stand-in for folly/Likely.h.
#pragma once
#include <folly/CPortability.h>
#define FOLLY_LIKELY(x) __builtin_expect(!!(x), 1)
#define FOLLY_UNLIKELY(x) __builtin_expect(!!(x), 0)
