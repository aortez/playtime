# Async single-flight, version 1

Implement `SingleFlight` in `singleflight.py`, using Python 3.12's standard library.
Its public interface is `async get(key, factory)`. A factory takes no arguments and
returns an awaitable; keys are hashable. All use occurs on one event loop.

Concurrent calls on the same instance and key must share one in-flight factory
invocation. Different keys run independently. Cancellation of any or all callers
must not cancel the shared work. Each active waiter receives its result or exception.
Retrieve failed background computations' exceptions even when all waiters cancel,
so the event loop never reports an unhandled task exception.

Completed successes and failures are not cached. A new call after computation
completion must start a new computation, even if old cleanup callbacks have not
run yet. Old cleanup must never remove a newer flight for the same key. A factory
that raises before returning its awaitable must also propagate and permit retry.
Instances must not share state with each other.

Write your own runnable tests and README.md explaining usage, cancellation,
cleanup races, and the checks you actually ran. Do not access the network or use
external packages. The private assessment is unavailable during your attempt.

Adapted from Q20 of the Reddit Brutal Model Test; API and race requirements are
pinned here for automated assessment. This is a tool-assisted coding task.
