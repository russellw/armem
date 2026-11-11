// Stand-in for folly/portability/Asm.h: folly's pause on aarch64.
#pragma once
namespace folly {
inline void asm_volatile_pause() { asm volatile("isb"); }
}
