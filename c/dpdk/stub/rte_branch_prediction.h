#ifndef _RTE_BRANCH_PREDICTION_H_
#define _RTE_BRANCH_PREDICTION_H_
#define likely(x) __builtin_expect(!!(x), 1)
#define unlikely(x) __builtin_expect(!!(x), 0)
#endif
