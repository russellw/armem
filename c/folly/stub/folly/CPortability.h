// Stand-in for folly/CPortability.h: the attributes the headers use.
#pragma once
#define FOLLY_ALWAYS_INLINE inline __attribute__((__always_inline__))
#define FOLLY_NOINLINE __attribute__((__noinline__))
