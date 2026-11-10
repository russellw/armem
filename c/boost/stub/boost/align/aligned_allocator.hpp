// Stand-in for <boost/align/aligned_allocator.hpp>. The tests size their
// queues at compile time, so nothing is allocated.
#ifndef STUB_BOOST_ALIGNED_ALLOCATOR
#define STUB_BOOST_ALIGNED_ALLOCATOR
#include <cstddef>
namespace boost { namespace alignment {
template<class T, std::size_t Alignment> struct aligned_allocator {
	typedef T value_type;
	template<class U> struct rebind { typedef aligned_allocator<U, Alignment> other; };
	T* allocate(std::size_t n);
	void deallocate(T* p, std::size_t n);
};
}}
#endif
