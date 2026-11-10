// Stand-in for Boost.Parameter's template keywords: an option names its
// tag (key_type) and its value (value_type).
#ifndef STUB_BOOST_PARAMETER_TEMPLATE_KEYWORD
#define STUB_BOOST_PARAMETER_TEMPLATE_KEYWORD
namespace boost { namespace parameter {
template<class Tag, class T> struct template_keyword {
	typedef Tag key_type;
	typedef T value_type;
};
}}
#endif
