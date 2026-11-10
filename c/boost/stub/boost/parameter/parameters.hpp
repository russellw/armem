// Stand-in for <boost/parameter/parameters.hpp>: binding a signature's
// options keeps them as a list, which binding<> searches. It does not
// check them against the signature.
#ifndef STUB_BOOST_PARAMETER_PARAMETERS
#define STUB_BOOST_PARAMETER_PARAMETERS
namespace boost { namespace parameter {
template<class... Args> struct bound {};
template<class... Specs> struct parameters {
	template<class... Args> struct bind { typedef bound<Args...> type; };
};
}}
#endif
