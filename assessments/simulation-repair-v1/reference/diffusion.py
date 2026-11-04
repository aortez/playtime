"""Known-correct fixture for validating the independent checker; never a seed."""


class Simulation:
    def __init__(self, initial, rate=0.25):
        self.cells = list(initial)
        self.rate = rate

    def values(self):
        return self.cells[:]

    def advance(self, steps):
        history = [self.values()]
        for _ in range(steps):
            previous = self.cells
            self.cells = [
                value + self.rate * (
                    previous[i - 1] - 2 * value
                    + previous[(i + 1) % len(previous)]
                )
                for i, value in enumerate(previous)
            ]
            history.append(self.values())
        return history
