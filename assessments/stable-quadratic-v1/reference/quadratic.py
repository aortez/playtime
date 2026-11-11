import math


def roots(a, b, c):
    if a == 0 or not all(math.isfinite(x) for x in (a, b, c)):
        raise ValueError('invalid coefficient')
    discriminant = b * b - 4 * a * c
    if discriminant < 0:
        raise ValueError('no real roots')
    if discriminant == 0:
        root = -b / (2 * a)
        return root, root
    q = -0.5 * (b + math.copysign(math.sqrt(discriminant), b))
    return tuple(sorted((q / a, c / q)))
