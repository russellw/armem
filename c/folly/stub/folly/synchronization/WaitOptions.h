// Stand-in for folly/synchronization/WaitOptions.h: spinning is not
// modeled (see detail/Spin.h).
#pragma once
#include <chrono>
namespace folly {
struct WaitOptions {};
}
