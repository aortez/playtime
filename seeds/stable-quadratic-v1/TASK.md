# Stable real quadratic roots, version 1

Implement `roots(a, b, c)` in `quadratic.py`, using Python 3.12's standard library.
Inputs are finite Python floats, with a != 0. Return the two real roots of
`a*x*x + b*x + c = 0` as a two-element tuple, in ascending order. Repeated roots
appear twice. Raise ValueError for a zero leading coefficient, nonfinite input,
or negative discriminant. Do not return complex numbers or NaNs.

Assume b*b, 4*a*c, their difference and the stable intermediate products are
representable without overflow or underflow; the requested roots are finite.
You do not need arbitrary-precision output or support inputs outside that domain.
Avoid cancellation destroying the small root, including `(1.0, -1e16, 1.0)`.
Handle b=0, c=0 and repeated zero roots explicitly. Negative a is allowed.

For each reference root r the numerical tolerance is
`abs(actual-r) <= max(1e-300, 1e-12*abs(r))`. Tests use well-separated roots or
exact repeated roots; no promise of recovering ill-conditioned near-double roots
beyond the information in the supplied floats is required. Do not simply round
both roots to an arbitrary number of decimal places.

Write your own runnable tests and README.md. Explain the cancellation problem,
your stable formulation, and limitations due to floating-point input and rounding.
Use residuals or Vieta's relations in your own checks and describe checks actually run.

Adapted from Q35 of the Reddit Brutal Model Test:
https://www.reddit.com/r/LocalLLM/comments/1xnikyz/please_test_your_models_before_spending_money_on/
The API, errors and tolerance are pinned here. The private checker uses a separate
high-precision decimal oracle; the explanation is not automatically graded.
