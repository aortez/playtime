"""Independent pricing tables and unchanged legacy behavior; Docker only."""
import hashlib
import importlib.util
from pathlib import Path
import sys
from assessment_support import emit_report, UnavailableModule

try:
    spec = importlib.util.spec_from_file_location('shipping', '/candidate/shipping.py')
    module = importlib.util.module_from_spec(spec)
    sys.modules['shipping'] = module
    spec.loader.exec_module(module)
except BaseException as error:
    module = UnavailableModule(error)


def prices(zone, pairs, express=False):
    for grams, want in pairs:
        got = module.quote(zone, grams, express)
        assert type(got) is int and got == want, (zone, grams, express, got, want)


def invalid(calls):
    for args in calls:
        try:
            module.quote(*args)
        except ValueError:
            continue
        raise AssertionError(('expected ValueError', args))


def labels():
    for text, want in [('ab-12', 'TRACK:AB-12'), ('x y', 'TRACK:X Y'), ('0', 'TRACK:0')]:
        assert module.tracking_label(text) == want
    for bad in ['', None, True, 'é', 123]:
        try:
            module.tracking_label(bad)
        except ValueError:
            continue
        raise AssertionError(('expected ValueError', bad))


def unchanged(name, digest):
    assert hashlib.sha256(Path('/candidate', name).read_bytes()).hexdigest() == digest, 'Protected file changed'


CASES = [
    ('island_base', lambda: prices('island', [(1,750),(499,750),(500,750)])),
    ('island_rounding', lambda: prices('island', [(501,875),(749,875),(750,875),(751,1000),(1000,1000),(1001,1125)])),
    ('island_limit', lambda: prices('island', [(4999,3000),(5000,3000)])),
    ('island_outside_range', lambda: invalid([('island',0),('island',-1),('island',5001),('island',10000)])),
    ('island_no_express', lambda: invalid([('island',1,True),('island',5000,True)])),
    ('strict_types', lambda: invalid([('island',True),('island',500.0),('island','500'),('island',1,1),('island',1,None),(None,1),('ISLAND',1)])),
    ('legacy_prices', lambda: [prices('local',[(1,400),(1000,400),(1001,475),(1500,475),(1501,550),(20000,3250)]),prices('remote',[(1,900),(1000,900),(1001,1050),(1501,1200),(20000,6600)])]),
    ('legacy_express', lambda: [prices('local',[(1,1000),(1501,1150)],True),prices('remote',[(1001,1650),(20000,7200)],True)]),
    ('legacy_validation', lambda: invalid([('local',0),('remote',20001),('local',True),('remote',1,'yes'),('unknown',1)])),
    ('tracking_unchanged', labels),
    ('TASK.md_unchanged', lambda: unchanged('TASK.md', 'fadf5e72cb4c9a1f37d86f65d858652b86ff1376b9779bd2fddb5f935803e930')),
    ('test_legacy.py_unchanged', lambda: unchanged('test_legacy.py', 'bc3ffe2ca88329c2e0ac652a33a970b4b65d1d0cc885d4b7df08751e7a71796c')),
]
results=[]
for name, call in CASES:
    try:
        call()
        results.append(dict(case=name,passed=True))
    except BaseException as error:
        results.append(dict(case=name,passed=False,error=repr(error)[:2000]))
results.append(dict(case='readme',passed=Path('/candidate/README.md').is_file()))
emit_report(dict(passed=sum(r['passed'] for r in results),total=len(results),results=results))
raise SystemExit(0 if all(r['passed'] for r in results) else 1)
