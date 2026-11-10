# Implement the approved island shipping change

Update `shipping.py` using the three approved R17 records in the supplied
archive context. Only records marked APPROVED R17 are authoritative; routine
records and rejected proposals do not change the contract. The archive is
provided in the conversation, not in a workspace file.

Preserve existing local/remote pricing, validation, express behavior, and
`tracking_label`. All amounts are integer cents. Add useful tests in a new
file and a README describing the implemented rules and checks actually run.
Do not edit `test_legacy.py` or `TASK.md`. Use only Python's standard library.

The public tests cover existing behavior. The three approved records provide
the additional behavior; no external knowledge or network access is required.
