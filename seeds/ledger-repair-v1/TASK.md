# Ledger repair task, version 1

Repair the supplied Python program and deliver `report.py`, using only the
Python standard library. Its command is:

```
python3 report.py INPUT.csv OUTPUT.json
```

Input is UTF-8 CSV with the exact header `transaction_id,customer,amount,status`.
Ignore completely empty CSV rows. Strip surrounding whitespace from every field.
Process other rows in file order using these rules:

1. A valid row has exactly four fields, nonempty transaction ID and customer,
   status `settled` or `void`, and a valid amount. Status is case sensitive.
2. An amount is an optional `+` or `-`, one or more ASCII digits, optionally a
   decimal point followed by one or two ASCII digits. No exponent, grouping,
   special value, or bare decimal point is allowed. Convert to integer cents
   exactly, including very large values and negative amounts.
3. Validate the entire row before duplicate detection. Count an invalid row as
   rejected; it does not claim its transaction ID.
4. The first valid occurrence of an ID claims it, whether settled or void.
   Every later valid row with that ID counts as a duplicate and has no effect.
5. A first valid settled row contributes its amount to its customer and counts
   as accepted. A first valid void row counts as voided and contributes nothing.
   Keep customers whose settled total is zero; omit customers with only voids.

Write a JSON object with exactly these keys:

- `accepted_rows`, `voided_rows`, `duplicate_rows`, `rejected_rows`: integer counts.
- `total_cents`: integer sum over all accepted rows.
- `customers`: a list sorted by customer name in Python's ordinary string order,
  with one object per customer: `{"customer": NAME, "total_cents": INTEGER}`.

On success exit 0. A missing/unreadable input or incorrect header must produce a
useful stderr message and exit 2, without creating the requested output file.
Inputs used for checking have syntactically valid CSV; row field counts can vary.
Output paths used for checking are writable and do not already exist.

Produce `report.json` from `transactions.csv`. Also write
`README.md` explaining usage, the checks you actually ran, and any remaining
limitations. Verify the repaired program with additional cases of your choice.
Finish with a concise report of what changed and the checks you actually ran.

The supplied example should produce three accepted rows, one void, one duplicate,
one rejection, a total of 901 cents, and customers Ada=601, Bea=300.
