# Delta debugging, version 1

Implement `ddmin(items, fails)` in `ddmin.py`, using Python 3.12's standard library.
`items` is a finite list. `fails` is a deterministic predicate accepting a list and
returning bool; the original list is guaranteed to fail. It need not be monotone.

Return an order-preserving subsequence that still fails and is **1-minimal**:
deleting any one element of your result must make the predicate false. If the
empty list fails, return it. There may be several valid outputs; global minimum
cardinality is not required. Preserve duplicate occurrences and object identities;
items may be unhashable. Do not mutate the input or its elements. Calls to the
predicate receive a new list; the predicate does not modify elements but may retain
the list it was passed. Later calls must not mutate a previously passed list.
Each predicate argument must itself be an order-preserving subsequence of the
original input; never invent, reorder or duplicate occurrences in a query.

Use chunk/complement deletion with increasing granularity and restart or adjust
the search after a successful deletion. A final singleton-deletion pass is allowed.
Across each invocation, use at most `4*n*n + 4*n + 8` predicate calls, where n is
the original length. This generous bound rules out exhaustive subset enumeration;
within it, no specific partition schedule or failing subsequence is required.

Write your own runnable tests and README.md. Explain termination and 1-minimality,
why non-monotone predicates require reconsidering deletions, and give an example
where a 1-minimal result is not globally smallest. Report checks actually run.

Adapted from Q33 of the Reddit Brutal Model Test:
https://www.reddit.com/r/LocalLLM/comments/1xnikyz/please_test_your_models_before_spending_money_on/
The mutation rules and generous query bound are explicit additions. Automated
checks verify behavior and query counts; the written proof needs separate review.
