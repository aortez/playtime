"""Independent Floyd-Warshall oracle for a Bellman-Ford-style problem."""
import copy
import importlib.util
import sys
from assessment_support import emit_report
import json
from pathlib import Path
import random


def oracle(n, edges, source, target):
    d = [[None] * n for _ in range(n)]
    for i in range(n):
        d[i][i] = 0
    for u, v, w in edges:
        d[u][v] = w if d[u][v] is None else min(d[u][v], w)
    for k in range(n):
        for i in range(n):
            for j in range(n):
                if d[i][k] is not None and d[k][j] is not None:
                    value = d[i][k] + d[k][j]
                    if d[i][j] is None or value < d[i][j]:
                        d[i][j] = value
    if any(d[source][k] is not None and d[k][k] < 0 and d[k][target] is not None for k in range(n)):
        return float('-inf')
    return d[source][target]


def check(n, edges, source, target, expected):
    original = copy.deepcopy(edges)
    actual = solve(n, edges, source, target)
    assert edges == original, 'mutated edges'
    assert actual == expected, ('actual', actual, 'expected', expected)
    if type(expected) is int:
        assert type(actual) is int, ('not exact Python int', type(actual).__name__)
    elif expected is None:
        assert actual is None


FIXED = [
    ('empty_walk', 1, [], 0, 0, 0),
    ('unreachable', 3, [(0, 1, 3)], 0, 2, None),
    ('negative_edges', 4, [(0, 1, 5), (0, 2, 1), (1, 2, -10), (2, 3, 4)], 0, 3, -1),
    ('parallel_edges', 2, [(0, 1, 7), (0, 1, -3), (0, 1, 2)], 0, 1, -3),
    ('negative_self_loop', 2, [(0, 0, -1), (0, 1, 2)], 0, 1, float('-inf')),
    ('negative_cycle_to_target', 4, [(0, 1, 0), (1, 2, -2), (2, 1, 1), (2, 3, 10)], 0, 3, float('-inf')),
    ('unreachable_negative_cycle', 4, [(0, 3, 5), (1, 2, -2), (2, 1, 1), (2, 3, 0)], 0, 3, 5),
    ('cycle_cannot_reach_target', 4, [(0, 1, 0), (1, 2, -2), (2, 1, 1), (0, 3, 7)], 0, 3, 7),
    ('cycle_and_unreachable_target', 3, [(0, 1, 0), (1, 1, -1)], 0, 2, None),
    ('zero_cycle', 3, [(0, 1, -3), (1, 0, 3), (1, 2, 8)], 0, 2, 5),
    ('source_target_cycle', 2, [(0, 1, -2), (1, 0, 1)], 0, 0, float('-inf')),
    ('source_target_no_return', 2, [(0, 1, 0), (1, 1, -1)], 0, 0, 0),
    ('huge_integer', 3, [(0, 1, 10**400), (1, 2, -10**400 + 7), (0, 2, 9)], 0, 2, 7),
    ('above_float_precision', 3, [(0, 1, 2**60), (1, 2, 1), (0, 2, 2**60 + 2)], 0, 2, 2**60 + 1),
    ('reverse_edge_order', 150, [(i, i+1, -1) for i in reversed(range(149))], 0, 149, -149),
]


def generated():
    rng = random.Random(2061122)
    for _ in range(160):
        n = rng.randint(1, 7)
        edges = [(rng.randrange(n), rng.randrange(n), rng.randint(-7, 9))
                 for _ in range(rng.randrange(2*n*n + 1))]
        source, target = rng.randrange(n), rng.randrange(n)
        check(n, edges, source, target, oracle(n, edges, source, target))


try:
    spec = importlib.util.spec_from_file_location('shortestwalk', '/candidate/shortestwalk.py')
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    solve = module.shortest_walk
    import_error = None
except BaseException as exc:
    sys.modules.pop(spec.name, None)
    import_error = repr(exc)
cases = [(row[0], lambda row=row: check(*row[1:])) for row in FIXED]
cases.append(('seeded_graphs_160', generated))
results = []
for name, test in cases:
    try:
        assert import_error is None, import_error
        test()
        results.append(dict(case=name, passed=True))
    except BaseException as exc:
        results.append(dict(case=name, passed=False, error=repr(exc)[:2000]))
results.append(dict(case='readme', passed=Path('/candidate/README.md').is_file()))
emit_report(dict(passed=sum(x['passed'] for x in results), total=len(results), results=results))
raise SystemExit(0 if all(x['passed'] for x in results) else 1)
