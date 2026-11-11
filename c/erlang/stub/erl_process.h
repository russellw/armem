/*
 * Stand-in for erts' erl_process.h: a process is just its id and lock.
 * The process table, which the tests do not use, is empty, and its
 * reference counts do nothing.
 */
#ifndef ERL_PROCESS_H__
#define ERL_PROCESS_H__

#include "erl_threads.h"

typedef struct process Process;

#define ERTS_PROCESS_LOCK_ONLY_PROC_LOCK_TYPE__
#ifdef OLD
#include "../old/erl_process_lock.h"
#else
#include "../erl_process_lock.h"
#endif
#undef ERTS_PROCESS_LOCK_ONLY_PROC_LOCK_TYPE__

struct process {
    struct { Eterm id; } common;
    erts_proc_lock_t lock;
};

#define internal_pid_data(pid) ((Uint) (pid))
#define internal_pid_index(pid) ((Uint) (pid))
#define is_not_internal_pid(pid) 0
#define ERTS_PROC_IS_EXITING(p) 0

typedef struct { int unused; } ErtsPTab;
static ErtsPTab erts_proc;
static Process erts_invalid_process;
static Uint erts_no_schedulers = 1;
static inline Uint erts_get_scheduler_id(void) { return 1; }
static inline int erts_ptab_max(ErtsPTab *t) { (void) t; return 0; }
static inline erts_aint_t erts_ptab_pix2intptr_nob(ErtsPTab *t, Uint ix)
{ (void) t; (void) ix; return 0; }
static inline erts_aint_t erts_ptab_pix2intptr_ddrb(ErtsPTab *t, Uint ix)
{ (void) t; (void) ix; return 0; }
static inline void erts_ptab_atmc_inc_refc(void *c) { (void) c; }
static inline Sint erts_ptab_atmc_dec_test_refc(void *c) { (void) c; return 1; }
static inline Sint erts_ptab_atmc_add_test_refc(void *c, Sint a)
{ (void) c; (void) a; return 1; }
static inline Sint erts_ptab_atmc_read_refc(void *c) { (void) c; return 1; }
static inline void erts_free_proc(Process *p) { (void) p; }

#ifdef OLD
#include "../old/erl_process_lock.h"
#else
#include "../erl_process_lock.h"
#endif

#endif
