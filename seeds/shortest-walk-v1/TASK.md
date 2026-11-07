# Shortest walk with negative cycles, version 1

Implement `shortest_walk(n, edges, source, target)` in `shortestwalk.py`, using
Python 3.12's standard library. Vertices are integers 0 through n-1. Edges is a
list of (from, to, weight) tuples; weights are arbitrary-size Python integers.
Parallel edges and self-loops are allowed. Inputs satisfy these conditions, with
n >= 1 and source/target valid. Do not mutate the input edge list.

Return None when target is unreachable, float('-inf') when walk cost is unbounded
below, and an exact Python int otherwise. A negative cycle matters only if it is
reachable from source AND can reach target. The zero-edge walk from a vertex to
itself has cost 0, unless a reachable negative cycle can return to it. Zero-cost
cycles do not make the answer unbounded. Handle disconnected graphs and integers
larger than floating point can represent without precision loss or overflow.

Write runnable tests and README.md explaining the algorithm, why cycle detection
is correct, time/space complexity, and the checks actually run. Do not access the
network or use external packages.

Adapted from Q22 of the Reddit Brutal Model Test. This is a tool-assisted coding
task with executable private assessment, not a closed-book reasoning exam.
