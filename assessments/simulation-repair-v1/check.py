"""Independent behavioral checks. Execute only in an isolated workshop container.

Usage: python -I /assessment/check.py /candidate/diffusion.py
JSON on stdout is checker evidence, separate from model self-assessment.
This is a correctness check, not an adversarial-code grading security boundary.
"""
import contextlib
import importlib.util
import io
import json
import math
import os
import sys


def close(actual, expected):
    assert len(actual) == len(expected), (actual, expected)
    assert all(math.isclose(a, b, rel_tol=1e-10, abs_tol=1e-10)
               for a, b in zip(actual, expected)), (actual, expected)


def step(values, rate):
    # Pairwise flux form, independent of the starter's pointwise update.
    result = values[:]
    for i in range(len(values)):
        j = (i + 1) % len(values)
        flux = rate * (values[j] - values[i])
        result[i] += flux
        result[j] -= flux
    return result


def initial_conditions(Simulation):
    initial = [3.0, -2.0, 7.0, 0.5]
    sim = Simulation(initial)
    close(sim.values(), initial)
    initial[0] = 999
    close(sim.values(), [3.0, -2.0, 7.0, 0.5])
    exposed = sim.values()
    exposed[1] = 999
    close(sim.values(), [3.0, -2.0, 7.0, 0.5])


def update_rule(Simulation):
    for initial in ([3.0], [1.0, 4.0], [0.0, 1.0, 0.0, 2.0, -1.0]):
        for rate in (0.0, 0.125, 0.25, 0.5):
            sim = Simulation(initial, rate)
            sim.advance(1)
            close(sim.values(), step(list(initial), rate))


def step_count(Simulation):
    for count in (0, 1, 2, 7):
        initial = [2.0, -1.0, 3.5, 0.0]
        expected = initial[:]
        history = [expected]
        for _ in range(count):
            expected = step(expected, 0.125)
            history.append(expected)
        sim = Simulation(initial, 0.125)
        actual = sim.advance(count)
        assert len(actual) == count + 1, f"{count} updates returned {len(actual)} states"
        for a, e in zip(actual, history):
            close(a, e)
        close(sim.values(), expected)


def independent_snapshots(Simulation):
    for count in (0, 1, 4):
        sim = Simulation([1.0, 0.0, 2.0, 0.0])
        states = sim.advance(count)
        assert len(states) == count + 1, "expected entry and each update"
        saved = [list(s) for s in states]
        current = list(sim.values())
        states[0][0] = 10000
        for state, expected in zip(states[1:], saved[1:]):
            close(state, expected)
        close(sim.values(), current)
        states[-1][1] = 10000
        close(sim.values(), current)
        sim.advance(1)
        for state, expected in zip(states[1:-1], saved[1:-1]):
            close(state, expected)


def continuation(Simulation):
    for a, b in ((0, 3), (1, 2), (4, 5), (3, 0)):
        initial = [0.5, 3.0, -1.0, 2.0, 7.0]
        sim = Simulation(initial, 0.125)
        sim.advance(a)
        before = list(sim.values())
        resumed = sim.advance(b)
        close(resumed[0], before)
        fresh = Simulation(initial, 0.125)
        fresh.advance(a + b)
        close(sim.values(), fresh.values())


def main():
    # -I excludes host/user import paths. Explicitly permit the read-only
    # candidate directory: factoring a solution into local modules is valid.
    candidate = os.path.abspath(sys.argv[1])
    sys.path.insert(0, os.path.dirname(candidate))
    cases = dict(zip(("R1", "R2", "R3", "R4", "R5"), (
        initial_conditions, update_rule, step_count,
        independent_snapshots, continuation,
    )))
    results = []
    # Each requirement loads a fresh module. Do not let candidate prints become
    # the checker's structured report. Container limits bound all candidate work.
    for requirement, check in cases.items():
        try:
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                spec = importlib.util.spec_from_file_location("diffusion", candidate)
                module = importlib.util.module_from_spec(spec)
                sys.modules["diffusion"] = module
                spec.loader.exec_module(module)
                check(module.Simulation)
            results.append({"id": requirement, "verdict": "passed"})
        except BaseException as error:
            results.append({"id": requirement, "verdict": "failed",
                            "detail": f"{type(error).__name__}: {error}"[:2000]})
    print(json.dumps({"schema_version": 1, "requirements": results}, indent=2))
    return 0 if all(r["verdict"] == "passed" for r in results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
