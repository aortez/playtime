# HTTP contract fixtures

These are synthetic #35 transport fixtures, not engine execution or archived
experiments. No real model output, local credentials or private paths are included.

- `openapi.json` fixes the new HTTP v1 schema independently of the foundation
  schema. Shared error responses use references; every reference is checked.
- `http-contract.json` fixes a launch request/acceptance, empty paired anchors,
  one fixed-frontier event page, encoded/decoded cursor, completed RunView, and
  checkpoint/artifact projection. The response retains 100 unknown reserved
  tokens rather than reporting zero usage. Missing retained bytes have a reason.
- `pixel.png` is a synthetic one-pixel PNG for exact download verification.

The fixture manifest and event-prefix hashes are calculated from deterministic
script bytes. They do not claim a production writer's acknowledgment or an
executed/frozen worker identity. The four scripted records retain an unknown
payload, and content projections include frozen and checkpoint-published bytes.

Candidate generators, for review only:

```sh
cargo run --locked -p playtime-server --example openapi
cargo run --locked -p playtime-server --example contract_fixture
```

Do not automatically replace these files after a failed test. Inspect wire,
schema, ordering and byte/digest differences, and keep old foundation fixtures
unchanged unless a separately reviewed compatibility change requires otherwise.
