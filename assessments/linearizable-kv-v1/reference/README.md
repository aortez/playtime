Validate every record first. Construct predecessor bitmasks from end <= start.
Search eligible operations in ID order, checking observations and applying an
atomic copy of each state transition. Backtrack on an impossible suffix.
Memoizing failed (completed subset, state) pairs merges equivalent prefixes;
the subset alone is unsafe because write order can change state. The first
complete witness is lexicographically smallest. The private checker compares
small cases against independent exhaustive permutation enumeration.
