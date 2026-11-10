import json
import zlib


def recover(data):
    if type(data) is not bytes:
        raise TypeError('data must be bytes')
    state, active, seen, committed = {}, {}, set(), []
    offset = 0
    while len(data) - offset >= 8:
        if data[offset:offset + 4] != b'PTJ1':
            break
        size = int.from_bytes(data[offset + 4:offset + 8], 'big')
        if size > 16384 or len(data) - offset < size + 12:
            break
        payload = data[offset + 8:offset + 8 + size]
        checksum = int.from_bytes(data[offset + 8 + size:offset + 12 + size], 'big')
        if (zlib.crc32(payload) & 0xffffffff) != checksum:
            break
        try:
            record = json.loads(payload.decode('utf-8'))
        except (UnicodeError, ValueError):
            break
        if type(record) is not list or len(record) < 2:
            break
        op, tx = record[:2]
        sizes = {'begin': 2, 'put': 4, 'del': 3, 'commit': 2, 'abort': 2}
        if type(op) is not str or op not in sizes or len(record) != sizes[op]:
            break
        if type(tx) is not int or not 0 <= tx < 2**31:
            break
        if any(type(value) is not str for value in record[2:]):
            break
        if op == 'begin':
            if tx in seen:
                break
            seen.add(tx)
            active[tx] = []
        else:
            if tx not in active:
                break
            if op in ('put', 'del'):
                active[tx].append(record)
            elif op == 'commit':
                for change in active.pop(tx):
                    if change[0] == 'put':
                        state[change[2]] = change[3]
                    else:
                        state.pop(change[2], None)
                committed.append(tx)
            else:
                active.pop(tx)
        offset += size + 12
    return {'state': state, 'committed': committed, 'valid_bytes': offset}
