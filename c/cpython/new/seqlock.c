/* The seqlock section of CPython's Python/lock.c, unchanged, at commit
   1d3cf79a5 (gh-121368, "Fix seq lock memory ordering in _PyType_Lookup"). */

#define SEQLOCK_IS_UPDATING(sequence) (sequence & 0x01)

void _PySeqLock_LockWrite(_PySeqLock *seqlock)
{
    // lock by moving to an odd sequence number
    uint32_t prev = _Py_atomic_load_uint32_relaxed(&seqlock->sequence);
    while (1) {
        if (SEQLOCK_IS_UPDATING(prev)) {
            // Someone else is currently updating the cache
            _Py_yield();
            prev = _Py_atomic_load_uint32_relaxed(&seqlock->sequence);
        }
        else if (_Py_atomic_compare_exchange_uint32(&seqlock->sequence, &prev, prev + 1)) {
            // We've locked the cache
            _Py_atomic_fence_release();
            break;
        }
        else {
            _Py_yield();
        }
    }
}

void _PySeqLock_AbandonWrite(_PySeqLock *seqlock)
{
    uint32_t new_seq = _Py_atomic_load_uint32_relaxed(&seqlock->sequence) - 1;
    assert(!SEQLOCK_IS_UPDATING(new_seq));
    _Py_atomic_store_uint32(&seqlock->sequence, new_seq);
}

void _PySeqLock_UnlockWrite(_PySeqLock *seqlock)
{
    uint32_t new_seq = _Py_atomic_load_uint32_relaxed(&seqlock->sequence) + 1;
    assert(!SEQLOCK_IS_UPDATING(new_seq));
    _Py_atomic_store_uint32(&seqlock->sequence, new_seq);
}

uint32_t _PySeqLock_BeginRead(_PySeqLock *seqlock)
{
    uint32_t sequence = _Py_atomic_load_uint32_acquire(&seqlock->sequence);
    while (SEQLOCK_IS_UPDATING(sequence)) {
        _Py_yield();
        sequence = _Py_atomic_load_uint32_acquire(&seqlock->sequence);
    }

    return sequence;
}

int _PySeqLock_EndRead(_PySeqLock *seqlock, uint32_t previous)
{
    // gh-121368: We need an explicit acquire fence here to ensure that
    // this load of the sequence number is not reordered before any loads
    // within the read lock.
    _Py_atomic_fence_acquire();

    if (_Py_atomic_load_uint32_relaxed(&seqlock->sequence) == previous) {
        return 1;
    }

    _Py_yield();
    return 0;
}

int _PySeqLock_AfterFork(_PySeqLock *seqlock)
{
    // Synchronize again and validate that the entry hasn't been updated
    // while we were readying the values.
    if (SEQLOCK_IS_UPDATING(seqlock->sequence)) {
        seqlock->sequence = 0;
        return 1;
    }

    return 0;
}

