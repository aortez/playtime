"""Run only inside the locked assessment container, never on the host."""
import os
import sys

# Revision 9 starts Python with -P -s rather than -I, because -I would ignore
# PYTHONHASHSEED. Before importing anything else, refuse to grade unless hash
# seed 0 is in effect and every other interpreter variable is the locked image's
# own, so ambient import paths stay excluded as under -I. Interpreter startup has
# already loaded os and sys.
ALLOWED_VARIABLES = {"PYTHONDONTWRITEBYTECODE", "PYTHONHASHSEED", "PYTHONUNBUFFERED",
                     "PYTHON_SHA256", "PYTHON_VERSION"}
if not (sys.flags.hash_randomization == 0 and os.environ.get("PYTHONHASHSEED") == "0"
        and sys.flags.safe_path and sys.flags.no_user_site
        and {name for name in os.environ if name.startswith("PYTHON")} <= ALLOWED_VARIABLES):
    raise SystemExit(2)
import asyncio
import itertools
import zlib
import sqlite3
import decimal
import math
import gc
import hashlib
import random
import signal
import tracemalloc
import contextlib
import copy
import importlib.util
import json
from pathlib import Path
import runpy
import subprocess
import tempfile

# Load trusted support before permitting local candidate helpers. Preload the
# checkers' standard-library dependencies above so a candidate helper cannot
# accidentally shadow a grader dependency. Only the selected checker is mounted,
# never its reference.
sys.path.insert(0, "/assessment")
import assessment_support


def main():
    sys.path.insert(0, "/candidate")
    os.chdir("/candidate")
    sys.dont_write_bytecode = True
    saved_stdout, saved_stderr = os.dup(1), os.dup(2)
    exit_code = 0
    try:
        with open(os.devnull, "w") as quiet:
            os.dup2(quiet.fileno(), 1)
            os.dup2(quiet.fileno(), 2)
            with contextlib.redirect_stdout(quiet), contextlib.redirect_stderr(quiet):
                try:
                    runpy.run_path("/assessment/check.py", run_name="__main__")
                except SystemExit as error:
                    exit_code = error.code
                except BaseException:
                    exit_code = 2
    finally:
        os.dup2(saved_stdout, 1)
        os.dup2(saved_stderr, 2)
        os.close(saved_stdout)
        os.close(saved_stderr)
    if type(exit_code) is not int or exit_code not in (0, 1):
        return 2
    if len(assessment_support.REPORTS) != 1:
        return 2
    encoded = json.dumps(assessment_support.REPORTS[0], ensure_ascii=True)
    if len(encoded) > 1024 * 1024:
        return 2
    print(encoded)
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
