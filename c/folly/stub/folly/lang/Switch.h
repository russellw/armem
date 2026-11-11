// Stand-in for folly/lang/Switch.h, and folly::assume_unreachable.
#pragma once
#define FOLLY_EXHAUSTIVE_SWITCH(...) __VA_ARGS__
namespace folly {
[[noreturn]] inline void assume_unreachable() { __builtin_unreachable(); }
}
