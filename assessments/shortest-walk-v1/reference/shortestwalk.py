def shortest_walk(n, edges, source, target):
    distance = [None] * n
    distance[source] = 0
    for _ in range(n - 1):
        changed = False
        for u, v, weight in edges:
            if distance[u] is not None:
                cost = distance[u] + weight
                if distance[v] is None or cost < distance[v]:
                    distance[v] = cost
                    changed = True
        if not changed:
            break
    affected = set()
    adjacency = [[] for _ in range(n)]
    for u, v, weight in edges:
        adjacency[u].append(v)
        if distance[u] is not None and (distance[v] is None or distance[u] + weight < distance[v]):
            affected.add(v)
    queue = list(affected)
    for u in queue:
        for v in adjacency[u]:
            if v not in affected:
                affected.add(v)
                queue.append(v)
    return float('-inf') if target in affected else distance[target]
