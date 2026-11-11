// Stand-in for folly/system/ThreadName.h.
#pragma once
#include <string>
namespace folly {
inline bool setThreadName(const std::string&) { return true; }
}
