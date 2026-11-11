"""Container-only SQL checks against an independent Python selection oracle."""
from pathlib import Path
import random
import sqlite3
from assessment_support import emit_report

SCHEMA = '''CREATE TABLE events(resource_id TEXT NOT NULL, version INTEGER NOT NULL,
event_id INTEGER PRIMARY KEY, deleted INTEGER NOT NULL, payload TEXT NOT NULL)'''
try:
    query = Path('/candidate/query.sql').read_text()
    if len(query) > 65536:
        raise ValueError('query exceeds 64 KiB')
    query_error = None
except BaseException as error:
    query_error = repr(error)


def check(rows, cutoff):
    if query_error:
        raise AssertionError(query_error)
    winners = {}
    for row in rows:
        name, version, event_id, deleted, payload = row
        if version <= cutoff and (name not in winners or
                                  (version, event_id) > winners[name][1:3]):
            winners[name] = row
    expected = [(name, row[1], row[2], row[4]) for name, row in
                sorted(winners.items()) if not row[3]]
    connection = sqlite3.connect(':memory:')
    try:
        connection.execute(SCHEMA)
        connection.executemany('INSERT INTO events VALUES (?,?,?,?,?)', rows)
        allowed = {sqlite3.SQLITE_SELECT, sqlite3.SQLITE_READ,
                   sqlite3.SQLITE_FUNCTION, sqlite3.SQLITE_RECURSIVE}
        connection.set_authorizer(lambda action, *args: sqlite3.SQLITE_OK
                                  if action in allowed else sqlite3.SQLITE_DENY)
        ticks = 0
        def progress():
            nonlocal ticks
            ticks += 1
            return ticks > 2000
        connection.set_progress_handler(progress, 100)
        cursor = connection.execute(query, {'cutoff': cutoff})
        actual = cursor.fetchmany(len(expected) + 1)
        assert [column[0] for column in cursor.description] == [
            'resource_id', 'version', 'event_id', 'payload'], 'column names/order'
        assert actual == expected, (actual, expected)
    finally:
        connection.close()


FIXED = [
    ('empty', [], 0),
    ('single_live', [('a', 1, 1, 0, 'one')], 1),
    ('cutoff_inclusive', [('a', 1, 1, 0, 'old'), ('a', 2, 2, 0, 'new')], 2),
    ('future_ignored', [('a', 1, 1, 0, 'old'), ('a', 3, 2, 0, 'future')], 2),
    ('future_only', [('a', 3, 1, 0, 'future')], 2),
    ('tombstone_wins', [('a', 1, 1, 0, 'old'), ('a', 2, 2, 1, 'deleted')], 2),
    ('resurrection', [('a', 1, 1, 1, 'deleted'), ('a', 2, 2, 0, 'back')], 2),
    ('equal_version_live_wins', [('a', 2, 3, 0, 'new'), ('a', 2, 2, 1, 'old')], 2),
    ('equal_version_delete_wins', [('a', 2, 2, 0, 'old'), ('a', 2, 3, 1, 'gone')], 2),
    ('version_before_id', [('a', 1, 100, 0, 'old'), ('a', 2, 3, 0, 'new')], 2),
    ('resources_independent', [('b', 9, 1, 1, 'gone'), ('a', 2, 2, 0, 'a'),
                               ('b', 1, 3, 0, 'old'), ('c', 8, 4, 0, 'c')], 9),
    ('negative_versions', [('a', -9, 1, 0, 'old'), ('a', -2, 2, 0, 'new')], -3),
    ('exact_i64', [('a', 2**53, 2**53, 0, 'old'),
                   ('a', 2**53 + 1, 2**53 + 1, 0, 'new')], 2**53 + 1),
    ('binary_order_and_payload', [(name, 0, i, 0, "'\n\x00" + name)
                                 for i, name in enumerate(['é', 'a', 'A', '', '😀', 'z'])], 0),
]


def generated():
    randomizer = random.Random(2407)
    for trial in range(80):
        rows = [(randomizer.choice(['a', 'B', '', 'é']), randomizer.randrange(-4, 9),
                 i, randomizer.randrange(2), f'payload-{trial}-{i}') for i in range(30)]
        randomizer.shuffle(rows)
        check(rows, randomizer.randrange(-5, 10))


results = []
for name, call in [(name, lambda rows=rows, cutoff=cutoff: check(rows, cutoff))
                   for name, rows, cutoff in FIXED] + [('seeded_histories_80', generated)]:
    try:
        call()
        results.append(dict(case=name, passed=True))
    except BaseException as error:
        results.append(dict(case=name, passed=False, error=repr(error)[:2000]))
results.append(dict(case='readme', passed=Path('/candidate/README.md').is_file()))
emit_report(dict(passed=sum(row['passed'] for row in results), total=len(results), results=results))
raise SystemExit(0 if all(row['passed'] for row in results) else 1)
