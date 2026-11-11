# SQL tombstones and version selection, version 1

Implement `query.sql` for SQLite 3 using a SQL window function. The table is:

```sql
CREATE TABLE events (
  resource_id TEXT NOT NULL, version INTEGER NOT NULL,
  event_id INTEGER PRIMARY KEY, deleted INTEGER NOT NULL,
  payload TEXT NOT NULL
);
```

The named integer parameter `:cutoff` is supplied by the caller. For each resource,
choose the greatest `(version, event_id)` among rows with `version <= :cutoff`.
Return that row only if `deleted = 0`. A later tombstone must not resurrect an
older live row. A row after the cutoff must not affect the result. Deleted is
always 0 or 1; integers fit SQLite's signed 64-bit range; all columns are non-NULL.

Return columns `resource_id, version, event_id, payload`, in exactly that order,
sorted by resource_id using SQLite's BINARY collation. The table may be empty,
and versions need not be positive, unique or contiguous. Resource names and
payloads may contain arbitrary Unicode text, quotes and empty strings.

Submit one read-only SQL statement in `query.sql`; no database mutation,
extensions, user-defined functions or external dependencies. The checker supplies
the database and parameter. Write your own runnable tests and README.md, including
a minimal counterexample to filtering tombstones before selecting the latest row
and an explanation of how equal versions are resolved. Describe checks actually run.

Adapted from Q24 of the Reddit Brutal Model Test:
https://www.reddit.com/r/LocalLLM/comments/1xnikyz/please_test_your_models_before_spending_money_on/
The SQL dialect, result order and parameter interface are pinned here for repeatable
assessment. Behavioral checks do not establish the quality of the written explanation.
