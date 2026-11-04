/* Variants of ck_fifo_mpmc (ck/include/ck_fifo.h, BSD license in
   ck/LICENSE), included by ck_fifo_mpmc.c.

   OLD_ENQ: ck_fifo_mpmc_enqueue from before ck commit ed6bace.
   OLD_DEQ: ck_fifo_mpmc_dequeue from before ck commit e78c421.
   FENCE_DEQ: the dequeue (current, or with OLD_DEQ the old one) with a
   load-store fence between its read of the value and the compare-and-swap
   that moves the head.

   They are copies of ck's code at those commits' parents (or at 8f0847c
   for the current dequeue), renamed, with the generation counter a
   uintptr_t, as since ed6bace, rather than a char *. */

#ifdef FENCE_DEQ
#define DEQ_FENCE() ck_pr_fence_load_store()
#else
#define DEQ_FENCE() ((void)0)
#endif

#ifdef OLD_ENQ
/* Before ed6bace: a node's generation starts at 0, and nothing orders
   the load of the tail's next pointer before the re-check of the tail's
   generation. */
CK_CC_INLINE static void
old_ck_fifo_mpmc_enqueue(struct ck_fifo_mpmc *fifo,
		     struct ck_fifo_mpmc_entry *entry,
		     void *value)
{
	struct ck_fifo_mpmc_pointer tail, next, update;

	/*
	 * Prepare the upcoming node and make sure to commit the updates
	 * before publishing.
	 */
	entry->value = value;
	entry->next.pointer = NULL;
	entry->next.generation = 0;
	ck_pr_fence_store_atomic();

	for (;;) {
		tail.generation = (uintptr_t)ck_pr_load_ptr(&fifo->tail.generation);
		ck_pr_fence_load();
		tail.pointer = ck_pr_load_ptr(&fifo->tail.pointer);
		next.generation = (uintptr_t)ck_pr_load_ptr(&tail.pointer->next.generation);
		ck_pr_fence_load();
		next.pointer = ck_pr_load_ptr(&tail.pointer->next.pointer);

		if ((uintptr_t)ck_pr_load_ptr(&fifo->tail.generation) != tail.generation)
			continue;

		if (next.pointer != NULL) {
			/*
			 * If the tail pointer has an entry following it then
			 * it needs to be forwarded to the next entry. This
			 * helps us guarantee we are always operating on the
			 * last entry.
			 */
			update.pointer = next.pointer;
			update.generation = tail.generation + 1;
			ck_pr_cas_ptr_2(&fifo->tail, &tail, &update);
		} else {
			/*
			 * Attempt to commit new entry to the end of the
			 * current tail.
			 */
			update.pointer = entry;
			update.generation = next.generation + 1;
			if (ck_pr_cas_ptr_2(&tail.pointer->next, &next, &update) == true)
				break;
		}
	}

	ck_pr_fence_atomic();

	/* After a successful insert, forward the tail to the new entry. */
	update.generation = tail.generation + 1;
	ck_pr_cas_ptr_2(&fifo->tail, &tail, &update);
	return;
}
#define ck_fifo_mpmc_enqueue old_ck_fifo_mpmc_enqueue
#endif

#ifdef OLD_DEQ
/* Before e78c421: an empty observation is not re-validated against the
   head's generation. */
CK_CC_INLINE static bool
old_ck_fifo_mpmc_dequeue(struct ck_fifo_mpmc *fifo,
		     void *value,
		     struct ck_fifo_mpmc_entry **garbage)
{
	struct ck_fifo_mpmc_pointer head, tail, next, update;

	for (;;) {
		head.generation = (uintptr_t)ck_pr_load_ptr(&fifo->head.generation);
		ck_pr_fence_load();
		head.pointer = ck_pr_load_ptr(&fifo->head.pointer);
		tail.generation = (uintptr_t)ck_pr_load_ptr(&fifo->tail.generation);
		ck_pr_fence_load();
		tail.pointer = ck_pr_load_ptr(&fifo->tail.pointer);

		next.generation = (uintptr_t)ck_pr_load_ptr(&head.pointer->next.generation);
		ck_pr_fence_load();
		next.pointer = ck_pr_load_ptr(&head.pointer->next.pointer);

		update.pointer = next.pointer;
		if (head.pointer == tail.pointer) {
			/*
			 * The head is guaranteed to always point at a stub
			 * entry. If the stub entry has no references then the
			 * queue is empty.
			 */
			if (next.pointer == NULL)
				return false;

			/* Forward the tail pointer if necessary. */
			update.generation = tail.generation + 1;
			ck_pr_cas_ptr_2(&fifo->tail, &tail, &update);
		} else {
			/*
			 * It is possible for head snapshot to have been
			 * re-used. Avoid deferencing during enqueue
			 * re-use.
			 */
			if (next.pointer == NULL)
				continue;

			/* Save value before commit. */
			*(void **)value = ck_pr_load_ptr(&next.pointer->value);
			DEQ_FENCE();

			/* Forward the head pointer to the next entry. */
			update.generation = head.generation + 1;
			if (ck_pr_cas_ptr_2(&fifo->head, &head, &update) == true)
				break;
		}
	}

	*garbage = head.pointer;
	return true;
}
#define ck_fifo_mpmc_dequeue old_ck_fifo_mpmc_dequeue
#elif defined(FENCE_DEQ)
/* The current dequeue. */
CK_CC_INLINE static bool
fenced_ck_fifo_mpmc_dequeue(struct ck_fifo_mpmc *fifo,
		     void *value,
		     struct ck_fifo_mpmc_entry **garbage)
{
	struct ck_fifo_mpmc_pointer head, tail, next, update;

	for (;;) {
		head.generation = (uintptr_t)ck_pr_load_ptr(&fifo->head.generation);
		ck_pr_fence_load();
		head.pointer = ck_pr_load_ptr(&fifo->head.pointer);
		tail.generation = (uintptr_t)ck_pr_load_ptr(&fifo->tail.generation);
		ck_pr_fence_load();
		tail.pointer = ck_pr_load_ptr(&fifo->tail.pointer);

		next.generation = (uintptr_t)ck_pr_load_ptr(&head.pointer->next.generation);
		ck_pr_fence_load();
		next.pointer = ck_pr_load_ptr(&head.pointer->next.pointer);

		update.pointer = next.pointer;
		if (head.pointer == tail.pointer) {
			/*
			 * The head is guaranteed to always point at a stub
			 * entry. If the stub entry has no references then the
			 * queue is empty. However, the head snapshot may have
			 * been re-used by this point (dequeued elsewhere and
			 * re-enqueued as the tail with a re-initialized next
			 * pointer), so the empty observation is only valid if
			 * the head has not moved since it was read.
			 */
			if (next.pointer == NULL) {
				ck_pr_fence_load();
				if ((uintptr_t)ck_pr_load_ptr(&fifo->head.generation) != head.generation)
					continue;

				return false;
			}

			/* Forward the tail pointer if necessary. */
			update.generation = tail.generation + 1;
			ck_pr_cas_ptr_2(&fifo->tail, &tail, &update);
		} else {
			/*
			 * It is possible for head snapshot to have been
			 * re-used. Avoid deferencing during enqueue
			 * re-use.
			 */
			if (next.pointer == NULL)
				continue;

			/* Save value before commit. */
			*(void **)value = ck_pr_load_ptr(&next.pointer->value);
			DEQ_FENCE();

			/* Forward the head pointer to the next entry. */
			update.generation = head.generation + 1;
			if (ck_pr_cas_ptr_2(&fifo->head, &head, &update) == true)
				break;
		}
	}

	*garbage = head.pointer;
	return true;
}
#define ck_fifo_mpmc_dequeue fenced_ck_fifo_mpmc_dequeue
#endif
