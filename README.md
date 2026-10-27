# Playtime

A bounded workshop for local LLM experiments. The model gets a direction,
development tools, and optional starting materials. Each session starts a fresh
conversation; workspace files and the model's own `MEMENTO.md` carry work forward.
Runs preserve frozen inputs, event logs, workspace checkpoints, and token accounting
for review in the native workbench or CLI.

The Rust daemon, CLI, and workbench reached their MVP in v0.1.0. The
[architecture and operation guide](docs/rust-mvp.md) explains the execution path
and current limitations. The Python controller is frozen; `./playtime` is its
legacy launcher, while `target/debug/playtime` is the Rust client.

## Start

The production path requires Linux x86-64, a working systemd user manager with
cgroup v2, access to the supported local Docker Engine and locked workshop image,
and the locked Ollama model. Building requires the pinned Rust toolchain, its
musl target, and a C compiler/linker. The scripts also use Python 3 for build
metadata and connection-file handling; they do not run the Python controller.

The current model is `gpt-oss:20b`, with the default endpoint
`http://oldman-bird.local:11434`. A clone does not contain the workshop image or
model weights. Startup verifies the locked identities; it does not pull images,
download models, or silently refresh locks. See the
[Docker requirements](docs/rust-docker.md#supported-selection-and-readiness) and
[live-model guide](docs/rust-live-model.md).

```sh
rustup target add x86_64-unknown-linux-musl --toolchain 1.94.1
./go.sh
```

`go.sh` builds a frozen worker bundle and release daemon/client binaries, starts the
`playtimed-go` systemd user service, and opens the workbench. With this machine's
defaults, data lives in `/home/data2/playtime` and the connection file is
`/home/data2/playtime/root/.playtime/connection.json`. `./go.sh --help` lists the
data-directory, endpoint, and unit settings.

```sh
./go.sh --no-build   # Reopen the workbench using the existing build.
./go.sh --stop       # Stop the daemon between runs.
```

Closing the workbench leaves the daemon and experiment running. Do not restart
the daemon during a run: automatic reattachment is not implemented. For manual
startup and CLI launch/cancel/release commands, see
[Run it](docs/rust-mvp.md#run-it).

## Experiments and review

The daemon loads reviewed TOML recipes from `recipes/` and freezes the selected
recipe, seed contents, limits, identities, and worker into each archive. The
`curiosity` recipe permits ten sessions of ten work turns, a 64K context,
300,000 generated tokens, and 30 minutes of controller work. These are upper
bounds; the model can finish early. Recipe limits change only with the owner's
review. See [recipes and their bounds](docs/rust-recipes.md).

Model tools run as uid 1000 inside the workshop, with no external network, host
mounts, or Docker socket. Only `/work` persists between sessions; conversations,
processes, and scratch space reset. Checkpoints preserve files and mementos.
Unknown token usage stays visible and is charged at the request's full allowance.

The CLI and workbench use the same authenticated loopback API. They show live
thinking/output, tool activity, progress against frozen limits, checkpoints,
memory, files, and verified image previews/downloads. A cancelled or failed run
can require `playtime runs release RUN_ID --reason TEXT` after the worker and
owned resources have stopped. Release frees admission without rewriting the
archive or claiming a successful outcome.

Run folders can be copied and verified independently of the service ledger.
Rust HTML reports and repeat/pilot workflows are not implemented. Historical
Python experiment plans and results remain under `experiments/`; they are not
instructions for running the Rust controller.

## Development and documentation

- [Architecture, operation, and document map](docs/rust-mvp.md)
- [Development setup and test gates](docs/development.md)
- [Experiment studio design and next phase plan](docs/experiment-studio.md) (draft)
- [Repository working rules](AGENTS.md)
- [CLI commands and connection settings](docs/rust-cli.md)
- [Workbench behavior and screenshots](docs/rust-ui.md)
- [Compatibility rules and reviewed behavior changes](docs/rust-compatibility.md)

`crates/` contains the Rust workspace; `xtask/` packages workers. `workshop/`
contains the locked image definition, fixed tool bridge, and in-container tool
documentation. `models/` and `environments/` hold committed identity locks.
`.playtime/`, `runs/`, and `target/` are local data/build directories and are not
committed. Preserve existing run records and image archives.
