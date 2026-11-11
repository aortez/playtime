def linearize(history, initial=None):
    def string(value):
        return type(value) is str

    def mapping(value, optional=False):
        return type(value) is dict and all(string(k) and (string(v) or (optional and v is None))
                                           for k, v in value.items())

    if initial is None:
        initial = {}
    if not mapping(initial) or type(history) is not list or len(history) > 18:
        raise ValueError('invalid input')
    ids = set()
    for entry in history:
        if type(entry) is not dict or not {'id', 'start', 'end', 'op', 'result'} <= entry.keys():
            raise ValueError('invalid record')
        name, start, end, op, result = (entry[k] for k in ('id', 'start', 'end', 'op', 'result'))
        if not string(name) or not name or name in ids:
            raise ValueError('invalid ID')
        ids.add(name)
        if type(start) is not int or type(end) is not int or start >= end:
            raise ValueError('invalid interval')
        if type(op) is not list or not op or not string(op[0]):
            raise ValueError('invalid operation')
        if op[0] == 'get':
            valid = len(op) == 2 and string(op[1]) and (result is None or string(result))
        elif op[0] == 'put':
            valid = len(op) == 3 and string(op[1]) and string(op[2]) and result is None
        elif op[0] == 'cas':
            valid = len(op) == 3 and mapping(op[1], True) and mapping(op[2], True) and type(result) is bool
        else:
            valid = False
        if not valid:
            raise ValueError('invalid operation/result')
    entries = sorted(history, key=lambda e: e['id'])
    predecessors = [sum(1 << j for j, other in enumerate(entries) if other['end'] <= entry['start'])
                    for entry in entries]
    complete = (1 << len(entries)) - 1
    impossible = set()

    def search(done, state):
        if done == complete:
            return []
        key = done, tuple(sorted(state.items()))
        if key in impossible:
            return None
        for i, entry in enumerate(entries):
            if done & (1 << i) or predecessors[i] & ~done:
                continue
            op, result = entry['op'], entry['result']
            next_state = state.copy()
            if op[0] == 'get':
                if state.get(op[1]) != result:
                    continue
            elif op[0] == 'put':
                next_state[op[1]] = op[2]
            else:
                matches = all(state.get(k) == v for k, v in op[1].items())
                if matches != result:
                    continue
                if matches:
                    for k, v in op[2].items():
                        if v is None:
                            next_state.pop(k, None)
                        else:
                            next_state[k] = v
            suffix = search(done | (1 << i), next_state)
            if suffix is not None:
                return [entry['id']] + suffix
        impossible.add(key)
        return None

    return search(0, initial.copy())
