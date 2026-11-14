# Project-owned development environments

The standalone launcher can use the selected project's `.devcontainer/devcontainer.json`
instead of the general workshop. The project owns its compiler/tool/dependency image;
Playtime owns the tool bridge, workspace mount, resource limits, offline execution,
logs and process cleanup. Inference still uses the selected remote model host.

Prepare explicitly from the project directory, then launch normally:

```sh
codex-launcher playtime-framework-fast --prepare-environment
codex-launcher playtime-framework-fast
```

Both commands accept `--workspace /absolute/project`. Preparation builds locally,
may download dependencies as specified by the Dockerfile, and probes the resulting
image as uid/gid 1000 without network access. Ordinary launches never build images,
fetch dependencies, or execute project lifecycle hooks. A missing or stale prepared
environment stops before acquiring inference. Preparation can also run independently
through the installed `project_environment.py --workspace /absolute/project`.

## Supported definition

The first version reads strict JSON from `.devcontainer/devcontainer.json`. It
supports `name`, `image` (an already available local image), or `build` with
`dockerfile`, `context`, `target` and literal string `args`. Build paths stay inside
the selected project. `remoteUser`, when given, must identify uid 1000. The fixed
workspace declarations are:

```json
{
  "workspaceFolder": "/work",
  "workspaceMount": "source=${localWorkspaceFolder},target=/work,type=bind"
}
```

These declarations may be omitted for Playtime; include them for other Dev Containers
clients to use the same mount layout. Compose, features, lifecycle hooks, custom mounts,
privileged options and other unsupported properties are rejected rather than ignored.
This is a deliberately bounded subset, not general Dev Containers compatibility.

Projects can declare additional build inputs, relative to the project root:

```json
{
  "customizations": {
    "playtime": {
      "inputs": ["docker", "dependencies.lock", "patches"]
    }
  }
}
```

Preparation fingerprints these files plus the definition, Dockerfile and Docker
ignore files. Declare dependencies copied into the image here. Changes require
re-preparation on the next fresh launch; normal source edits do not rebuild the image.
Up to 512 regular files and 16 MiB of definition inputs are supported. Directories
are expanded; links inside declared input directories are rejected.

## Image contract

The project image must provide Linux/amd64, Python 3.12, Bash, Git, ripgrep, and
an account for uid/gid 1000. The compatibility layer installs the exact existing
Playtime tool bridge and supplies `/usr/local/bin/python3.12` from
`/usr/bin/python3.12` when necessary. It does not install a project toolchain.
Implicit image volumes, OnBuild hooks and Playtime ownership labels are refused.
Descriptive base-image labels are allowed. The probe records available
tool versions and the container user, which supply the chat's environment inventory.
The general workshop retains its original inventory exactly for saved-session
compatibility.

Commands have the same private network, read-only root, `/tmp` allocation and
selected-project-only writable bind mount as general chat. Put persistent build
outputs/caches under `/work`, and prepare other dependencies into the image. The
project can provide image-internal aliases for an existing script's working paths.
The bridge remains uid 1000; the independent lifetime timer and owned cleanup remain
under Playtime's control. Project definitions cannot add host/device mounts or alter
those controls. The general workshop and experiment image locks are unchanged.

## Locks and saved sessions

Prepared image IDs, input hashes and observed inventories live outside the project
under `~/.local/state/playtime-environments`. Each preparation publishes a private
immutable lock and then updates the project's current pointer. Derived images retain
content-specific tags so dangling-image cleanup does not discard a previous session's
image. Explicit image removal can still make a saved environment unavailable; it is
never silently replaced.

New chat sessions copy their chosen lock to `environment.lock.json`. Explicit
`--session`, inspection and recovery use that saved lock, even when the project
has since changed its definition. Older sessions without this file use the original
general workshop. Existing model/workspace/configuration checks still apply.
The picker shows conversations compatible with the currently selected environment.
Use `--environment general` before chat options for the general workshop, or
`--environment /private/environment.lock.json` to select an older prepared project
image and see its compatible conversations. A project image change requires a fresh
conversation; this feature does not migrate history to changed capabilities.

## Validation

Run the launcher/environment Python fixtures and `playtime-chat` tests, then the
repository's Rust checks and frozen Python suite. Fixtures cover rejected execution
options/paths, dependency drift, source-only edits, private publication, saved-image
selection, legacy fallback and failure before inference ownership. An explicitly
owned live chat must additionally exercise the prepared image, actual tools,
completed-turn restart, and retained build outputs. Image probes alone do not
establish that a project's builds or tests pass.
