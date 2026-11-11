// Stand-in for folly/synchronization/detail/Spin.h: spinning tests the
// condition once, then advances to blocking. More tests would only poll
// again.
#pragma once
namespace folly::detail {
enum class spin_result { success, timeout, advance };
template<typename Deadline, typename Options, typename F>
spin_result spin_pause_until(const Deadline&, const Options&, F f) {
	return f() ? spin_result::success : spin_result::advance;
}
template<typename Deadline, typename F>
spin_result spin_yield_until(const Deadline&, F f) {
	return f() ? spin_result::success : spin_result::advance;
}
}
