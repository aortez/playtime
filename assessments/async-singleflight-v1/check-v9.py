"""Private, deterministic event/gate-based checks; run in the assessment container."""
import asyncio
import gc
import importlib.util
import sys
from assessment_support import emit_report
import json
from pathlib import Path


async def ticks():
    for _ in range(8):
        await asyncio.sleep(0)


async def scenario(kind):
    sf = SingleFlight()
    gate = asyncio.Event()
    calls = []
    cancelled = []
    sentinel = object()

    async def factory():
        calls.append(1)
        try:
            await gate.wait()
        except asyncio.CancelledError:
            cancelled.append(1)
            raise
        if kind in ('errors', 'orphan_error'):
            raise LookupError('shared failure')
        return sentinel

    a = asyncio.create_task(sf.get('k', factory))
    b = asyncio.create_task(sf.get('k', factory))
    await ticks()
    assert len(calls) == 1, 'duplicate factory calls'
    if kind in ('one_cancel', 'all_cancel', 'orphan_error'):
        a.cancel()
        if kind != 'one_cancel':
            b.cancel()
        await ticks()
        assert not cancelled, 'waiter cancelled shared work'
        if kind == 'one_cancel':
            assert not b.done(), 'other waiter prematurely completed'
    gate.set()
    outcomes = await asyncio.gather(a, b, return_exceptions=True)
    await ticks()
    if kind == 'errors':
        assert all(isinstance(x, LookupError) and str(x) == 'shared failure' for x in outcomes)
    elif kind == 'one_cancel':
        assert isinstance(outcomes[0], asyncio.CancelledError) and outcomes[1] is sentinel
    elif kind not in ('all_cancel', 'orphan_error'):
        assert all(x is sentinel for x in outcomes)
    assert not cancelled
    del a, b, outcomes, sf
    gc.collect()
    await ticks()


async def independent():
    sf = SingleFlight()
    gate = asyncio.Event()

    async def blocked():
        await gate.wait()
        return 'a'

    async def ready():
        return 'b'

    a = asyncio.create_task(sf.get('a', blocked))
    await ticks()
    assert await sf.get('b', ready) == 'b'
    assert await SingleFlight().get('a', ready) == 'b'
    assert not a.done()
    gate.set()
    assert await a == 'a'


async def no_cache(fail=False):
    sf = SingleFlight()
    count = 0

    async def factory():
        nonlocal count
        count += 1
        if fail:
            raise LookupError(str(count))
        return count

    for expected in range(1, 5):
        try:
            result = await sf.get('k', factory)
            assert not fail and result == expected
        except LookupError as exc:
            assert fail and str(exc) == str(expected)


async def completion_race():
    sf = SingleFlight()
    gates = [asyncio.Event(), asyncio.Event()]
    started = []
    replacements = []

    async def work():
        i = len(started)
        started.append(i)
        if i == 0:
            # Queue a new caller before the shared task's completion callbacks.
            replacements.append(asyncio.create_task(sf.get('k', work)))
        await gates[i].wait()
        return i

    gates[0].set()
    assert await sf.get('k', work) == 0
    await ticks()
    assert len(started) == 2, 'completed result cached or replacement not started'
    c = asyncio.create_task(sf.get('k', work))
    await ticks()
    assert len(started) == 2, 'old cleanup deleted replacement flight'
    gates[1].set()
    assert await replacements[0] == 1 and await c == 1


async def synchronous_failure():
    sf = SingleFlight()
    def fail():
        raise ValueError('factory failed synchronously')
    try:
        await sf.get('k', fail)
    except ValueError:
        pass
    else:
        raise AssertionError('missing factory exception')
    async def good():
        return 7
    assert await sf.get('k', good) == 7


CASES = [(name, lambda name=name: scenario(name)) for name in
         ['shared', 'one_cancel', 'all_cancel', 'errors', 'orphan_error']]
CASES += [('independent_keys_instances', independent),
          ('success_not_cached', no_cache),
          ('failure_not_cached', lambda: no_cache(True)),
          ('completion_cleanup_race', completion_race),
          ('synchronous_factory_failure', synchronous_failure)]
results = []
try:
    spec = importlib.util.spec_from_file_location('singleflight', '/candidate/singleflight.py')
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    SingleFlight = module.SingleFlight
    import_error = None
except BaseException as exc:
    sys.modules.pop(spec.name, None)
    import_error = repr(exc)
for name, call in CASES:
    warnings = []
    # Bind this case's list; a late exception callback must not reach the next one.
    async def run(warnings=warnings):
        asyncio.get_running_loop().set_exception_handler(lambda loop, ctx: warnings.append(str(ctx)))
        await asyncio.wait_for(call(), 2)
        gc.collect()
        await ticks()
        assert not warnings, warnings
    try:
        assert import_error is None, import_error
        asyncio.run(run())
        # Collect this case's leftovers now so a later case is not blamed for them.
        gc.collect()
        assert not warnings, warnings
        results.append(dict(case=name, passed=True))
    except BaseException as exc:
        results.append(dict(case=name, passed=False, error=repr(exc)[:2000]))
results.append(dict(case='readme', passed=Path('/candidate/README.md').is_file()))
emit_report(dict(passed=sum(x['passed'] for x in results), total=len(results), results=results))
raise SystemExit(0 if all(x['passed'] for x in results) else 1)
