"""An elementary cellular automaton. All state is explicit and deterministic."""
import argparse


def evolve(cells, rule):
    width = len(cells)
    return [
        (rule >> ((cells[(i - 1) % width] << 2) | (cells[i] << 1) | cells[(i + 1) % width])) & 1
        for i in range(width)
    ]


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rule", type=int, default=90, choices=range(256), metavar="0..255")
    parser.add_argument("--width", type=int, default=61)
    parser.add_argument("--steps", type=int, default=30)
    args = parser.parse_args()
    if args.width < 1 or args.steps < 1:
        parser.error("width and steps must be positive")
    cells = [0] * args.width
    cells[args.width // 2] = 1
    for _ in range(args.steps):
        print("".join("#" if cell else "." for cell in cells))
        cells = evolve(cells, args.rule)
