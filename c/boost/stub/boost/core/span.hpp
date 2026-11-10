// Stand-in for <boost/core/span.hpp>.
#ifndef STUB_BOOST_SPAN
#define STUB_BOOST_SPAN
#include <cstddef>
namespace boost {
template<class T, std::size_t E = std::size_t(-1)> class span {
	T* p;
	std::size_t n;
public:
	T* data() const noexcept { return p; }
	std::size_t size() const noexcept { return n; }
};
}
#endif
