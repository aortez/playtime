# Repair byte-range normalization
Repair ranges.py using only the Python standard library. This is a pure utility with the following explicit application policy; implement exactly this policy rather than adding different HTTP behavior.

resolve_ranges(header, size) returns a dictionary with exactly `status` (integer) and `ranges` (list of [start,end] integer pairs, inclusive). size must be a nonnegative int, excluding bool. header must be None or a string. Invalid argument types/size raise ValueError with a nonempty message.

No header (None or empty after stripping ASCII spaces/tabs), an unknown unit, or ANY malformed header yields status 200 and the entire representation: [[0,size-1]], or [] when size is zero. A bytes header uses a case-insensitive unit with the exact separator `=` and no spaces around it, e.g. `bytes=0-2, 7-`. Strip ASCII spaces/tabs around the whole header and around each comma-separated member only. Internal whitespace within a member is malformed. Permit 1 through 32 members; more members makes the whole header malformed, even when they would merge.

Valid member forms use only ASCII digits: START-END, START-, and -SUFFIX_LENGTH. At least one side of the single hyphen must be nonempty. Plus signs, non-ASCII digits, multiple hyphens, empty members, and START>END are malformed; if any member is malformed ignore the entire header (status 200), even if other members are satisfiable. Leading zeros are allowed. Large integers must be handled exactly.

For valid syntax, clip explicit/open ends to size-1; START>=size is unsatisfiable. A suffix selects the last min(SUFFIX_LENGTH,size) bytes; suffix length zero is unsatisfiable. All members are unsatisfiable for size zero. Skip unsatisfiable members when other members are satisfiable. If none remain return {"status":416,"ranges":[]}. Otherwise sort by start and merge overlapping OR adjacent ranges, returning status 206, including when the ranges happen to cover the full representation.

Keep the CLI `python3 ranges.py SIZE HEADER`: print JSON to stdout and exit 0 for all valid arguments (including a returned 416). SIZE must consist of one or more ASCII digits. Wrong argument count or invalid SIZE must print a useful stderr message and exit 2. The CLI does not perform network or file reads.

Produce example.json containing the result for size=10 and header `bytes=-2,0-1`. Write README.md explaining usage, the checks you actually ran, and remaining limitations. Run additional checks of your own.
