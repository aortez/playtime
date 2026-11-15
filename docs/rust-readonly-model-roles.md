# Read-only model invocations

`playtime-task` supports fresh setup, supervisor and reviewer invocations through
`playtime-readonly-v1`. They use the existing shared chat controller, Strata
Responses transport, native project sandbox, evidence writer and cleanup path.
This is an explicitly selected model backend; it does not change a configured
Codex model, worker defaults or daily chat defaults. Devbot integration is separate.

The worker protocol remains `playtime-worker` version 1, with `worker-report-v1`
and completed-worker continuation. Its description gains a `readonly_invocation`
capability object. A read-only invocation is not a worker report and cannot resume
or adopt a worker, chat, previous setup, or review session.

## Invocation and admission

A trusted caller selects `--role setup`, `--role supervisor` or `--role reviewer`,
`--tool-backend native`, and a protected native policy with mode `read_only`.
Edit policies, Docker configuration, worker sessions and missing output contracts
are rejected before model activity. The caller must select the model explicitly.
The [native tools guide](rust-native-project-tools.md) defines account, policy,
helper, cache, toolchain and execution identity admission.

The installed owner wrapper accepts:

```sh
run-strata.py framework-gyro-m-fast --read-only \
  --role reviewer --tool-backend native \
  --native-policy /etc/playtime/native/devbot-read-only.json \
  --workspace /assigned/standalone/checkout \
  --run-dir /private/fresh-run --request /private/inputs/prompt.txt \
  --output-contract /private/inputs/review-schema.json \
  --output-contract-sha256 EXACT_SCHEMA_BYTES_SHA256 \
  --wall-seconds 900
```

The caller creates a private run directory and private owned regular input files.
Use input basenames distinct from the reserved output files below. The request is
nonempty UTF-8, at most 256 KiB. It includes the assignment and bounded evidence
selected by the trusted caller. Paths to private evidence do not grant the model
access: copy only the necessary excerpts into the request. Do not place model
credentials, signing material or authoritative controller state in tool mounts.

As for workers, the caller starts the wrapper with a pipe, durably records its
process-start receipt, then writes the **exact request bytes** and closes stdin.
The wrapper compares the bytes before SSH ownership or model activity. It freezes
their SHA-256 into `--request-sha256` for both input checking and actual execution;
a changed request cannot be substituted during host startup. The caller supplies
the output-contract hash; both stages verify it against the exact schema bytes.
Role, policy and paths are trusted argv, not model arguments. The owner keeps its
SSH agent; the local task child does not inherit `SSH_AUTH_SOCK`.

Direct `playtime-task` calls take the same options without `--read-only`, plus
`--request-sha256 HEX`, `--model-lock LOCK` and `--endpoint URL`. Direct callers
must already hold and verify the endpoint and enforce their own durable process
start gate. `--check-inputs` checks local admission without acquiring inference.
The wrapper owns model/image/endpoint selection, rejects a Docker environment for
native tools, probes the selected capability and checks inputs before acquiring
the inference host. Host verification, cancellation and release are unchanged.
No live host is acquired by the fixture gates for this feature.

Selected capability probe:

```sh
playtime-task --role reviewer --describe-protocol
```

Its JSON includes:

```json
{
  "protocol": "playtime-readonly",
  "version": 1,
  "capability": "playtime-readonly-v1",
  "roles": ["setup", "supervisor", "reviewer"],
  "tools": "project-native-v1",
  "report": "structured-result-v1",
  "output_contract": "json-schema-subset-v1",
  "continuation": "none",
  "transport": "strata-responses-v1",
  "native_policy_schema": 1,
  "native_helper_sha256": "<installed helper digest>"
}
```

The description also carries the native backend capability. Default worker
capability fields and report rules keep their original meanings.

## Output contract profile v1

The caller supplies a JSON Schema document of at most 32 KiB, with an exact-byte
SHA-256. The root must have `type: "object"`. There are at most 512 schema nodes
and 16 nested child-schema levels. Every node has an explicit `type`, either a
single standard JSON type or an array of types (for example `["string","null"]`).

The only accepted keywords are:

- `type`, `properties`, `required`, `additionalProperties`, `items`, `enum`.
- `description`, `title`.
- `minLength`, `maxLength`, `minItems`, `maxItems`.
- `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum`.

Every object schema requires `properties` and `additionalProperties: false`;
every array schema requires one `items` schema. Only `false` is allowed for
`additionalProperties`. Keywords and values must be valid under JSON Schema
2020-12. Unknown keywords are errors; references, resource identifiers, `$schema`,
regex, formats and combinators are deliberately unsupported. Compilation is
offline, without filesystem or network schema resolution. No constraints are
silently ignored as unknown keywords. Duplicate JSON object keys are rejected
in both contracts and model output. Nested property names and enum values are
data, not schema keywords.

For example:

```json
{
  "type": "object",
  "additionalProperties": false,
  "required": ["decision", "reason"],
  "properties": {
    "decision": {"type": "string", "enum": ["ready", "blocked"]},
    "reason": {"type": "string", "maxLength": 4096}
  }
}
```

The exact schema is included in trusted system instructions. This path uses
prompting followed by local validation, not server-side constrained generation.
The final response must be exactly one complete JSON object, at most 1 MiB; prose
prefixes, Markdown fences, trailing objects and invalid values fail. There is no
repair inference or automatic replay. Schema validity establishes shape only.
Readiness, authorization, review acceptance and the meaning and validity of any
evidence citations remain Devbot's responsibility. In particular, a schema-valid
`approved` or `publish` value grants no authority to act.

## Boundary, sessions and evidence

The Rust tool boundary rejects `write_file` without launching the helper. The
shared harness's common tool definitions still list it; instructions identify it
as denied, and an attempted call receives a recorded denial. This does not rely
on the model obeying instructions. `exec` and `read_file` use the existing fixed
helper under the dedicated agent UID. Workspace, `.git` and persistent cache are
mounted read-only; only disposable temporary storage is writable. Attempts to
modify these mounts through shell commands fail at the filesystem boundary.
Commands have the same deadlines, output bounds and per-command PID namespace
lifetime as native workers. Unknown cleanup retains the workspace's active fence.

Host networking is still shared. Read-only filesystem access does **not** isolate
loopback, abstract Unix sockets or external network effects. Installed-account
qualification must examine reachable host services as described in the native
and provisioning guides. This feature makes no broader network-isolation claim.

Each invocation creates a random `ptr-` plus 64 lowercase hex session ID. There is
no resume flag for these roles. The role, exact contract digest, model identity,
full native execution binding and limits enter the conversation configuration
fingerprint. Separate immutable `read_only` policy files are required; do not
rewrite an edit policy to switch roles. Even a successful review starts a fresh
context next time. A worker session ID cannot be supplied to this protocol.

The private run directory retains:

| File | Meaning |
| --- | --- |
| `request.txt`, `output-contract.json` | Exact admitted inputs, committed before model startup |
| `invocation.json` | Session ID, configuration fingerprint and invocation binding |
| `invocation-events.jsonl` | Startup, streamed runtime observations and terminal event |
| `artifacts/tool-<nonce>-<effect>.json` | Committed tool request/result, including read inspections and denied writes |
| `response.raw.txt` | Exact completed model text, committed before parsing/validation |
| `response.json` | Validated JSON serialization, created only after successful validation |
| `invocation-result.json` | Terminal success or observed failure receipt |

Artifacts contain `schema_version`, `tool`, `arguments`, `result`, and `command`
(the latter is null for file tools). `evidence_log_path` is returned only after a
successful private artifact commit. Aggregate artifacts are bounded at 64 MiB,
individual artifacts at 1 MiB. References are opaque; the model cannot read those
paths. The caller verifies any claimed citations against the records. Unlike
worker reports, arbitrary domain output is not rewritten or granted special
reference semantics. Original full tool observations also remain in the private
shared session journal, whose normal log bounds still apply.

Malformed completed output retains `response.raw.txt` and the session/tool
observations. Cancellation, incomplete model responses and failed persistence
retain available journal events; they do not fabricate completed raw responses
or successful results. Startup or persistence failure may leave no terminal
receipt: absence is failure/uncertainty, never success. A stale `response.json`
without a matching terminal receipt and successful process exit is not accepted.
All writes are create-new; another invocation never overwrites prior evidence.

## Receipts and caller checks

`--check-inputs` returns protocol/version, `inputs_valid`, native `execution`,
and `invocation`. The invocation binding is identical in startup and terminal
receipts:

```json
{
  "capability": "playtime-readonly-v1",
  "role": "reviewer",
  "output_contract_sha256": "<exact input digest>",
  "request_sha256": "<exact input digest>",
  "execution": {
    "capability": "project-native-v1",
    "policy_path": "<canonical protected path>",
    "policy_sha256": "<digest>",
    "policy": {},
    "workspace_identity": {"device": 1, "inode": 2},
    "cache_identity": {"device": 1, "inode": 3},
    "git_identity": {"device": 1, "inode": 4},
    "helper_sha256": "<digest>"
  }
}
```

`policy` above abbreviates the full admitted native policy; placeholders are not
valid receipts. Startup has `schema_version:1`, `type:"invocation_started"`,
`session_id`, `execution` and `invocation`. Success has
`type:"invocation_finished"`, the same binding/session, `turn_id`, `cleanup`,
`usage`, and `response_sha256` over the exact `response.json` bytes. Failure has
`type:"invocation_failed"`, binding/session, observed `reason`, `cleanup`, and
`usage`; output validation failure uses `reason:"invalid_output"` and includes
the completed `turn_id`. Failure never includes a response success digest.

Before accepting output, the caller checks the selected protocol/version, fresh
session identity, exact frozen invocation binding, successful child and owner
exit, completed cleanup, matching terminal receipt, and response hash. It then
independently checks the output schema, cited evidence and domain rules. The
model cannot choose the policy, contract, role, artifact directory or record names.

## Validation limits

Run `cargo test -p playtime-chat --locked`, protocol tests, launcher tests and
`./scripts/test-rust-native-project.sh`, plus repository-wide required checks.
Fixtures exercise fresh role contexts, contract/role/execution compatibility,
malformed-output retention, source/read-tool inspection, write/source/Git/cache
denial, cancellation and existing worker continuation. Namespace fixtures run as
the operator UID with the existing test-only helper launcher; they do not qualify
sudo, the real dedicated account, model quality or live Strata inference.

Reviewed provisioning, installed cross-UID isolation/cleanup, Devbot role
integration and a complete owned live task remain separate rollout gates. No
account, privileged helper, candidate runtime or active checkout is installed or
enrolled by this implementation slice.
