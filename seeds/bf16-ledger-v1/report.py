"""Starter implementation. It contains correctness and error-handling bugs."""
import csv
import json
import sys


def summarize(path):
    totals = {}
    accepted = 0
    with open(path) as source:
        for row in csv.DictReader(source):
            if row['status'] == 'void':
                continue
            cents = int(float(row['amount']) * 100)
            customer = row['customer']
            totals[customer] = totals.get(customer, 0) + cents
            accepted += 1
    return {
        'accepted_rows': accepted,
        'voided_rows': 0,
        'duplicate_rows': 0,
        'rejected_rows': 0,
        'total_cents': sum(totals.values()),
        'customers': [dict(customer=k, total_cents=v) for k, v in totals.items()],
    }


if __name__ == '__main__':
    result = summarize(sys.argv[1])
    with open(sys.argv[2], 'w') as destination:
        json.dump(result, destination)
