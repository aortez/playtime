# Linearizable key-value history, version 1

Implement `linearize(history, initial=None)` in `linearize.py` using Python
3.12's standard library. Return the lexicographically smallest list of operation
IDs that is a legal atomic ordering of the entire history, or None if no such
ordering exists. Empty history returns []. Initial state defaults to {}.
Do not mutate history, initial, or their nested values.

`history` is a list of at most 18 dictionaries with required fields `id`,
`start`, `end`, `op`, `result`. Extra fields are ignored. IDs are distinct
nonempty strings. Start/end are integers (not booleans), with start < end.
If A.end <= B.start, A MUST precede B. Otherwise their intervals overlap and
either order is allowed; list order is irrelevant. Ordering is lexicographic
over ID strings using Python's string order, not by invocation time.

The state maps string keys to string values. Missing keys read as None; the
empty string is a real value. `initial` must be a dict of strings to strings.
An operation is one of these exact list shapes:

- `["get", key]`: result is the observed string or None; state is unchanged.
- `["put", key, value]`: key/value are strings; result must be None. Set the key.
- `["cas", expected, writes]`: expected/writes are dictionaries whose keys are
  strings and whose values are strings or None. Result is a bool. The comparison
  succeeds iff EVERY expected key has its stated value (None means absent).
  The recorded bool must equal that comparison. On success apply ALL writes
  atomically: None deletes a key, a string sets it. On failure apply no writes.
  Empty expected always succeeds; empty writes is valid.

All recorded results must agree with the state immediately before their atomic
operation. A read may observe an overlapping write invoked later. A locally
legal choice may prevent any complete ordering, so choosing the smallest
currently legal ID without backtracking is insufficient. Conversely, more than
one complete ordering can be valid; return the lexicographically smallest one.

Raise ValueError for any invalid input shape/type, duplicate ID or invalid
timestamp, even when another record already makes the history impossible.
Validate the complete input before deciding consistency. Non-None `initial`
must be a dictionary; all other type requirements above are exact built-in
types. Test histories contain no cyclic Python containers.

Handle histories up to the stated bound, including many interchangeable
operations followed by an impossible observation, without enumerating every
permutation. A small set of exhaustive permutation tests can be useful for
checking your implementation, but is not sufficient as the main search.

Write runnable tests and README.md explaining search completeness, real-time
precedence, safe pruning and the lexicographic result. Include a case that
requires backtracking and an impossible atomic multi-key history. Report checks
actually run. Automated scoring checks behavior; prose is reviewed separately.
