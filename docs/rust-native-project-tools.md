# Native project tools (opt-in)

`project-native-v1` lets the headless worker edit and test the assigned checkout at
its actual host path. The same controller, asynchronous driver, model transport,
private session journal and evidence-reference conversion remain in use. Docker
is still the default; native mode never performs Docker admission, reads an image
lock, or falls back to an unrestricted shell.

The dedicated account, reviewed privileged installation, cross-UID access/cleanup
tests and live model journey are separate integration prerequisites. Worker
protocol 1 still advertises only `roles: ["worker"]`. Explicit setup, supervision
and review use the separate [read-only invocation protocol](rust-readonly-model-roles.md).
Read-only native mounts do not isolate tools launched by an unrelated Codex client.

## Launch and capability contract

The trusted controller runs as the operator/control account. Only tool processes
cross to the dedicated account. After provisioning and review:

```sh
/private/fixed/package/run-strata.py framework-gyro-m-fast --worker \
  --tool-backend native --native-policy /etc/playtime/native/devbot-edit.json \
  --workspace /assigned/standalone/checkout --run-dir /private/run \
  --request /private/run/prompt.txt < /private/run/prompt.txt
```

Direct `playtime-task` accepts the same native arguments and the trusted endpoint
and model lock. Native mode rejects `--image-lock`; the wrapper also rejects
`--environment`. The wrapper checks the capability and local inputs before
acquiring inference. Its stdin assignment/owned-start release gate is unchanged.
`--check-inputs` validates local bindings without starting tools or inference; it
does not prove sudo authorization or the dedicated account's actual access.

`playtime-task --describe-protocol` returns these additive capability fields:

```json
{
  "protocol": "playtime-worker",
  "version": 1,
  "roles": ["worker"],
  "tools": "project-docker-v1",
  "tool_backends": {"docker": "project-docker-v1", "native": "project-native-v1"},
  "native_policy_schema": 1,
  "native_helper_sha256": "<64 lowercase hex digits>",
  "report": "worker-report-v1",
  "continuation": "completed-session-only"
}
```

Adding `--tool-backend native` to that local query selects `tools:
"project-native-v1"`. Missing capability is an admission error. Native
`--check-inputs`, `worker-invoked.json` and `worker_started` include `execution`:
capability, canonical `policy_path`, exact `policy_sha256`, parsed `policy`,
`workspace_identity`, `cache_identity` and `git_identity` (each device/inode), and
`helper_sha256`. This binding enters the conversation configuration fingerprint
and is saved in the session's `native-execution.json`. Devbot can compare the
run-local receipts with its own frozen setup without opening arbitrary session
paths. Policy, identity, role or helper changes reject continuation. Docker
sessions and instructions are not migrated.

Native configurations use the `native_v1` tool profile: the workshop's tool names
and argument schemas, described for the per-command sandbox at the checkout's host
path. Their recorded CPU value is a fixed nominal 1.0, because the sandbox has no
CPU quota and the binding must not depend on the CPU count a worker happens to see.

## Fixed helper and policy

Provisioning owns account creation, filesystem access and installation. Suggested
paths are `/usr/local/libexec/playtime-native-v1/helper` for the reviewed bytes of
`scripts/chat/native_tool.py`, `/etc/playtime/native/` for policies, and
`/opt/devbot-native/toolchains/rust-1.94.1` for a dedicated toolchain. The helper
has an absolute `/usr/bin/python3 -IS` interpreter and standard-library imports;
neither project PATH nor Python startup hooks select privileged code.

The helper, policy, toolchain and their ancestors must be root-owned, without
group/other write access or symlinks. Install the helper executable, keep policy
files readable by the trusted controller, and verify its SHA-256 against the
runtime's `native_helper_sha256`. The runtime embeds the reviewed helper bytes;
a mismatching installed helper is rejected. Normal chat installation does not
install privileged files or sudoers rules. Never install from an agent-writable
checkout while a supervising process is running from it.

Policy schema 1 has exactly these required fields. IDs and workspace below are
examples; provisioning records actual values and never claims an active checkout:

```json
{
  "schema_version": 1,
  "controller_uid": 1000,
  "agent_uid": 1001,
  "agent_gid": 1001,
  "workspace": "/assigned/standalone/checkout",
  "cache": "/var/lib/devbot-agent/projects/devbot",
  "toolchain": "/opt/devbot-native/toolchains/rust-1.94.1",
  "helper": "/usr/local/libexec/playtime-native-v1/helper",
  "mode": "edit",
  "limits": {
    "command_seconds": 300,
    "memory_bytes": 17179869184,
    "processes": 512,
    "output_bytes": 16384
  }
}
```

Use separate immutable policies for `edit` and `read_only`; do not rewrite a
shared policy between roles. The target UID/GID must be nonzero and the target
UID different from the controller. The account must have no administrative
memberships, agent socket or copied operator credentials.

The account must not be able to log in, or a tool could start a process outside
the sandbox through a local login service. Its shell must be `nologin` or `false`,
its password locked (`!` or `*`), and its passwd home and `~/.ssh` outside the
workspace, cache and toolchain. Admission checks the passwd entry. Before each
call the helper also checks the shadow entry, which only root can read, and
answers a failure with a refusal receipt. Hosts need `kernel.yama.ptrace_scope` of
1 or higher, which admission also checks, so that no tool can attach to a process
it did not start.

Admission refuses a checkout whose `.git/config` or `.git/config.worktree` could
carry credentials, because tools can read both and reach the network: a user or
password in an HTTP(S) URL, a password in any URL (including `url.<base>`
sections), an `http.extraHeader`, a `credential` section, or an include of another
file. SSH login names such as `ssh://git@host/` are allowed.

Provision the cache's `cargo/`, `cargo/git/`, `cargo/registry/` and `tmp/`
directories under the agent UID. Edit policies also need a root-owned
`/usr/bin/slirp4netns` (tested with 1.3.3); admission checks both. The toolchain uses
real binaries under `bin/`; it need not use Rustup proxies or a `rustup/` directory.
Roots must be canonical, disjoint directories. The workspace must be an ordinary
standalone Git checkout with a real `.git` directory; missing, file or symlink
Git metadata layouts are rejected. Tool roots cannot contain private
state, the request, policy, helper or running controller executable.

Authorize only this exact argv shape, with reviewed literal paths, in sudoers:

```text
/usr/bin/sudo -n -- /usr/local/libexec/playtime-native-v1/helper --policy /etc/playtime/native/devbot-edit.json
```

The sudoers command entry itself names the helper and exact `--policy` argument;
`sudo -n --` is the caller invocation. No wildcard arguments, SETENV, generic
shell/setpriv/runuser permission or model-selected UID/mounts are supported.
The helper checks effective root and the policy's exact `SUDO_UID`, then uses
`/usr/bin/setpriv` to drop UID/GID, supplementary groups and capabilities and set
no-new-privileges **before** Bubblewrap, a project shell, build script or file tool
runs. Limits are installed through the fixed `/usr/bin/prlimit` path, and
`/usr/bin/nsenter` starts each edit command's network after the same drop.

## Exposed filesystem, resources and lifetime

Bubblewrap creates separate user, mount, PID, network, IPC and UTS namespaces,
disables nested user namespaces, starts a new session and uses `--die-with-parent`.
System tools/libraries are read-only, and so are the sandbox root, `/etc`, the
masked `/usr/local` and `/dev`. `/lib64` is bound where the host has one. The
selected read-only toolchain is mounted explicitly. Only selected host name, TLS,
alternatives and loader configuration is exposed from `/etc`; `/etc/resolv.conf`
names the network's DNS forwarder in edit commands and is absent otherwise.
`/proc` belongs to the new PID namespace; `/dev` is a minimal new device tree. No operator home,
Docker socket, host SSH-agent socket or private controller records are mounted.
`HOME`, `CARGO_HOME`, `TMPDIR` and PATH are explicit; inherited environment is
cleared. `HOME` (`/tmp/home`) and `CARGO_HOME` (`/tmp/cargo`) start empty in each
command's private `/tmp`. `CARGO_HOME` links only `registry/`, `git/` and Cargo's
two package-cache lock files to the cache's `cargo/`, so Cargo configuration,
credentials and installed binaries never outlive a command. Because each command's
cache-usage tracker is new, Cargo's automatic cache cleaning is disabled.
A project may execute its own shell/build configuration only after these
boundaries apply. Read/write tools use an isolated Python interpreter inside the
same sandbox. `exec` uses Bash with `pipefail` and no profile/rc startup. As in
the Docker bridge, a relative `cwd` resolves against the checkout, `read_file`
returns only UTF-8 text, and `write_file` creates missing parent directories.

**Edit commands have a private network.** Each edit-mode `exec` gets its own
network namespace, connected to the outside by user-mode NAT through slirp4netns.
The helper starts slirp4netns as the agent, inside the user namespace that owns the
sandbox's network, where it runs in its own sandbox with `--disable-host-loopback`.
The host's loopback services, including the worker's model tunnel, and its
abstract Unix sockets are unreachable. The sandbox is 10.0.2.100, the gateway
10.0.2.2 and the DNS forwarder 10.0.2.3, which relays to the host's resolver.
The command is held until the network is up: a setup failure returns an error
receipt, and if the helper dies first the command never starts. The network ends
with the command. Read-only commands and the file tools have only their own
loopback.

Workspace, cache and `.git` mount sources use opened directory descriptors, with
expected device/inode identity checked by the helper. This prevents path
replacement between admission and mount selection from selecting another root.
`read_only` mounts both workspace and persistent cache read-only and refuses
`write_file`; a fresh `/tmp` remains writable. Symlink targets outside exposed
mounts are unavailable. A fixed read-only `.git` overlay applies even in edit mode,
preventing metadata writes and rename/replacement of that mount. Git's safe.directory
exception names only this assigned checkout; inherited/global Git configuration is
not loaded. In edit mode, source files and everything written to the cache,
including Cargo's downloads and `tmp/`, persist.
Each tool invocation gets its own PID namespace; background services end with
that command, including commands that start detached children. Nothing promises
background process continuity across commands or turns.

The example allows a 300-second command, a 16 GiB per-process address-space
limit, 512 processes per dedicated UID, 4096 descriptors per process, and 16 KiB
retained **per stdout/stderr stream**. Output is continuously drained after the
retained cap; truncation is explicit. Outside the bound roots only `/tmp` (512
MiB) and `/dev/shm` (64 MiB) are writable, both RAM-backed. There is no
2-CPU cap, aggregate memory limit or workspace/cache disk quota. The per-UID
process limit is shared, and address-space limits are not resident-memory quotas.
These limits are explicit policy/session inputs, not a claim that all host
resource consumption is isolated. System binaries/toolchain installation must
remain reviewed and immutable during a binding; this version does not hash every
system binary.

## Requests, cleanup and evidence

The controller starts `/usr/bin/sudo` with a cleared environment and cwd `/`.
It commits a private `active` intent before launch, using the same device/inode
workspace lease and fence name as Docker. Neither backend clears an existing
uncertain fence of the other backend. Native tools never reinterpret a Docker
proposal or silently replay an unfinished operation.

The helper accepts one JSON line, at most 256 KiB, within five seconds. It has
exact fields: `schema_version:1`, `policy_sha256`, `helper_sha256`, workspace/cache/Git identities,
`tool` (`exec`, `read_file`, `write_file`), `arguments`, `timeout_ms` and
`output_bytes`. Model arguments contain none of the policy/identity/launcher
choices. The helper re-reads the protected policy and its installed source and checks both
digests against the admitted bytes. The runtime also rechecks helper bytes and directory
identities before committing the intent. A wrong controller-chosen field
(schema, digests, identities, bounds, tool or field set) is a protocol error: the
helper exits nonzero and the caller keeps the fence. Model-chosen values the
helper refuses get a complete receipt carrying an error, and nothing is
dispatched: non-text or NUL characters in a command, `cwd` or path, non-text
content, a negative offset, a
non-positive or non-finite timeout, a non-boolean `append`, and `write_file` under
`read_only`. The runtime refuses the same values, and requests over 256 KiB,
before it writes its intent. Root modes and the cache's `cargo/`, `cargo/git/`,
`cargo/registry/` and `tmp/` directories are checked only at admission: tools can
change them, so a check at call time would let a tool fence the workspace.

The controller holds stdin open after the line. EOF (including controller loss)
or any extra input cancels the command. SIGINT, SIGTERM and SIGHUP delivered to
the helper also request orderly cancellation. The root helper kills its Bubblewrap
child on cancellation or deadline, and drains bounded output throughout.

Bubblewrap's monitor exits as soon as its command does, while its PID 1 may still
be stopping other processes, so the monitor's exit does not show that the sandbox
is empty. The helper is therefore a child
subreaper and learns that init's PID from Bubblewrap's `--info-fd`. Once the
monitor has exited, the init is the helper's own child, which it kills and reaps.
The kernel lets a PID namespace's init exit only after every other process in the
namespace has. The helper then kills and reaps the command's slirp4netns process
group, and returns one JSON object with `schema_version:1`,
`cleanup:"complete"` and `result` only once no tool process remains. If it cannot
identify the init while any child remains, or cleanup takes more than five
seconds, it exits nonzero instead. A file tool reports only after it has finished,
so its report stands even if the deadline stops the sandbox afterwards; a
`write_file` stopped before reporting says the file may have changed. Tool stdout
cannot forge the receipt because it is captured inside `result`. Protocol and
installation failures use a nonzero exit. The caller requires a successful helper exit and
complete receipt before removing its own active intent. A missing, malformed,
failed or uncertain receipt retains the fence and stops the turn. Cleanup gets a
five-second helper bound plus bounded caller reaping; no broad process scan/kill
or automatic recovery is attempted.

Exec results use the existing command/exit/timeout/output fields and private
[evidence-reference reports](rust-headless-worker.md#model-facing-references-and-canonical-conversion).
Raw model text, canonical report and terminal digest stay distinct. A native
report is still a worker claim awaiting independent Devbot checks. Docker-only
checks must be reported unavailable and run separately by the operator; a native
subset is not the complete suite.

## Validation and remaining qualification

```sh
./scripts/test-rust-native-project.sh
cargo test -p playtime-chat --locked
python3 -m unittest discover -s scripts/chat -p 'test_*.py' -v
```

The explicit namespace gate runs real file edits, Rust compilation/tests, command
errors, truncation, read-only mounts and sandbox root, private-path/socket denial,
the private network (unreachable host loopback and abstract sockets, NAT to a host
address, DNS through the forwarder, no network in read-only mode), per-command
`HOME` and Cargo configuration, network setup failure, helper death before the
command's release, workspace identity, refusal receipts, and descendant cleanup
confirmed by reaping the sandbox's init. It also drives native tools through the shared Rust driver,
saves reference-backed reports and resumes a completed session. Its test-only
helper runner bypasses privileged installation/UID admission and substitutes the
supplementary-group probe because an unprivileged fixture cannot clear host groups.
It never appears in the installed binary. These tests establish same-UID namespace
behavior only; they do **not** qualify dedicated-account or sudo access. Ordinary
CPU suites mark the real namespace fixtures ignored unless this gate is invoked.

Before first use, the provisioning/integration owners must install reviewed fixed
builds, record account/path/package/policy identities, confirm the account's
shell, locked password and home and the host's `ptrace_scope`, verify actual cross-UID
Rust builds and denied credentials/evidence/Docker access, test timeout/cancellation
and parent loss with detached descendants, and exercise read-only role enforcement.
The integration owner then runs the owned Framework model checkpoint, continuation
and interruption journey. Existing pilot receipts and active user sessions remain
untouched. Unsupported native supervisor/reviewer paths must be explicit admission
failures until a reviewed model-tool integration exists.

### Remaining exposure

[Issue #224](https://github.com/aortez/playtime/issues/224) tracked the shared
loopback and persistent agent configuration closed above. What remains:

- **Reachable networks.** LAN hosts and host services that listen on all
  addresses remain reachable through the NAT, as does the host's DNS resolver.
  Anything readable in the mounts can leave over the network. Admission refuses
  credentials in the Git config, but other readable secrets are not contained.
  Dedicated-account qualification must check the host's actual service exposure.
- **The network relay.** slirp4netns handles the sandbox's traffic outside
  Bubblewrap, as the agent in the host's network namespace, confined only by its
  own mount sandbox and seccomp filter. A flaw in it would give a command what the
  agent account can reach outside the sandbox, host loopback included.
- **Shared downloads and build output.** In edit mode, Cargo's index cache,
  extracted crate sources and Git checkouts, and any build output a project keeps
  in the cache, are shared and writable. Cargo does not re-verify sources it has
  already extracted, so a task can change what later tasks build without the
  change appearing in a reviewed diff. Give each trust domain its own cache, or
  clear these between tasks that must not influence each other.

Implementation references: [Bubblewrap documentation](https://github.com/containers/bubblewrap/blob/v0.11.1/bwrap.xml),
[setpriv](https://kernel.googlesource.com/pub/scm/utils/util-linux/util-linux/+/refs/tags/v2.41.1/sys-utils/setpriv.1.adoc)
and [slirp4netns](https://github.com/rootless-containers/slirp4netns/blob/v1.3.3/slirp4netns.1.md).


## Read-only model role integration

The follow-up [read-only invocation protocol](rust-readonly-model-roles.md) adds
fresh setup/supervisor/reviewer calls over this same sandbox and Strata transport.
It requires mode read_only and a pinned trusted output contract. Worker protocol
and continuation remain separate; Devbot must explicitly integrate/select the
new path. It does not install a helper or qualify dedicated-account execution.
