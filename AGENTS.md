# Playtime

This repository is a local-model experiment controller and a disposable Docker
workshop. Keep the controller independent of third-party Python packages.

- `src/playtime/`: host controller, Ollama adapter, snapshotting, reports, CLI.
- `workshop/`: image definition, locked packages, fixed container tool bridge.
- `experiments/`: complete TOML recipes. Paths are relative to the project root.
- `prompts/` and `seeds/`: versioned experimental inputs.
- `models/` and `environments/`: small identity lock files; commit these.
- `.playtime/` and `runs/`: local image archives and run records; never commit.

Use `python3 -m unittest discover -s tests -v` for controller tests. Use
`PLAYTIME_DOCKER_TESTS=1 python3 -m unittest discover -s tests -v` when changing
the Docker integration or workspace limits. `./playtime smoke
experiments/pilot.toml` exercises the real runner without a model; `--live`
adds a small real-model check.

Preserve these properties:

- Model-controlled commands execute only inside the workshop as uid 1000.
- Logs, input archives, controller code, and Docker access stay outside it.
- Each session starts a new conversation. Only workspace files persist.
- Limits and cleanup are enforced by the controller and container lifetime.
- Keep partial inference visible; never report unknown token usage as zero.
- Archived model text and filenames must be escaped in generated HTML.
- Reruns verify identities and archived inputs; do not silently update locks.

Changing workshop sources requires a deliberate `prepare --refresh` and a new
locked image. Preserve existing run records and image archives.
