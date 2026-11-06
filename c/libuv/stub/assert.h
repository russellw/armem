/* Freestanding: a failed assertion traps. */
#define assert(x) do { if (!(x)) __builtin_trap(); } while (0)
