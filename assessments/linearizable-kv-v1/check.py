"""Exhaustive small-history oracle, independent of memoized witness search."""
import copy
import importlib.util
import itertools
from pathlib import Path
import random
import sys
from assessment_support import emit_report, UnavailableModule

try:
    spec = importlib.util.spec_from_file_location('linearize', '/candidate/linearize.py')
    module = importlib.util.module_from_spec(spec)
    sys.modules['linearize'] = module
    spec.loader.exec_module(module)
except BaseException as error:
    module = UnavailableModule(error)


def entry(name, op, result, start=0, end=10):
    return dict(id=name, start=start, end=end, op=op, result=result)


def oracle(history, initial):
    for ordering in itertools.permutations(sorted(history, key=lambda e: e['id'])):
        positions = {e['id']: i for i, e in enumerate(ordering)}
        if any(a['end'] <= b['start'] and positions[a['id']] >= positions[b['id']]
               for a in history for b in history):
            continue
        state = dict(initial)
        valid = True
        for event in ordering:
            kind, *args = event['op']
            if kind == 'get':
                observed = state[args[0]] if args[0] in state else None
            elif kind == 'put':
                state[args[0]] = args[1]
                observed = None
            else:
                expected, writes = args
                observed = not any((k not in state if v is not None else k in state)
                                   or (v is not None and state.get(k) != v) for k, v in expected.items())
                if observed:
                    state = {k: v for k, v in state.items() if k not in writes}
                    state.update({k: v for k, v in writes.items() if v is not None})
            if type(observed) is not type(event['result']) or observed != event['result']:
                valid = False
                break
        if valid:
            return [e['id'] for e in ordering]
    return None


def check(history, initial=None):
    initial = {} if initial is None else initial
    before = copy.deepcopy((history, initial))
    want = oracle(history, initial)
    got = module.linearize(history, initial)
    assert got is None or type(got) is list, 'expected list or None'
    assert got == want, (got, want, history)
    assert (history, initial) == before, 'input mutated'


def invalid():
    good = entry('a', ['get', 'x'], None)
    bads = [None, (), [None], [{}], [good, good], [entry('', ['get', 'x'], None)],
            [entry('a', ['get', 'x'], None, True, 10)], [entry('a', ['get', 'x'], None, 4, 4)],
            [entry('a', ['get', 'x'], False)], [entry('a', ['put', 'x', 4], None)],
            [entry('a', ['put', 'x', 'v'], 'v')], [entry('a', ['cas', {}, {}], 1)],
            [entry('a', ['cas', [], {}], True)], [entry('a', ['cas', {'x': 1}, {}], False)],
            [entry('a', ['cas', {}, {1: 'v'}], True)], [entry('a', ['bad'], None)],
            [entry(str(i), ['get', 'x'], None) for i in range(19)],
            [entry('impossible', ['get', 'x'], 'never'), entry('bad', ['get', 'x'], None, 3, 2)]]
    for history in bads:
        before = copy.deepcopy(history)
        try:
            module.linearize(history)
        except ValueError:
            assert history == before
            continue
        raise AssertionError(('expected ValueError', history))


def invalid_initial():
    for initial in ([], (), {'x': None}, {'x': False}, {0: 'v'}):
        try:
            module.linearize([], initial)
        except ValueError:
            continue
        raise AssertionError('expected ValueError')


def generated():
    rng = random.Random(64012)
    for trial in range(80):
        history = []
        initial = {'x': '0'} if trial % 2 else {}
        for i in range(rng.randrange(1, 7)):
            kind = rng.choice(['get', 'put', 'cas'])
            key = rng.choice(['x', 'y'])
            value = rng.choice([None, '', '0', '1'])
            if kind == 'get':
                op, result = ['get', key], value
            elif kind == 'put':
                op, result = ['put', key, str(i % 2)], None
            else:
                op = ['cas', {key: value}, {rng.choice(['x', 'y']): rng.choice([None, '', '1'])}]
                result = bool(rng.randrange(2))
            start, end = (i*2, i*2+1) if trial % 3 == 0 else (rng.randrange(5), rng.randrange(5, 10))
            history.append(entry(chr(97+i), op, result, start, end))
        rng.shuffle(history)
        check(history, initial)


def equivalent_prefixes():
    history = [entry(f'r{i:02}', ['get', 'x'], None) for i in range(13)]
    history.append(entry('z', ['get', 'x'], 'unwritten'))
    before = copy.deepcopy(history)
    assert module.linearize(history) is None
    assert history == before
    history[-1]['result'] = None
    assert module.linearize(history) == sorted(e['id'] for e in history)


def shuffled():
    history = [entry('a', ['put', 'x', '1'], None), entry('b', ['get', 'x'], '0'),
               entry('c', ['get', 'x'], '1')]
    for order in itertools.permutations(history):
        check(list(order), {'x': '0'})


CASES = [
    ('empty', lambda: check([])),
    ('absent_vs_empty', lambda: check([entry('a', ['get', 'x'], '')], {'x': ''})),
    ('overlapping_future_write', lambda: check([entry('a', ['get', 'x'], '1', 0, 5), entry('b', ['put', 'x', '1'], None, 2, 6)])),
    ('real_time_blocks_future', lambda: check([entry('a', ['get', 'x'], '1', 0, 2), entry('b', ['put', 'x', '1'], None, 3, 6)])),
    ('touching_intervals', lambda: check([entry('a', ['get', 'x'], '1', 0, 2), entry('b', ['put', 'x', '1'], None, 2, 6)])),
    ('backtrack_legal_write', lambda: check([entry('a', ['put', 'x', '1'], None), entry('b', ['get', 'x'], '0')], {'x': '0'})),
    ('failed_cas_is_observation', lambda: check([entry('a', ['cas', {'x': '0'}, {'x': '1'}], False)], {'x': '0'})),
    ('cas_checks_every_key', lambda: check([entry('a', ['cas', {'x': '0', 'y': '0'}, {'x': '1'}], True)], {'x': '0'})),
    ('failed_cas_no_writes', lambda: check([entry('a', ['cas', {'x': 'missing'}, {'x': '1', 'y': '1'}], False, 0, 1), entry('b', ['get', 'y'], None, 1, 2)])),
    ('atomic_batch_no_partial_visibility', lambda: check([entry('a', ['cas', {}, {'x': '1', 'y': '1'}], True), entry('b', ['get', 'x'], '1', 1, 2), entry('c', ['get', 'y'], None, 2, 3)])),
    ('delete_to_absent', lambda: check([entry('a', ['cas', {'x': ''}, {'x': None}], True, 0, 1), entry('b', ['get', 'x'], None, 1, 2)], {'x': ''})),
    ('empty_cas_maps', lambda: check([entry('a', ['cas', {}, {}], True)])),
    ('multi_key_cycle', lambda: check([entry('a', ['cas', {'y': None}, {'x': '1'}], True), entry('b', ['cas', {'x': None}, {'y': '1'}], True)])),
    ('lexicographic_witness', lambda: check([entry('z', ['put', 'x', '1'], None), entry('é', ['put', 'y', '2'], None), entry('A', ['get', 'z'], None)])),
    ('state_matters_for_pruning', lambda: check([entry('a', ['put', 'x', '1'], None, 0, 2), entry('b', ['put', 'x', '2'], None, 0, 2), entry('c', ['get', 'x'], '1', 2, 3)])),
    ('shuffled_input', shuffled),
    ('invalid_history_validation', invalid),
    ('invalid_initial', invalid_initial),
    ('seeded_permutation_oracle_80', generated),
    ('equivalent_prefixes', equivalent_prefixes),
]
results = []
for name, call in CASES:
    try:
        call()
        results.append(dict(case=name, passed=True))
    except BaseException as error:
        results.append(dict(case=name, passed=False, error=repr(error)[:2000]))
results.append(dict(case='readme', passed=Path('/candidate/README.md').is_file()))
emit_report(dict(passed=sum(r['passed'] for r in results), total=len(results), results=results))
raise SystemExit(0 if all(r['passed'] for r in results) else 1)
