# Interactive host coding

`--preset coding-host` uses Playtime's terminal client, saved conversations and
shared compacting controller with the pinned Codex-derived coding prompt. Tools
run directly on the workstation as the invoking user, in the launch directory.
The inference model remains a separate selection:

```sh
codex-launcher playtime-bird-nvfp4 --preset coding-host
```

The wrapper accepts `--preset coding-host` before chat arguments and alongside
its other leading options, including `--sessions`. Direct `playtime-chat` accepts
`--preset coding-host` with the usual endpoint/model-lock arguments. It does not
read Docker image locks, prepare images or scan the workspace tree. It rejects
Docker environment selection and the legacy minimal prompt. The startup banner
shows `Preset: coding-host` and host filesystem/network access.

The host needs Linux, `/bin/bash`, `/usr/bin/python3`, systemd's `systemd-run` and
`systemctl`, and a working systemd user manager. The helper is embedded in the
installed Rust binary; no project script is used as its implementation. Tools
inherit the caller's environment, PATH, home and normal user permissions.
Commands use Bash without profile/rc initialization. Device permissions still
come from the user's groups and host setup. There is no filesystem, network,
device or IPC sandbox, sudo API, CPU/memory quota, or claimed protection of
same-user session records. State and the installed client stay separate from the
checkout to avoid accidental edits, but host execution can access them.

## Tools and process lifetime

The frozen `host_v1` tool contract supplies:

- `exec`: run a Bash command, defaulting to the real workspace path. The default
  controller command limit is 300 seconds; `timeout_seconds` may shorten it.
  Output is bounded and marked when truncated. Command descendants stop when
  this invocation completes, times out or is cancelled.
- `read_file` / `write_file`: bounded UTF-8 reads and text writes to regular host
  files. Absolute paths, outside paths and symbolic links use ordinary user access.
- `start_process`: start an explicitly persistent command, optionally `tty: true`.
  The returned `process_id` belongs to this running CLI's process registry.
- `poll_process`: read incremental bounded output and exit status, or omit the ID
  to list current process handles. Polls can wait up to ten seconds.
- `write_process`: send input; partial writes report the unsent byte count. Pipe
  input can be closed explicitly; PTYs accept terminal control characters.
- `stop_process`: stop the selected command and descendants and consume its final
  output. It releases the process handle.

Persistent jobs survive assistant turns and compaction. They stop on CLI exit,
including control EOF, and are never restarted when a conversation is reopened.
Old handles in saved history must be checked against `poll_process`. At most eight
process handles are retained; stop finished jobs to release them. This registry
is independent of the model's context summary.

Each command gets a uniquely named systemd user scope. The helper journals the
scope before dispatch and confirms it stopped before clearing its reservation.
Cleanup includes descendants which change process group or session. Only the
owned scopes are stopped; unrelated workstation processes are unaffected. Stop
requests allow one second of grace, then send SIGKILL to remaining owned scopes,
with one shared eight-second scope verification deadline. The
supervisor receives cancellation independently of model inference and record
writes. An unresolved control/cleanup operation retains the private `active`
fence with its unit names; subsequent launches refuse to silently reinterpret it.
The same directory identity lease prevents competing Playtime host/Docker/worker
sessions from editing the same checkout under the same state root.

Session configuration freezes the host profile, workspace device/inode, user,
helper digest, prompt and tool schemas. Docker conversations keep their original
fingerprints when loaded with their original prompt and cannot be silently resumed
with host capabilities. New host records use conversation schema two: `execution`
holds tool profile, policy, sampling and budgets; `compaction` holds summary settings.
Host execution has no workshop allocation. Legacy schema-one records keep their
original serialization and fingerprints. An internal compatibility allocation is
supplied only to the shared archive controller, never to host admission or execution.
The updated helper digest deliberately changes the host capability binding: older
host sessions are not silently upgraded to new tool behavior. Turn cleanup
accounts for foreground command scopes; persistent jobs belong to CLI-session
cleanup. The CLI holds model ownership through that final cleanup.

## Terminal controls

During generation, one activity row shows a first-line preview, spinner, elapsed
time, received text size and average KiB/s since the request began (including the
initial wait). This measures received text bytes, not model tokens. Responses
print in complete blocks at the next effect
boundary. `--stream` restores the previous display; redirected output and JSON
remain streamed. Tool results show a compact status and first output line;
`/tool` lists recent results and `/tool N` displays captured stdout, stderr,
file content and metadata. The CLI retains 32 results until exit or restart;
session records retain the durable evidence. Buffering does not delay cancellation
or change saved history.

Bracketed multiline paste stays one editable message until Enter. Escape, Ctrl-C
or `/stop` interrupts work. After confirmed cleanup and observed tool outcomes,
the CLI returns to input using a new recovery fork containing completed history
and a bounded notice about the stopped turn. Workspace changes are preserved;
commands and the interrupted message are never replayed. Unknown tool outcomes
display an evidence report and wait for `/recover` to acknowledge possible partial
changes, or `/quit` to exit. If a remote model request needs
owner shutdown, the launcher stops and reacquires that owner before continuing;
this client restart also stops persistent host processes.

## Launcher entry

Add this entry to `~/.codex/codex-launcher.toml` to make the choice visible in the
normal launcher menu. Model settings continue to come from the installed lock.

```toml
[[harness]]
name = "playtime-bird-nvfp4-host"
harness = "Playtime"
description = "Host coding with the Codex prompt and Playtime compaction."
launch = ["~/.local/lib/playtime-chat/run-strata.py", "bird-basalt-nvfp4", "--preset", "coding-host"]
status = ["~/.local/lib/playtime-chat/run-strata.py", "--status", "bird-basalt-nvfp4"]
model = "bird-qwen38-flashnext-nvfp4-dq8-basalt"
provider = "Basalt on Bird"
context_window = 163840
effort = "xhigh"
tools = "host filesystem, network, devices; persistent processes and PTYs"
```

## Verification

In addition to the Rust workspace and launcher checks:

```sh
PLAYTIME_TEST_HOST_TOOLS=1 python3 -m unittest discover -s scripts/chat -p test_host_tools.py -v
cargo test -p playtime-chat --test host --locked -- --include-ignored
```

These gates use temporary projects and actual systemd scopes. They exercise host
paths and toolchains, socket/special-file admission, network communication,
PTY input, timeout/cancellation, detached descendants, control-EOF cleanup,
persistent processes through the real controller's compaction, and saved-session
reopening with a fresh process registry. They do not prove access to any physical
USB/serial device. An owned live model smoke separately qualifies the installed
launcher, model transport and coding behavior.
