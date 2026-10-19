# Playtime

A bounded workshop for local LLM experiments. The model gets a short direction,
development tools, and optional starting materials. Each session starts a fresh
conversation; workspace files and the model's own `MEMENTO.md` carry work forward.
Runs leave event logs, workspace snapshots, and an HTML report.

Playtime is moving to Rust. The Rust daemon, CLI and workbench reached their MVP
in v0.1.0; [docs/rust-mvp.md](docs/rust-mvp.md) says how they fit together and
how to run them. The Python controller and the commands below are frozen.

## Start

Requirements: Linux, Python 3.12+, Docker access, and an Ollama server with a
tool-capable model installed. The controller uses only Python's standard library;
run it directly from this checkout.

The bundled recipes use `gpt-oss:20b` at `http://oldman-bird.local:11434`, currently
served by an RTX 4090. Adjust `model.endpoint` for your server. Edit
[`prompts/inspiration.txt`](prompts/inspiration.txt) to choose the model's direction.

`prepare` reuses the locked Docker image, restoring it from `.playtime/images/`
if needed. **A fresh clone does not include that archive.** If neither the image
nor its archive is available, add `--refresh` to the first command below to build
a workshop and replace the image/model locks with a new baseline. `prepare`
does not download models or configure Ollama.

```sh
./playtime prepare experiments/curiosity.toml
./playtime check experiments/curiosity.toml
./playtime smoke experiments/curiosity.toml
./playtime serve
```

Open <http://127.0.0.1:8765/> to preview a recipe, launch a run, and watch it live.
Browser launches require a working systemd user manager. To run from a terminal:

```sh
./playtime run experiments/curiosity.toml
```

The scripted `smoke` command checks Docker, tools, file persistence across a
reset, timeouts, reporting, and cleanup. Add `--live` to attempt the two-session
persistence task with the real model and smaller budgets.

## Recipes

Paths in recipes are relative to the project root. The recipes below use the
same model, inspiration prompt, protocol, and workshop image. Additional prompt
variants are described in the [chunked prompt comparison](experiments/prompt-screening.md).

| Recipe | Thinking | Sessions × work turns | Context tokens | Generation budget | Wall limit |
| --- | --- | --- | --- | --- | --- |
| [curiosity.toml](experiments/curiosity.toml) | high | 10 × 10 | 65,536 | 100,000 | 15 min |
| [pilot.toml](experiments/pilot.toml) | low | 12 × 20 | 32,768 | 60,000 | 2 hours |
| [empty.toml](experiments/empty.toml) | low | 12 × 20 | 32,768 | 60,000 | 2 hours |

These are upper bounds; the model may finish early or encounter an error.
A work turn is one model request followed by its tool calls.
`empty.toml` starts without seed files. The others copy
[`seeds/assortment-01/`](seeds/assortment-01/) into `/work/seeds/`.

Curiosity allows 8,192 generated tokens per work or handoff request, including
thinking. Context capacity limits one conversation; the generation budget
covers output across the entire run. They can be adjusted independently.
Curiosity permits two consecutive tool-call parsing failures to receive
correction feedback. The third consecutive error or eighth total parser error
stops the run. Failures still consume ordinary turns and budget.

## Dashboard and reports

Launch freezes the recipe, prompts, seeds, identities, and controller. Later
edits do not affect that run. Dashboard workers run as temporary systemd user
services and survive closing the browser or restarting the dashboard.
**Stop run** requests a final checkpoint and cleanup. CLI runs can also be
watched here; stop those from their terminal. Current CLI and browser launches
share a project lock, allowing one active run at a time without a queue.

The live page follows model output and tool activity. Scroll up to pause
following; **Follow live** resumes it. Completed turns show short previews and
can be expanded. Images, file changes, and mementos appear at checkpoints.

Saved reports offer **Compact**, **Readable**, and **Full** views, inline raster
images and animated GIFs, file previews, and incoming/outgoing memory diffs.
Memory labels distinguish a successful handoff from stale or missing memory;
they do not verify the model's claims. Generated HTML and SVG are displayed as
escaped source. Reports also work directly from disk.

Completed runs update `runs/index.html` and the local
[archive](http://127.0.0.1:8765/archive); [latest](http://127.0.0.1:8765/latest/)
opens the newest report. `serve` binds only to loopback; use `--port` to change
its port. Ctrl-C stops the dashboard.

For automatic startup at login, adjust the two checkout paths in
[`contrib/playtime-reports.service`](contrib/playtime-reports.service), then:

```sh
mkdir -p ~/.config/systemd/user
cp contrib/playtime-reports.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now playtime-reports.service
```

Restart the service after updating dashboard code. Worker diagnostics are
available through `journalctl --user -u playtime-<run-id>.service`; the run archive
remains the durable record after temporary services are collected.

## Workshop and memory

Select collections through `workshop.toolsets`. Each combination needs its own
`workshop.lock` path and prepared image.

| Collection | Capabilities |
| --- | --- |
| `core` (required) | Python, Bash, Git, ripgrep, Unix tools |
| `computation` | NumPy, SciPy, Matplotlib, Pillow |
| `browser` | Playwright and Chromium for local pages and screenshots |
| `systems` | C/C++ compiler, Make, Node.js, npm, SQLite |

The model has `exec`, `read_file`, `write_file`, and `end_session` tools. Offline
usage notes and installed versions are available at `/opt/workshop/`. Tool
responses are text; saved images and audio are artifacts for human review.

Commands run as uid 1000 in a container with a read-only image, no external
network, no host-directory mounts, and no Docker socket. Workshop programs use
CPU resources; the GPU serves inference. `/work` is a bounded RAM-backed volume.
Processes and `/tmp` are discarded between sessions; workspace files persist.
CPU, memory, storage, command, output, and log limits are set in the recipe.

At a boundary, the controller allows up to two handoff requests within the
remaining budgets, stops the container, and snapshots the workspace. The next
session automatically receives `/work/MEMENTO.md` (8,192 UTF-8 bytes in the
bundled recipes). Oversized memory is reported rather than silently truncated.
`end_session` saves memory; `finish_run=false` ends the session and
`finish_run=true` ends the whole experiment. The controller does not summarize
or correct memory. See [`prompts/protocol.txt`](prompts/protocol.txt).

Opt into `memory_condition = "self_plus_evidence"` to also supply a separate,
at most 4,096-byte appendix of the preceding session's observed file changes and
command results. The default is `"self"`. See the [paired memory experiment](experiments/memory-evidence.md)
for deterministic selection, frozen six-run commands, and the review protocol.

The controller reserves context for handoff and may shorten new tool results,
recording the truncation and original bounded result. Unknown generation usage
is shown separately and charged at the request's full allowance. Wall limits
bound agent work; snapshotting and cleanup may take additional time. Ctrl-C or
SIGTERM attempts a final checkpoint; an abrupt host failure can lose work since
the last snapshot.

## Records and repeat runs

```text
runs/<run-id>/
  manifest.json       # Recipe, identities, input hashes, runtime
  inputs/             # Frozen prompts, tools, seeds, and controller
  events.jsonl        # Requests, streamed output/thinking, tool results
  sessions/001/       # Workspace snapshot, file changes, received/saved memory
  final-workspace/    # Regular files extracted from the last snapshot
  result.json         # Stop reason, token accounting, errors, cleanup status
  report.html         # Human-readable report
```

Logs and archives stay outside the model's workshop. To regenerate a report or
repeat archived starting conditions in a fresh workspace:

```sh
./playtime report runs/<run-id>
./playtime repeat runs/<run-id>
./playtime repeat runs/<run-id> --sampling-seed 1043
```

Report regeneration leaves recorded inputs, events, and snapshots unchanged.
`repeat` checks input hashes, controller identity, Docker image identity, and
model digest/definition/Ollama version. If the controller has changed, use its
archived copy with the current project's artifact store:

```sh
python3 runs/<run-id>/inputs/controller/playtime --root "$PWD" repeat runs/<run-id>
```

`repeat --endpoint http://another-host:11434` changes the server address while
retaining model identity checks. A fixed seed does not guarantee an identical
trajectory across hardware or nondeterministic programs.

Keep `.playtime/` and `runs/` when moving the project; both are ignored by Git.
Model weights stay on the Ollama server and require a separate backup. For an
exact workshop, retain its archived image: rebuilding can change Debian
packages even though the base image and Python dependencies are pinned.
Use `prepare --refresh` deliberately when changing the workshop or model;
previous runs retain their own locks and archives.

## Development

Repository layout and contribution constraints are in [AGENTS.md](AGENTS.md).

```sh
python3 -m unittest discover -s tests -v
PLAYTIME_DOCKER_TESTS=1 python3 -m unittest discover -s tests -v
./playtime smoke experiments/curiosity.toml
```

The second command includes Docker integration checks. Use `smoke --live` with
a recipe when checking the real model's tool use and handoff behavior.

The Rust workspace contains `playtime-core`, `playtime-protocol`,
`playtime-engine`, `playtime-storage`, `playtime-runner`, `playtime-server`,
`playtime-client`, `playtime-local`, `playtime-cli`, `playtime-test-support`, and
`playtime-ui`: typed contracts, a bounded controller,
deterministic simulation, durable portable archives, and an authenticated
HTTP/client fixture. The Python command remains the operational runner. See the
[architecture decision](docs/rust-foundation.md),
[controller contract](docs/rust-controller.md), [storage contract](docs/rust-storage.md),
[durable simulation driver](docs/rust-archive-driver.md),
[HTTP/client contract](docs/rust-http-api.md), and
[compatibility notes](docs/rust-compatibility.md) for scope and remaining work.
With rustup installed, Cargo selects the pinned toolchain:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

The independent [backend ownership guard](docs/rust-backend-guard.md) has a CPU
subprocess/crash gate, `./scripts/test-rust-backend-guard.sh`. It fences requests
and preserves uncertainty using private durable state; production Ollama host
enforcement and worker integration remain separate work.

Run the two-session simulation without Docker, Ollama, or wall-clock sleeps:

```sh
cargo run --locked -p playtime-engine --example simulate -- crates/playtime-engine/tests/fixtures/two-session.json
```

This prints a simulation-labeled result and event trace. Scripted commands only
mutate an in-memory workshop; their command text is never executed on the host.
The example spends 26 measured tokens over 400 virtual milliseconds. Real Rust
model/workshop adapters and production service integration remain later work.

To publish and verify the synthetic two-session archive in a **new** directory:

```sh
cargo run --locked -p playtime-storage --example archive_fixture -- --output /tmp/playtime-archive-fixture
```

This is a fixture replay; it executes no controller, worker, model, or workshop.
The resulting folder contains all inputs and evidence and can be copied and
inspected without a service database.

To execute the real controller while committing its events and checkpoints:

```sh
cargo run --locked --release -p playtime-runner --example archive_simulation -- --scenario crates/playtime-engine/tests/fixtures/two-session.json --output /tmp/playtime-simulation
cargo run --locked --release -p playtime-runner --example inspect_archive -- --run /tmp/playtime-simulation
```

The output directory must be new. After closing the writer, the complete folder
can be copied and inspected independently of this checkout. This CPU-only driver
freezes its actual executable and gates each scripted effect on a durable intent.
See the [driver contract](docs/rust-archive-driver.md) for queries and failure bounds.

To exercise the HTTP API and shared client on CPU, start the bounded fixture in
one terminal and its walkthrough in another:

```sh
cargo run --locked -p playtime-server --example api_fixture -- --root /tmp/playtime-api-demo --listen 127.0.0.1:0 --lifetime-seconds 120
cargo run --locked -p playtime-client --example api_walkthrough -- --connection-file /tmp/playtime-api-demo/.playtime/connection.json
```

This serves labeled scripted replies through real authenticated HTTP, SSE and
verified downloads. It does not execute an experiment or supply durable worker
supervision. See [the HTTP/client guide](docs/rust-http-api.md) for the application
boundary that #20 connects to the real worker and archives.

The Rust CLI can use the same running fixture:

```sh
cargo run --locked -p playtime-cli -- --connection-file /tmp/playtime-api-demo/.playtime/connection.json recipes list --format json
./scripts/test-rust-cli-e2e.sh --backend fixture
```

See [the native client guide](docs/rust-cli.md) for launches, saved-key retries,
observation, downloads, and exit codes. The fixture gate is implemented; the
production simulation gate remains #20/#42/#43. `./playtime` is still Python.

The native Linux workbench uses that same public service connection:

```sh
cargo run --locked -p playtime-ui -- --connection-file /tmp/playtime-api-demo/.playtime/connection.json
./scripts/test-rust-ui-e2e.sh --backend fixture
```

It provides recipe preview/launch, pending launch recovery, a grouped live
transcript, memory/file inspection, and verified image previews/downloads. See
[the workbench guide](docs/rust-ui.md) for bounds, screenshots and performance.
The fixture milestone advances #22; the real simulation gate still requires
#20/#43 before #22 can close.
