/* ck_epoch_poll_deferred from before ck commit d1b348c (ck/src/ck_epoch.c
   at d1b348c^, BSD license in ck/LICENSE), renamed, with a wrapper like
   ck_epoch_poll. Included by ck_epoch_poll3.c with -DOLD_POLL. */
static bool
old_ck_epoch_poll_deferred(struct ck_epoch_record *record, ck_stack_t *deferred)
{
	bool active;
	unsigned int epoch;
	struct ck_epoch_record *cr = NULL;
	struct ck_epoch *global = record->global;
	unsigned int n_dispatch;

	epoch = ck_pr_load_uint(&global->epoch);

	/* Serialize epoch snapshots with respect to global epoch. */
	ck_pr_fence_memory();

	/*
	 * At this point, epoch is the current global epoch value.
	 * There may or may not be active threads which observed epoch - 1.
	 * (ck_epoch_scan() will tell us that). However, there should be
	 * no active threads which observed epoch - 2.
	 *
	 * Note that checking epoch - 2 is necessary, as race conditions can
	 * allow another thread to increment the global epoch before this
	 * thread runs.
	 */
	n_dispatch = ck_epoch_dispatch(record, epoch - 2, deferred);

	cr = ck_epoch_scan(global, cr, epoch, &active);
	if (cr != NULL)
		return (n_dispatch > 0);

	/* We are at a grace period if all threads are inactive. */
	if (active == false) {
		ck_pr_store_uint(&record->epoch, epoch);

		for (epoch = 0; epoch < CK_EPOCH_LENGTH; epoch++)
			ck_epoch_dispatch(record, epoch, deferred);

		return true;
	}

	/*
	 * If an active thread exists, rely on epoch observation.
	 *
	 * All the active threads entered the epoch section during
	 * the current epoch. Therefore, we can now run the handlers
	 * for the immediately preceding epoch and attempt to
	 * advance the epoch if it hasn't been already.
	 */
	(void)ck_pr_cas_uint(&global->epoch, epoch, epoch + 1);

	ck_epoch_dispatch(record, epoch - 1, deferred);
	return true;
}
static bool old_ck_epoch_poll(struct ck_epoch_record *r) { return old_ck_epoch_poll_deferred(r, NULL); }
