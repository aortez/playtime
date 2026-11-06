# Repair a deterministic dependency planner
Repair planner.py using only the Python standard library. Implement plan(tasks), returning a list of execution waves, each a sorted list of task IDs. A wave contains ALL tasks ready before that wave begins; completing a task must not allow a dependent into the same wave. An empty task list yields []. Return waves in dependency order. Within waves use Python's ordinary Unicode string ordering, independent of input order.

Input must be a list of dictionaries. Every dictionary must have id (a nonempty string with no leading/trailing whitespace) and deps (a list of IDs obeying the same string rule). Extra dictionary fields are ignored. Duplicate dependency entries count only once. Duplicate task IDs, unknown dependency IDs, invalid structures/types, and invalid IDs must raise ValueError with a nonempty message. Validate the ENTIRE input before trying to schedule it. Never mutate caller input.

For a cycle (including self-dependency), raise ValueError with exactly `cycle: ` followed by the sorted IDs of ALL unscheduled tasks separated by `, `. This includes tasks downstream of a cycle, but excludes tasks that can finish independently. Support a chain of 1,500 tasks without a recursion-depth error.

Preserve the command `python3 planner.py INPUT.json OUTPUT.json`. On valid input, write the plan as UTF-8 JSON and exit 0. Missing/unreadable input, malformed JSON, validation errors, and cycles must produce a useful stderr message and exit 2 without creating the requested output file. Checking uses writable, initially absent output paths.

Generate plan.json from tasks.json. Write README.md with usage, the checks you actually ran, and remaining limitations. Run your own additional checks. Do not use external libraries or network access.
