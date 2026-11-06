"""Ledger repair program.

Usage:
    python3 report.py INPUT.csv OUTPUT.json

Reads UTF-8 CSV with header `transaction_id,customer,amount,status`, applies
the ledger rules, and writes a JSON summary. Exits 0 on success; exits 2 with
a stderr message (and no output file) if the input is missing/unreadable or the
header is incorrect.
"""
import csv
import json
import re
import sys

EXPECTED_HEADER = ["transaction_id", "customer", "amount", "status"]

# Optional +/-, one or more ASCII digits, and an optional decimal point
# followed by exactly one or two ASCII digits. No exponent, grouping, bare
# decimal point, or special value is allowed.
AMOUNT_RE = re.compile(r"[+-]?[0-9]+(?:\.[0-9]{1,2})?")


class LedgerError(Exception):
    """Raised for unrecoverable input problems (bad header, unreadable file)."""


def parse_amount_cents(value):
    """Return the amount as an integer cent value, or None if invalid."""
    if AMOUNT_RE.fullmatch(value):
        negative = value.startswith("-")
        body = value[1:] if negative else value
        if "." in body:
            whole, frac = body.split(".", 1)
            cents = int(whole) * 100 + int(frac.ljust(2, "0"))
        else:
            cents = int(body) * 100
        return -cents if negative else cents
    return None


def summarize(path):
    """Process the CSV at `path` and return the summary dict."""
    totals = {}
    seen_ids = set()
    accepted = 0
    voided = 0
    duplicates = 0
    rejected = 0

    try:
        source = open(path, newline="", encoding="utf-8")
    except OSError as exc:
        raise LedgerError(f"cannot open input file: {exc}")

    with source:
        reader = csv.reader(source)
        try:
            header = next(reader)
        except StopIteration:
            raise LedgerError("input is empty; missing required header")

        if [field.strip() for field in header] != EXPECTED_HEADER:
            raise LedgerError(
                "incorrect header; expected "
                + ",".join(EXPECTED_HEADER)
            )

        for row in reader:
            # Completely empty rows (blank lines) are ignored.
            if not row:
                continue

            fields = [field.strip() for field in row]

            # A valid row has exactly four fields.
            if len(fields) != 4:
                rejected += 1
                continue

            transaction_id, customer, amount_str, status = fields

            # Validate the whole row before any duplicate detection.
            if not transaction_id or not customer:
                rejected += 1
                continue
            if status not in ("settled", "void"):
                rejected += 1
                continue

            cents = parse_amount_cents(amount_str)
            if cents is None:
                rejected += 1
                continue

            # The row is valid; now handle duplicate claims.
            if transaction_id in seen_ids:
                duplicates += 1
                continue

            seen_ids.add(transaction_id)
            if status == "void":
                voided += 1
            else:  # settled
                totals[customer] = totals.get(customer, 0) + cents
                accepted += 1

    return {
        "accepted_rows": accepted,
        "voided_rows": voided,
        "duplicate_rows": duplicates,
        "rejected_rows": rejected,
        "total_cents": sum(totals.values()),
        "customers": [
            {"customer": name, "total_cents": total}
            for name, total in sorted(totals.items())
        ],
    }


def main(argv):
    if len(argv) != 3:
        print(
            "usage: python3 report.py INPUT.csv OUTPUT.json",
            file=sys.stderr,
        )
        return 2

    input_path, output_path = argv[1], argv[2]

    try:
        result = summarize(input_path)
    except LedgerError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    try:
        with open(output_path, "w", encoding="utf-8") as destination:
            json.dump(result, destination, indent=2)
            destination.write("\n")
    except OSError as exc:
        print(f"error: cannot write output file: {exc}", file=sys.stderr)
        return 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
