// Stand-in for Boost.Predef: clang on aarch64 Linux. As in Boost.Predef,
// BOOST_COMP_GNUC is 0 for clang, which only emulates GCC.
#ifndef STUB_BOOST_PREDEF
#define STUB_BOOST_PREDEF
#define BOOST_VERSION_NUMBER(major, minor, patch) \
	((((major) % 100) * 10000000) + (((minor) % 100) * 100000) + ((patch) % 100000))
#define BOOST_COMP_CLANG BOOST_VERSION_NUMBER(__clang_major__, __clang_minor__, __clang_patchlevel__)
#define BOOST_COMP_GNUC 0
#define BOOST_ARCH_ARM BOOST_VERSION_NUMBER(8, 0, 0)
#define BOOST_ARCH_X86_64 0
#define BOOST_ARCH_SYS390 0
#define BOOST_ARCH_PPC 0
#define BOOST_OS_MACOS 0
#define BOOST_OS_IOS 0
#define BOOST_PLAT_ANDROID 0
#endif
