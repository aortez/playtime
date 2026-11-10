The decoder accepts one fully validated frame at a time. State changes occur
only when a validated commit applies its transaction's buffered operations.
The accepted offset advances for all valid records, including pending work.
Seen IDs outlive active transactions, preventing reuse. A parse, checksum or
lifecycle failure ends recovery without resynchronizing or undoing old commits.
