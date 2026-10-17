# Playtime

A bounded workshop for local LLM experiments. The model gets a short direction,
development tools, and optional starting materials. Its conversation resets
between sessions; files carry its work and memory forward. Each run leaves a
complete event log, workspace snapshots, and an HTML report.

The initial setup runs the controller and Docker workshop on `oldman-fish` and
uses Ollama at `http://oldman-bird.local:11434` for the 4090-backed model. These
hostnames are configuration, not requirements of the controller.

## Start

Requirements: Linux, Python 3.12 or newer, access to a Docker daemon, and an
Ollama server with a tool-capable model installed. The controller uses Python's
standard library. Run it directly from this checkout; no pip installation is
needed.

```sh
./playtime prepare experiments/pilot.toml
./playtime check experiments/pilot.toml
./playtime smoke experiments/pilot.toml
./playtime smoke experiments/pilot.toml --live
```

`prepare` builds the selected workshop, archives the finished image under
`.playtime/images/`, and locks the installed model's identity and Ollama version.
It reuses matching locks on subsequent invocations. It does not download a model
or change the inference server's configuration.

The bundled creative prompt is a placeholder. Replace
[`prompts/inspiration.txt`](prompts/inspiration.txt) with your direction, then:

```sh
./playtime run experiments/pilot.toml
```

For the short exploratory pilot, run `./playtime run experiments/curiosity.toml`.
It uses the same prompt, seeds, model, and workshop with high reasoning,
up to four sessions, 10 work turns per session, 100,000 generated tokens, and a
15-minute wall limit.
Both work and handoff responses allow up to 4,096 generated tokens, including
reasoning.
Its context window is 65,536 tokens per request. Context limits the current
conversation, tool output, and response; the generation budget limits total
model output across all sessions. These limits can be adjusted independently.
The model may finish early. This recipe permits recovery from up to two
tool-call parsing failures within the existing turn and token limits.
Observe its artifact, behavior across the first
reset, and response to failures without coaching it during the run.

The command prints the new run directory and report path. Every completed run
also updates the local gallery at `runs/index.html`. Open either HTML file
directly, or keep a local report server running:

```sh
./playtime serve
```

Visit <http://127.0.0.1:8765/> for all runs, or bookmark
<http://127.0.0.1:8765/latest/> for the newest report. The gallery refreshes every
15 seconds and discovers new reports without restarting the server. It binds
only to this machine's loopback interface. Use `--port` to choose another port.
Ctrl-C stops it; experiment runs are independent of the server.

For automatic startup at login, install the example user service (adjust its
two checkout paths if necessary):

```sh
mkdir -p ~/.config/systemd/user
cp contrib/playtime-reports.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now playtime-reports.service
```

Use `systemctl --user restart playtime-reports` after updating the viewer code,
or `systemctl --user disable --now playtime-reports` to stop automatic hosting.

For an empty starting workspace, use `experiments/empty.toml` instead.
The two smoke commands are infrastructure checks; they replace the creative
prompt with a two-session file-persistence task and use smaller budgets. The
default smoke uses a deterministic scripted model and still exercises Docker,
timeouts, snapshots, reporting, and cleanup. `--live` uses the locked model.

## Workshop choices

Choose collections in the recipe's `workshop.toolsets` array:

| Collection | Installed capabilities |
| --- | --- |
| `core` (required) | Python, Bash, Git, ripgrep, Unix tools |
| `computation` | NumPy, SciPy, Matplotlib, Pillow |
| `browser` | Playwright and Chromium for local pages, DOM checks, screenshots |
| `systems` | C/C++ compiler, Make, Node.js, npm, SQLite |

Every combination becomes one frozen image. Give a different combination its
own `workshop.lock` path. Short offline usage notes and exact installed package
versions are available inside the image at `/opt/workshop/`.

The agent has four tools: `exec`, `read_file`, `write_file`, and `end_session`.
Shell commands can create helper scripts or background jobs. Background jobs
must redirect output to files and stop at the session boundary. The browser
can reach a server on the container's loopback interface. External networking
is disabled. Screenshots and audio are artifacts; this initial model interface
does not give the model image or audio perception.
`read_file` checks the file header and requested slice for UTF-8 text. Binary or
non-UTF-8 data returns a short error and file size, without including the bytes
in the conversation. The model can use `exec` for metadata or numerical checks.

`seeds` selects a directory, or `""` for no seeds. Its contents are copied into
`/work/seeds/` at the start of each run. The included assortment contains a
synthetic signal, a color palette, and a cellular automaton. Descriptions and
provenance are included. Input symlinks are rejected.

## Sessions and limits

The pilot allows 12 sessions of up to 20 work turns, with a 60,000 generated-token
limit and a two-hour wall deadline. A turn is one model request followed by up
to eight tool executions. Two additional requests may be used for handoff,
within the same global budgets. The controller reserves generation capacity
for handoff. Thinking and ordinary output are counted using Ollama's reported
`eval_count`. Requests without final usage are explicitly marked as incompletely
accounted. Their full requested generation allowance is charged to the budget;
that reservation is not reported as measured usage. Later successful requests
do not erase the unknown counts.

Optional `limits.tool_parse_retries` (default `0`) permits a bounded number of
Ollama `error parsing tool call:` failures to receive correction feedback. The
curiosity recipe sets it to `2`: the third parser failure stops the run. Each
failed call uses an ordinary work or handoff turn. The model is told that none
of the failed response's tools ran and asked to emit valid JSON arguments. The
controller does not repair JSON, execute partial tool calls, or add extra turns.
This specific error is recognized in both stream chunks and JSON HTTP error
responses, including HTTP 500. Other HTTP errors, malformed response envelopes,
timeouts, and broken streams still stop the run. Wall, context, log, and token
limits continue to apply to every request, including corrections.

Results distinguish `generated_tokens` (server-reported usage),
`unaccounted_token_reserve` (full allowances for requests without usage), and
`token_budget_charged` (their sum). `token_usage_complete` stays false after any
unaccounted request, even if the run later finishes successfully. Error chunks
and `model_error` records preserve the failures and recovery decisions.
HTTP error responses are recorded as `http_error` metadata in the event log,
including their status, bounded body, and whether the body was truncated.

At each boundary the controller:

1. Gives the model an opportunity to write its own memento using `end_session`.
2. Stops and removes the agent container, including its background processes.
3. Archives the workspace and records changes and the memento.
4. Creates a fresh container and conversation with the same workspace files.

`/work/MEMENTO.md` is automatically supplied to the next session. The default
limit is 8,192 UTF-8 bytes. Oversized files are not silently truncated: the next
session receives an explanation and can inspect and shorten the file. If the
model does not save a handoff, its existing file remains. There is no automatic
summary or transcript retrieval.
If the first handoff response contains no tool call (including ordinary text
or an empty reply), the controller explicitly says that response did not save
a memento and asks for `end_session` before the existing second attempt. This
feedback is recorded in the report and event log. It adds no requests beyond
the two handoff attempts; all existing budgets still apply.

The protocol asks for a compact memento with current direction, important files,
checked results, unresolved problems, and a next action. It asks the model to
retain still-relevant facts from earlier sessions, verify its file and result
claims before saving, and record failed or unverified work explicitly. This is
guidance for the model; the controller neither fills those sections nor rewrites
its memory.

The wall deadline bounds agent work. Final snapshotting and cleanup can take
additional time after that deadline.

The controller uses a conservative input-byte guard with output/template
reserves to avoid overflowing the model context. It can end a session early.
If a large response or tool result leaves too little room even for handoff, it
records a skipped handoff and resets with the files already saved. It never
silently trims the conversation. This guard is deliberately conservative; it
is not an exact tokenizer implementation.

The workshop runs commands as uid 1000, with a read-only image, no capabilities,
no external network, and no host-directory or Docker-socket mounts. The pilot
uses four CPUs, 4 GiB of process memory, a 256 MiB workspace, and a 256 MiB
temporary filesystem. The workspace is a size-limited **RAM-backed Docker
volume**. A separate controller-owned store container keeps it mounted across
session resets and permits snapshots after an agent timeout. Snapshots are
ordinary files on the host. The image archive is approximately 2 GiB.

This workspace does not require configuring host filesystem quotas. Its size
limit bounds allocated data; sparse files can have larger logical sizes, so
snapshot output has a separate byte limit. Snapshot failure is reported and
the last successful snapshot remains available. `/tmp` and all process state
are cleared at each reset. The GPU belongs to inference; workshop programs
receive CPU access only.

Commands have deadlines and combined stdout/stderr limits. Truncation is
explicit in their results. The external event log has a byte limit. Each agent
container also has a root-owned lifetime timer, independent of the controller,
so workshop code cannot keep running indefinitely if the controller disappears.
Ctrl-C or SIGTERM attempts a final snapshot and cleanup. An abrupt host failure
can lose work since the last snapshot. After an uncatchable controller kill,
the store expires two minutes after the run deadline.

## Records

```text
runs/<run-id>/
  manifest.json             # Configuration, identities, input hashes, runtime
  inputs/                   # Exact prompts, tools, initial workspace, controller
  events.jsonl              # Requests, streaming responses, tools, timing
  sessions/001/             # Snapshot tar, file manifest/diff, memento, summary
  final-workspace/          # Regular files extracted from the last snapshot
  result.json               # Stop reason, counts, errors, cleanup status
  report.html               # Readable timeline and links to the records
```

The event log includes the exact request JSON and streamed model output,
including thinking when emitted. Tool results are the bounded results actually
delivered to the model. A report shows claims alongside observable tool results;
it does not assign a subjective creativity score.

Reports open in **Readable** view: each model turn is grouped with its tool
calls and results, with short passages visible and longer passages explicitly
excerpted. Incoming and saved mementos appear together for each session, along
with checkpoint file changes. **Compact** shows the action timeline;
**Full** opens the complete passages and raw records. Individual passages can
also be expanded. Partial model streams, unknown token usage, tool failures,
and tool output truncation stay visible. The complete requests and stream
chunks remain in `events.jsonl`.

Reports work directly from disk and contain no external assets. Their small
view-control script is permitted by an exact content hash. The local server
serves artifacts as downloads, blocks symlinks and path traversal, and exposes
only the run archive. Regenerating a report updates its presentation and the
gallery; it does not change recorded prompts, events, or snapshots.

Logs and snapshots are outside the model's environment. Workspace snapshots
preserve links in their tar archives; automatic artifact extraction creates
only regular files, so links cannot escape onto the host. The report escapes
model text and never embeds agent-generated HTML as active content.

```sh
./playtime report runs/<run-id>
./playtime repeat runs/<run-id>
./playtime repeat runs/<run-id> --sampling-seed 1043
```

`repeat` uses the original prompts, seeds, model lock, image lock, and recipe in
a fresh run directory. It verifies input hashes and controller identity. A new
sampling seed is an explicit experimental variation. Per-request seeds follow
`(sampling_seed + request_number - 1) % 2**31` and are recorded. Fine-grained wall
timing is logged outside model prompts; model budget updates use token and
session counts plus the fixed wall limit.

If the controller has changed, execute the archived copy to reproduce the old
protocol while using the current project's artifact store:

```sh
python3 runs/<run-id>/inputs/controller/playtime \
  --root "$PWD" repeat runs/<run-id>
```

`repeat --endpoint http://another-host:11434` can change the server address;
the model digest, definition, and Ollama version must still match.

## What is frozen

The base image is pinned in `workshop/base-image.txt`. Python dependencies and
their hashes are locked in `workshop/requirements-*.lock`. The complete built
image is saved and identified by its Docker image ID; package lists live inside
it. Reusing that image reproduces the workshop. A source rebuild can pick up
different Debian packages because the Debian repositories are not snapshotted.
Use the archived image when reproducing a run.

Model locks include the digest, template, parameters, model metadata, and Ollama
version. **Model weights are retained on the Ollama server, not copied into this
repo or its run archives.** Keep a backup of that server's Ollama model store
and installation if you need to reproduce runs after retiring or upgrading it.
GPU driver/kernel differences and nondeterministic programs can still change
the trajectory. A fixed seed does not promise an identical full agent run.

Keep `.playtime/` and `runs/` when moving the project to another machine; they
are intentionally ignored by Git. Missing Docker images are restored from
their verified archives. The controller snapshot captures source changes even
when they were not committed. Existing image archives and run archives are not
deleted when refreshing locks.

To intentionally change the environment or model:

```sh
./playtime prepare experiments/pilot.toml --refresh
```

This replaces the current recipe's lock files. Previous runs retain their own
copies. To update Python locks, edit the `.in` files and use `uv pip compile
--python-version 3.12 --generate-hashes --output-file <lock-file> <input-file>`.
Then refresh the workshop. `uv` is a maintainer tool, not a runtime dependency.

## Development checks

```sh
python3 -m unittest discover -s tests -v
PLAYTIME_DOCKER_TESTS=1 python3 -m unittest discover -s tests -v
./playtime smoke experiments/pilot.toml
./playtime smoke experiments/pilot.toml --live
```

The Docker suite checks non-root execution, storage and output limits,
background-process termination, the independent lifetime timer, numerical
tools, and Chromium. The controller tests cover stream failures, log limits,
archive paths, stable seeds, and HTML escaping.

If a host crash leaves stopped resources, inspect only this project's labeled
containers and volumes before removing them:

```sh
docker ps -a --filter label=playtime.run
docker volume ls --filter label=playtime.run
```

References: [Ollama chat API](https://docs.ollama.com/api/chat),
[Docker image pinning](https://docs.docker.com/build/building/best-practices/#pin-base-image-versions),
[Docker tmpfs storage](https://docs.docker.com/engine/storage/tmpfs/),
[Playwright browsers](https://playwright.dev/python/docs/browsers).
