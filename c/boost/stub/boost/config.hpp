// Stand-in for Boost.Config: C++17 on clang, without <optional>.
#ifndef STUB_BOOST_CONFIG
#define STUB_BOOST_CONFIG
#define BOOST_HAS_PRAGMA_ONCE
#define BOOST_NO_CXX20_HDR_CONCEPTS
#define BOOST_NO_CXX17_HDR_OPTIONAL
#define BOOST_LIKELY(x) __builtin_expect(!!(x), 1)
#define BOOST_UNLIKELY(x) __builtin_expect(!!(x), 0)
#endif
