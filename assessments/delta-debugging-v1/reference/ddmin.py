def ddmin(items, fails):
    current = list(items)
    if fails([]):
        return []
    granularity = 2
    while len(current) >= 2:
        count = min(granularity, len(current))
        for part in range(count):
            start = part * len(current) // count
            end = (part + 1) * len(current) // count
            candidate = current[:start] + current[end:]
            if fails(list(candidate)):
                current = candidate
                granularity = max(2, count - 1)
                break
        else:
            if count == len(current):
                return current
            granularity = min(len(current), count * 2)
    return current
