// Stand-in for folly/Portability.h, which also brings in, through
// folly's other headers, the standard headers FlatCombining.h uses
// without including them.
#pragma once
#include <folly/CPortability.h>
#include <optional>
#include <stdexcept>
#include <string>
