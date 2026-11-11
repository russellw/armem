// Stand-in for glog/logging.h: debug checks, compiled out with NDEBUG.
#pragma once
#define DCHECK_EQ(a, b) ((void) 0)
