// Stand-in for <boost/core/allocator_access.hpp>: allocator_rebind.
#ifndef STUB_BOOST_ALLOCATOR_ACCESS
#define STUB_BOOST_ALLOCATOR_ACCESS
namespace boost {
template<class A, class T> struct allocator_rebind { typedef typename A::template rebind<T>::other type; };
}
#endif
