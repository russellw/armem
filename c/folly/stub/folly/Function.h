// Stand-in for folly/Function.h: a move-only function stored inline.
#pragma once
#include <new>
#include <utility>
namespace folly {
template<typename> class Function;
template<typename R, typename... A> class Function<R(A...)> {
	alignas(16) unsigned char buf[48];
	R (*call)(void*, A...) = nullptr;
public:
	Function() noexcept = default;
	template<typename F> Function(F f) noexcept {
		static_assert(sizeof(F) <= sizeof(buf), "stored inline");
		new (buf) F(std::move(f));
		call = [](void* p, A... a) -> R { return (*static_cast<F*>(p))(static_cast<A&&>(a)...); };
	}
	Function(Function&&) noexcept = default;
	Function& operator=(Function&&) noexcept = default;
	explicit operator bool() const noexcept { return call != nullptr; }
	R operator()(A... a) { return call(buf, static_cast<A&&>(a)...); }
};
}
