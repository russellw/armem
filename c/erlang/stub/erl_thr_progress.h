/* Stand-in for erts' erl_thr_progress.h: every thread is managed. */
#ifndef ERL_THR_PROGRESS_H__
#define ERL_THR_PROGRESS_H__

typedef int ErtsThrPrgrDelayHandle;
#define ERTS_THR_PRGR_DHANDLE_MANAGED ((ErtsThrPrgrDelayHandle) -1)
static inline ErtsThrPrgrDelayHandle erts_thr_progress_unmanaged_delay(void)
{ return ERTS_THR_PRGR_DHANDLE_MANAGED; }
static inline void erts_thr_progress_unmanaged_continue(ErtsThrPrgrDelayHandle h)
{ (void) h; }

#endif
