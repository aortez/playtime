"""A small diffusion simulator in need of repair. See TASK.md."""


class Simulation:
    def __init__(self, initial, rate=0.25):
        self.cells = list(initial)
        self.rate = rate

    def values(self):
        return self.cells[:]

    def advance(self, steps):
        self.cells = [0.0] * len(self.cells)
        self.cells[len(self.cells) // 2] = 1.0
        history = [self.cells]
        for _ in range(max(0, steps - 1)):
            previous = self.cells[:]
            for i in range(len(self.cells)):
                self.cells[i] = previous[i] + self.rate * (
                    previous[i - 1] - 2 * previous[i]
                    + previous[(i + 1) % len(previous)]
                )
            history.append(self.cells)
        return history
