"""Decimal oracle deliberately uses the ordinary formula at high precision."""
from decimal import Decimal, localcontext
import importlib.util
import math
from pathlib import Path
import random
import sys
from assessment_support import emit_report, UnavailableModule

try:
    spec = importlib.util.spec_from_file_location('quadratic', '/candidate/quadratic.py')
    module = importlib.util.module_from_spec(spec)
    sys.modules['quadratic'] = module
    spec.loader.exec_module(module)
except BaseException as error:
    module = UnavailableModule(error)


def check(a, b, c):
    with localcontext() as context:
        context.prec = 400
        x, y, z = map(Decimal.from_float, (a, b, c))
        discriminant = y*y - 4*x*z
        expected = sorted([(-y - discriminant.sqrt()) / (2*x),
                           (-y + discriminant.sqrt()) / (2*x)])
    actual = module.roots(a, b, c)
    assert type(actual) is tuple and len(actual) == 2, 'expected a two-element tuple'
    assert all(type(value) in (float, int) and math.isfinite(value) for value in actual), 'nonfinite/nonreal roots'
    assert actual[0] <= actual[1], 'roots not sorted'
    for got, want in zip(actual, expected):
        target = float(want)
        assert abs(got-target) <= max(1e-300, 1e-12*abs(target)), (actual, tuple(map(float, expected)))


def invalid():
    for args in [(0.0, 1.0, 2.0), (-0.0, 1.0, 2.0), (1.0, 0.0, 1.0),
                 (1.0, 1.0, 1.0), (-1.0, 0.0, -1.0)]:
        try:
            module.roots(*args)
        except ValueError:
            continue
        raise AssertionError(('expected ValueError', args))


def nonfinite():
    for bad in (float('nan'), float('inf'), -float('inf')):
        for position in range(3):
            args = [1.0, -3.0, 2.0]
            args[position] = bad
            try:
                module.roots(*args)
            except ValueError:
                continue
            raise AssertionError(('expected ValueError', args))


def generated():
    randomizer = random.Random(3507)
    for trial in range(80):
        small = randomizer.choice([-1.0, 1.0]) * 10.0**randomizer.randrange(-60, -4)
        large = randomizer.choice([-1.0, 1.0]) * 10.0**randomizer.randrange(5, 61)
        a = randomizer.choice([-1.0, 1.0]) * 2.0**randomizer.randrange(-10, 11)
        check(a, -a*(small+large), a*small*large)


FIXED = [
    ('ordinary', (1.0, -3.0, 2.0)),
    ('cancellation_positive', (1.0, -1e16, 1.0)),
    ('cancellation_negative', (1.0, 1e16, 1.0)),
    ('opposite_sign_roots', (1.0, -1e16, -1.0)),
    ('negative_leading', (-1.0, 1e16, -1.0)),
    ('repeated_positive', (1.0, -2.0, 1.0)),
    ('repeated_negative', (1.0, 2.0, 1.0)),
    ('repeated_zero', (1.0, 0.0, 0.0)),
    ('negative_repeated_zero', (-1.0, -0.0, 0.0)),
    ('zero_b', (1.0, 0.0, -4.0)),
    ('zero_c_positive_b', (1.0, 3.0, 0.0)),
    ('zero_c_negative_b', (1.0, -3.0, 0.0)),
    ('scaled_coefficients', (1e100, -3e100, 2e100)),
    ('tiny_root', (1.0, -1e100, 1e-100)),
]
results = []
cases = [(name, lambda args=args: check(*args)) for name, args in FIXED]
cases += [('invalid_domain', invalid), ('nonfinite_inputs', nonfinite), ('seeded_scales_80', generated)]
for name, call in cases:
    try:
        call()
        results.append(dict(case=name, passed=True))
    except BaseException as error:
        results.append(dict(case=name, passed=False, error=repr(error)[:2000]))
results.append(dict(case='readme', passed=Path('/candidate/README.md').is_file()))
emit_report(dict(passed=sum(row['passed'] for row in results), total=len(results), results=results))
raise SystemExit(0 if all(row['passed'] for row in results) else 1)
