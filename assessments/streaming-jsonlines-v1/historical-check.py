"""Private parser checks, including pre-allocation of hostile input before tracing."""
import importlib.util
import json
from pathlib import Path
import tracemalloc


def raises(error, function):
    try:
        function()
    except error:
        return
    raise AssertionError('expected ' + error.__name__)


def closed(parser):
    raises(RuntimeError, lambda: parser.feed(b''))
    raises(RuntimeError, parser.finish)


def split_records():
    raw = '{"name":"雪🦜"}\r\n\n\t \r\n[1,true,null]\n42\n"end"'.encode()
    expected = [{'name': '雪🦜'}, [1, True, None], 42, 'end']
    for split in range(len(raw) + 1):
        p = Parser(100)
        actual = p.feed(raw[:split]) + p.feed(raw[split:]) + p.finish()
        assert actual == expected, (split, actual)
        closed(p)
    p = Parser(100)
    actual = []
    for byte in raw:
        actual.extend(p.feed(bytes([byte])))
    assert actual + p.finish() == expected


def boundaries():
    for raw, limit in [(b'123\n', 3), (b'12\r\n', 3), ('"雪"\n'.encode(), 5)]:
        p = Parser(limit)
        assert p.feed(raw) == [json.loads(raw)]
        assert p.finish() == []
    p = Parser(3)
    assert p.feed(b'12') == []
    assert p.feed(b'3') == []
    raises(ValueError, lambda: p.feed(b'4'))
    closed(p)


def oversized(raw, limit=3):
    p = Parser(limit)
    raises(ValueError, lambda: p.feed(raw))
    closed(p)


def malformed(raw, final=False):
    p = Parser(100)
    if final:
        p.feed(raw)
        raises(ValueError, p.finish)
    else:
        raises(ValueError, lambda: p.feed(raw + b'\n'))
    closed(p)


def blanks_empty():
    p = Parser(10)
    assert p.feed(b'\n\r\n \t\r\n') == []
    assert p.feed(b'') == []
    assert p.finish() == []
    closed(p)
    p = Parser(10)
    assert p.feed(b' \t') == []
    assert p.finish() == []


def null_and_types():
    p = Parser(100)
    got = p.feed(b'null\nfalse\n0\n1.5\n"x"\n[]\n{}\n')
    assert got == [None, False, 0, 1.5, 'x', [], {}]
    assert [type(v) for v in got] == [type(None), bool, int, float, str, list, dict]


def limits():
    for limit in [0, -1, True, False, 1.5, '3', None]:
        raises(ValueError, lambda: Parser(limit))


def memory_bound():
    raw = b'x' * (16 * 1024 * 1024)
    p = Parser(32)
    tracemalloc.start()
    try:
        raises(ValueError, lambda: p.feed(raw))
        peak = tracemalloc.get_traced_memory()[1]
    finally:
        tracemalloc.stop()
    assert peak < 1024 * 1024, ('extra allocation bytes', peak)
    closed(p)


CASES = [('all_byte_splits', split_records), ('exact_limits', boundaries),
         ('cr_counts_toward_limit', lambda: oversized(b'123\r\n')),
         ('blank_lines_count', lambda: oversized(b'    \n')),
         ('unterminated_limit', lambda: oversized(b'1234')),
         ('many_short_records', lambda: many_records()),
         ('blank_empty_finish', blanks_empty), ('null_and_json_types', null_and_types),
         ('invalid_limits', limits), ('bounded_allocation', memory_bound)]
for name, raw in [('bad_utf8', b'"\xff"'), ('bad_json', b'{x}'),
                  ('trailing_garbage', b'1 2'), ('nan', b'NaN'),
                  ('infinity', b'Infinity'), ('negative_infinity', b'-Infinity'),
                  ('invalid_escape', b'"\\x"')]:
    CASES.append((name, lambda raw=raw: malformed(raw)))
CASES += [('incomplete_utf8_finish', lambda: malformed(b'"\xe9', True)),
          ('incomplete_json_finish', lambda: malformed(b'{', True)),
          ('valid_final_record', lambda: final_record())]


def many_records():
    p = Parser(1)
    assert p.feed(b'0\n' * 10000) == [0] * 10000
    assert p.finish() == []


def final_record():
    p = Parser(10)
    assert p.feed(b'123') == []
    assert p.finish() == [123]
    closed(p)


try:
    spec = importlib.util.spec_from_file_location('candidate', '/candidate/jsonlines.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    Parser = module.JSONLinesParser
    import_error = None
except Exception as exc:
    import_error = repr(exc)
results = []
for name, test in CASES:
    try:
        assert import_error is None, import_error
        test()
        results.append(dict(case=name, passed=True))
    except BaseException as exc:
        results.append(dict(case=name, passed=False, error=repr(exc)[:2000]))
results.append(dict(case='readme', passed=Path('/candidate/README.md').is_file()))
print(json.dumps(dict(passed=sum(x['passed'] for x in results), total=len(results), results=results)))
