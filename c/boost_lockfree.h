/* Shared by the boost_spsc_* programs: freestanding Boost.Lockfree
   (boost/include, unchanged from Boost.Lockfree commit d0172a2, October
   2026; Boost Software License, see boost/LICENSE), with stand-ins for
   the headers it includes (boost/stub). Each test constructs its object
   in its setup function, run before the threads start, so that the
   object needs no static constructor or destructor. */
#include <new>
