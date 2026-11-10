// Stand-in for <boost/parameter/binding.hpp>: the value of the first
// option with the tag, or the default.
#ifndef STUB_BOOST_PARAMETER_BINDING
#define STUB_BOOST_PARAMETER_BINDING
#include <boost/parameter/parameters.hpp>
#include <type_traits>
namespace boost { namespace parameter {
template<class T> struct identity { typedef T type; };
template<class Bound, class Tag, class Default> struct binding;
template<class Tag, class Default> struct binding<bound<>, Tag, Default> { typedef Default type; };
template<class Tag, class Default, class A, class... Rest> struct binding<bound<A, Rest...>, Tag, Default> {
	typedef typename std::conditional_t<std::is_same<typename A::key_type, Tag>::value,
		identity<typename A::value_type>, binding<bound<Rest...>, Tag, Default>>::type type;
};
}}
#endif
