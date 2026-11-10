"""Known frame boundaries plus commit-log replay, independent of the decoder."""
import importlib.util
import json
from pathlib import Path
import random
import sys
import zlib
from assessment_support import emit_report, UnavailableModule

try:
    spec = importlib.util.spec_from_file_location('journal', '/candidate/journal.py')
    module = importlib.util.module_from_spec(spec)
    sys.modules['journal'] = module
    spec.loader.exec_module(module)
except BaseException as error:
    module = UnavailableModule(error)


def raw(payload):
    return b'PTJ1' + len(payload).to_bytes(4, 'big') + payload + (zlib.crc32(payload) & 0xffffffff).to_bytes(4, 'big')


def encode(record):
    return raw(json.dumps(record, ensure_ascii=False, separators=(',', ':')).encode())


def expected(records):
    # Replay only transactions with a commit in the accepted record sequence.
    state, commits = {}, []
    for position, event in enumerate(records):
        if event[0] != 'commit':
            continue
        commits.append(event[1])
        for change in records[:position]:
            if change[1] != event[1]:
                continue
            if change[0] == 'put':
                state.update({change[2]: change[3]})
            elif change[0] == 'del' and change[2] in state:
                del state[change[2]]
    return {'state': state, 'committed': commits,
            'valid_bytes': sum(len(encode(r)) for r in records)}


def check(records, tail=b''):
    blob = b''.join(map(encode, records)) + tail
    got = module.recover(blob)
    assert type(got) is dict and got == expected(records), (got, expected(records))
    assert type(got['valid_bytes']) is int and type(got['committed']) is list and type(got['state']) is dict


BASE = [['begin', 7], ['put', 7, 'x', 'old'], ['commit', 7]]
INTERLEAVED = [['begin', 1], ['put', 1, 'x', 'A'], ['begin', 2],
               ['put', 2, 'x', 'B'], ['commit', 2], ['commit', 1]]


def torn():
    records = BASE + [['begin', 8], ['put', 8, 'é', '😀'], ['commit', 8], ['begin', 9]]
    frames = list(map(encode, records))
    blob = b''.join(frames)
    for stop in range(len(blob) + 1):
        count, size = 0, 0
        for frame in frames:
            if size + len(frame) > stop:
                break
            size += len(frame)
            count += 1
        assert module.recover(blob[:stop]) == expected(records[:count]), stop


def checksum():
    bad = bytearray(encode(['begin', 8]))
    bad[-1] ^= 1
    check(BASE, bytes(bad) + b''.join(map(encode, [['begin', 9], ['put', 9, 'x', 'bad'], ['commit', 9]])))


def malformed():
    for payload in (b'\xff', b'{', b'{}', b'null', b'[]', b'["begin",true]',
                    b'["begin",1.0]', b'["begin",-1]', b'["begin",2147483648]',
                    b'["begin",0,0]', b'[{},0]', b'["unknown",0]', b'["begin",NaN]'):
        check(BASE, raw(payload) + encode(['begin', 100]))


def lifecycle():
    for op in (['commit', 100], ['abort', 100], ['put', 100, 'k', 'v'], ['del', 100, 'k']):
        check(BASE, encode(op) + encode(['begin', 100]))
    check([['begin', 1]], encode(['begin', 1]))


def reuse():
    for close in ('commit', 'abort'):
        check([['begin', 1], [close, 1]], encode(['begin', 1]) + encode(['commit', 1]))


def bad_operation():
    prefix = BASE + [['begin', 8], ['put', 8, 'x', 'pending']]
    for op in (['put', 8, 'x'], ['put', 8, 'x', 3], ['del', 8, None], ['commit', 8, 'extra']):
        check(prefix, encode(op) + encode(['commit', 8]))


def payload_limit():
    prefix = [['begin', 0]]
    fixed = len(json.dumps(['put', 0, 'x', ''], separators=(',', ':')).encode())
    event = ['put', 0, 'x', 'a' * (16384 - fixed)]
    assert len(encode(event)) == 16396
    check(prefix + [event, ['commit', 0]])
    event[-1] += 'b'
    check(prefix, encode(event) + encode(['commit', 0]))


def generated():
    rng = random.Random(64011)
    for trial in range(60):
        records, live = [], set()
        for tx in range(6):
            records.append(['begin', tx])
            live.add(tx)
            for _ in range(4):
                chosen = rng.choice(sorted(live))
                key = rng.choice(['x', 'y', '', 'é'])
                records.append(['del', chosen, key] if rng.randrange(3) == 0 else
                               ['put', chosen, key, str(rng.randrange(100))])
            if len(live) > 1:
                chosen = rng.choice(sorted(live))
                records.append([rng.choice(['commit', 'abort']), chosen])
                live.remove(chosen)
        for tx in sorted(live):
            records.append([rng.choice(['commit', 'abort']), tx])
        check(records)
        count = rng.randrange(len(records))
        suffix = encode(records[count])
        check(records[:count], suffix[:rng.randrange(len(suffix))])


def input_type():
    for value in (None, '', bytearray(), memoryview(b''), []):
        try:
            module.recover(value)
        except TypeError:
            continue
        raise AssertionError('expected TypeError')


CASES = [
    ('empty', lambda: check([])),
    ('pending_not_visible', lambda: check(BASE + [['begin', 8], ['put', 8, 'x', 'pending']])),
    ('abort_discard', lambda: check(BASE + [['begin', 8], ['del', 8, 'x'], ['abort', 8]])),
    ('commit_order', lambda: check(INTERLEAVED)),
    ('within_transaction_order', lambda: check([['begin', 0], ['put', 0, 'x', 'a'], ['del', 0, 'x'], ['put', 0, 'x', 'b'], ['commit', 0]])),
    ('delete_at_commit_time', lambda: check([['begin', 1], ['del', 1, 'x'], ['begin', 2], ['put', 2, 'x', 'B'], ['commit', 2], ['commit', 1]])),
    ('independent_interleaved_abort', lambda: check(INTERLEAVED[:-1] + [['abort', 1]])),
    ('empty_commit', lambda: check([['begin', 0], ['commit', 0]])),
    ('unicode_and_empty', lambda: check([['begin', 2**31-1], ['put', 2**31-1, '', 'é\x00😀'], ['commit', 2**31-1]])),
    ('every_crash_byte', torn),
    ('checksum_no_resync', checksum),
    ('magic_no_resync', lambda: check(BASE, b'junk' + encode(['begin', 8]))),
    ('oversized_length', lambda: check(BASE, b'PTJ1\xff\xff\xff\xff' + encode(['begin', 8]))),
    ('malformed_payload', malformed),
    ('inactive_or_duplicate_begin', lifecycle),
    ('closed_id_reuse', reuse),
    ('invalid_operation_stops_commit', bad_operation),
    ('payload_limit_inclusive', payload_limit),
    ('seeded_interleavings_60', generated),
    ('input_type', input_type),
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
