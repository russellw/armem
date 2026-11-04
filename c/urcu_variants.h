/* Old versions of liburcu's flavor headers, for the urcu_*.c tests.

   OLD_QSBR: urcu/static/urcu-qsbr.h with liburcu commit 9e6448d
   ("urcu-qsbr: Use CMM_SEQ_CST_FENCE for quiescent state update and
   offline", January 2026) reverted. OLD_MB: urcu/static/urcu-mb.h with
   commit 96ef712 ("urcu-mb: Use CMM_SEQ_CST_FENCE for
   _urcu_mb_read_unlock_update_and_wakeup", January 2026) reverted. Each
   is in urcu/old, the current file with just that commit's change undone.
   Included first, it defines the include guard of the current file,
   which the test's include then skips. */

#ifdef OLD_QSBR
#include "urcu/old/urcu/static/urcu-qsbr.h"
#endif
#ifdef OLD_MB
#include "urcu/old/urcu/static/urcu-mb.h"
#endif
