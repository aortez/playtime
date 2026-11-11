"""Check properties of any valid minimizer, rather than a preferred output."""
import copy
import importlib.util
from pathlib import Path
import random
import sys
from assessment_support import emit_report, UnavailableModule

try:
    spec = importlib.util.spec_from_file_location('ddmin', '/candidate/ddmin.py')
    module = importlib.util.module_from_spec(spec)
    sys.modules['ddmin'] = module
    spec.loader.exec_module(module)
except BaseException as error:
    module = UnavailableModule(error)


def subsequence(candidate, original):
    position = 0
    for item in candidate:
        while position < len(original) and original[position] is not item:
            position += 1
        if position == len(original):
            return False
        position += 1
    return True


def check(items, predicate):
    original = list(items)
    snapshot = copy.deepcopy(items)
    calls = []
    limit = 4 * len(items)**2 + 4 * len(items) + 8
    def fails(candidate):
        assert type(candidate) is list, 'predicate needs a list'
        assert candidate is not items and all(candidate is not previous for previous, _ in calls), 'query list reused'
        assert subsequence(candidate, original), 'query is not a subsequence'
        assert len(calls) < limit, 'predicate-call budget exceeded'
        calls.append((candidate, list(candidate)))
        return predicate(candidate)
    assert predicate(items), 'invalid private fixture'
    answer = module.ddmin(items, fails)
    assert type(answer) is list and subsequence(answer, original), 'invalid result'
    assert predicate(answer), 'result no longer fails'
    assert not predicate([]) or answer == [], 'missed failing empty input'
    assert all(not predicate(answer[:i] + answer[i+1:]) for i in range(len(answer))), 'not 1-minimal'
    assert items == snapshot and len(items) == len(original), 'input mutated'
    assert all(a is b for a, b in zip(items, original)), 'input elements replaced'
    for passed, saved in calls:
        assert len(passed) == len(saved) and all(a is b for a, b in zip(passed, saved)), 'retained query mutated'


def unhashable():
    items = [{'n': i} for i in range(8)]
    check(items, lambda xs: any(x['n'] == 6 for x in xs))


def generated():
    randomizer = random.Random(3307)
    for trial in range(80):
        count = randomizer.randrange(1, 9)
        items = list(range(count))
        randomizer.shuffle(items)
        failures = {mask for mask in range(1 << count) if randomizer.randrange(4) == 0}
        failures.add((1 << count) - 1)
        check(items, lambda xs: sum(1 << x for x in xs) in failures)


CASES = [
    ('empty_fails', lambda: check([], lambda xs: True)),
    ('always_fails', lambda: check(list(range(12)), lambda xs: True)),
    ('singleton', lambda: check([7], lambda xs: len(xs) == 1)),
    ('all_required', lambda: check(list(range(12)), lambda xs: len(xs) == 12)),
    ('one_relevant', lambda: check(list(range(64)), lambda xs: 37 in xs)),
    ('distant_pair', lambda: check(list(range(32)), lambda xs: 1 in xs and 30 in xs)),
    ('ordered_pair', lambda: check(list('dbca'), lambda xs: 'd' in xs and 'a' in xs and xs.index('d') < xs.index('a'))),
    ('duplicates', lambda: check(['x', 'y', 'x', 'z', 'x'], lambda xs: xs.count('x') >= 2)),
    ('unhashable_identity', unhashable),
    ('nonmonotone_cardinality', lambda: check(list(range(8)), lambda xs: len(xs) in (2, 5, 8))),
    ('global_minimum_not_required', lambda: check(list('abcd'), lambda xs: xs in [list('abcd'), list('ab'), list('c')])),
    ('reconsider_deletions', lambda: check(list(range(6)), lambda xs: tuple(xs) in {(0,1,2,3,4,5), (1,2,3,4,5), (1,2,4,5), (2,4,5), (2,5)})),
    ('seeded_nonmonotone_80', generated),
]
results = []
for name, call in CASES:
    try:
        call()
        results.append(dict(case=name, passed=True))
    except BaseException as error:
        results.append(dict(case=name, passed=False, error=repr(error)[:2000]))
results.append(dict(case='readme', passed=Path('/candidate/README.md').is_file()))
emit_report(dict(passed=sum(row['passed'] for row in results), total=len(results), results=results))
raise SystemExit(0 if all(row['passed'] for row in results) else 1)
