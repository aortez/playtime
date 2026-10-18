# Dedicated Ollama host boundary — #67 / #23 O2b

This slice adds `playtime-backend-host`, a restricted SSH bridge, the native
client/clock helper, and staged host policy. It does not install managed mode
merely by building or testing. **Dedicated-host activation and the live acceptance
gate require review and remain outstanding until explicitly performed.** No
worker capability, HTTP route or archive settlement is added; those belong to O2c.

## Processes and trust

`playtime-backend-host owner-initialize` is the narrow root deadline owner. Its
only production configuration path is `/etc/playtime-backend/host.json`; the unit,
endpoint, state and socket paths are fixed. It retains a stable private exclusion
inode, a systemd manager connection/reference and the original cgroup directory,
events and kill handles. Its bounded private operations are inspection, immutable
lease/request arming, verified terminal, release, stop/termination and explicit
administrator generation preparation. No operation accepts an executable, unit
name, shell command, URL or general systemd properties. `prepare-generation` is
root-only and requires original termination and released admission. It does not
resume an experiment or replay an inference request.
It also takes the deployment tool's stable maintenance fence, preventing a
supported generation change from racing activation or rollback. Stop and deadline
servicing never acquire that private-file fence.

`playtime-backend-host guard-initialize` runs as the dedicated non-administrative
`playtime-bridge` UID. It composes the existing #60 journal/machine with #55's
`Adapter`, including metadata inspection, native request encoding, partial
observations, completion and usage validation. Fixed chat bodies must match the
canonical O1 encoding, configured settings and original allowance exactly. Raw
partial output is forwarded even when completion is missing or malformed. No
missing usage is converted to zero, and no model-management operation is exposed.

The full bounded metadata inspection runs during preparation, before opening
admission. `verify_read_only` only checks the retained profile and fresh owned
manager identity, within the existing one-second bound. A new generation repeats
full inspection. Readiness also verifies that the loopback listener's socket inode
belongs to the original unit process tree; an unrelated listener cannot substitute
for it. The root helper's read-only proc descriptor inspection requires
`CAP_SYS_PTRACE`; this operation is absent from the stop path. Metadata still cannot atomically lock Ollama's mutable model
name. Root replacement/configuration changes are outside the supported managed
mode; observed identity replacement closes admission and never authorizes
stopping the replacement through an old unit binding.
Initial attachment has a fixed 50-second startup bound. Once the guard attaches,
its two-second liveness bound applies; reconnect cannot extend startup or any
lease/request deadline.

The root owner acknowledges arming only after its private deadline register is
durable. It arms its timer before waiting for that publication. The guard must
receive this acknowledgment before acknowledging acquisition or submitting
inference. Terminal disarming follows the guard's durable terminal record.
Normal terminal/release permits reuse of the same live generation. Tombstones
are retained in both registers. A failed/restarted owner never treats a missing
or corrupt file as a new empty generation. Ordinary unit restarts intentionally
fail closed; no `Restart=always`, automatic generation creation or journal reset
is provided.
The guard binds to the deadline owner but survives an Ollama stop, so clients can
still inspect termination and release the old lease before an explicit generation
change. Backend exit never grants a new lease by itself.

The guard's new optional `Enforcement` port leaves the existing CPU constructor
unchanged. In production it also adopts the owner's first stop operation before
recording the stop locally. A late cancel or reconnect cannot replace the first
reason, operation or deadlines. Publication failure still leaves independent
host stopping active. Wire `terminated` observations are diagnostic: the adapter
constructs held authority only from the authenticated root peer connection whose
owner retains the original manager/cgroup handles and exclusion. JSON read from
disk or an arbitrary IPC peer cannot provide that authority.

## Independent deadlines

The initial supported profile admits at most 120 seconds of remaining host work
and exactly 180 seconds between the mapped execution and cleanup deadlines.
Both must fit the mapped 300-second horizon. Requests can shorten their deadline.
The host owner uses an absolute Linux `CLOCK_BOOTTIME` timerfd, serviced by its
own event loop. Guard SIGKILL/SIGSTOP, stream backpressure, SSH loss and a blocked
guard journal do not own that timer. A dedicated state writer may block without
blocking the owner's timer or manager operations.

The same enforcing event loop feeds the owner's two-second systemd watchdog;
there is no independent heartbeat thread masking an unhealthy enforcing path.
Ollama binds to that owner with `BindsTo`/`After`, `Restart=no`, control-group
killing and a **two-second** manager stop timeout. This deliberately leaves
headroom within the original 90-second supervisor and 120-second observation
bounds if the owner itself dies or stalls. Manager delivery consumes the original
bounds; the held original cgroup kill handle is used when the supervisor deadline
expires. Scheduling or suspend overrun cannot manufacture timely termination.
The guard retains uncertainty once its original observation bound expires.

Termination requires fresh original boot/unit/invocation evidence and an empty
manager process tree, combined with the held original cgroup's `populated 0` or
the original kernfs object's `ENODEV` transition. The latter is an original-object
observation, not a lookup of a disappeared path: cgroup v2 can remove only a group
without live processes/children, and a dying cgroup cannot revive. MainPID exit,
quiet GPU, an SSH exit or a saved boolean does not suffice. The systemd fixture
exercises removal of the original kernel object and a subsequent replacement
invocation. See the [kernel cgroup v2 contract](https://docs.kernel.org/admin-guide/cgroup-v2.html),
[systemd 255 manager API](https://www.freedesktop.org/software/systemd/man/255/org.freedesktop.systemd1.html),
and [service watchdog/deadline semantics](https://www.freedesktop.org/software/systemd/man/255/systemd.service.html).

## Native client and protocol handoff

The existing guard control/inference protocol remains version 1, byte-for-byte.
Readiness uses the separate `playtime_protocol::backend_host` version-1 contract;
unknown versions/fields fail explicitly. A challenge echoes the client's boot,
send timestamp and nonce, and returns the authenticated host sample, exact
generation, lock-byte hash and fixed-settings hash. It grants no lease or dispatch
permission. Preview never bypasses the guard to query arbitrary Ollama endpoints.

`clock::map_deadline` takes local send/receive BOOTTIME stamps L0/L1, authenticated
host sample H and original local deadline D. It computes:

```text
host deadline = H + (D - L1) - 1000 milliseconds
```

It rejects RTT over one second, nonpositive results, overflow, boot/nonce mismatch
and horizons beyond 300 seconds. The one-second early margin assumes relative
clock-rate error at most 1000 ppm over this short horizon; it is not clock
synchronization. Arbitrary epoch offsets and transit asymmetry are supported.
`check_send` must run immediately before submission and after any local pause.
O2c must persist the **original mapping and operation together** before first
submission; retries reuse exactly those bytes and must not resample to extend
the operation. The host independently checks the mapped absolute deadlines on
admission and dispatch. BOOTTIME, not wall time, includes suspend.

`ssh::Client` freezes hashes of a private identity file and a one-line Ed25519
`known_hosts` pin for `oldman-bird.local`. It rejects drift before spawning fixed
`/usr/bin/ssh` argv with BatchMode, strict host checking, no shared control
connection, environment forwarding, local/remote commands or forwarding. The
administrative key in the notes is never passed to this client. Its arguments
contain file paths, never credential contents. `ssh::Channel` retains the SSH
process; dropping it closes the inference-owner channel, not an experiment's UI.

`playtime-backend-bridge` is a root-owned forced command under the dedicated UID.
It rejects any supplied remote command/arguments. Its first input byte selects
1=control, 2=inference, 3=readiness; every subsequent byte retains the existing
bounded protocol framing. These are three fixed Unix sockets, not destinations
chosen by a caller. Same-UID peer checks plus independent service and lease
credentials still terminate inside the guard. See [OpenSSH restrictions](https://man.openbsd.org/sshd.8).

Control JSON has a big-endian u32 length, at most 64 KiB. Inference appends the
exact admitted body (at most 4 MiB), receives the initial control reply, then tag
1/u32/raw bytes (512 KiB payload/frame, 4 MiB total), or tag 2/final control reply.
The forwarding queue remains 1 MiB. EOF is never a final reply. A matching request
retry returns recorded status only; the client never automatically resends it.
O2c must validate the final nonce/version/request binding and persist its own
archive settlement before exposing canonical progress. Host terminal evidence
neither finalizes an archive nor substitutes a zero token charge.

## Staging and review

`deploy/backend-host/manifest.json` pins the host and policy. The checked-in
systemd, SSH and nftables files are auditable templates. Only the narrowly owned
`inet playtime_backend` table is installed: non-loopback port 11434 ingress is
denied, and direct local port 11434 access is denied for every UID except the
guard, for IPv4/IPv6 including loopback. No UFW rule or global policy is flushed.
Administrative SSH remains unchanged. Root can bypass the policy by changing
it; root administration is outside managed mode.

Build native static host binaries first (a musl cross compiler is required):

```sh
cargo build --locked --release --target x86_64-unknown-linux-musl -p playtime-backend-host
python3 scripts/backend-host-deploy.py prepare \
  --stage /private/staging/playtime-backend \
  --bin-dir target/x86_64-unknown-linux-musl/release \
  --public-key /private/keys/dedicated-bridge.pub --guard-uid 65432 \
  --model-lock models/gpt-oss-20b.lock.json
python3 scripts/backend-host-deploy.py inspect --stage /private/staging/playtime-backend
python3 scripts/backend-host-deploy.py prepare-private \
  --stage /private/staging/playtime-backend --output /private/staging/host.json
```

Preparation only writes the requested private staging area. The offline native
profile helper verifies the locked model without network access. The public plan
records every installed artifact hash; private credentials are written separately
outside the repository and never printed. Do not copy the administrator's
root-equivalent SSH key to a worker. Provision the dedicated account and its
private root-owned/group-readable host configuration only after review; no account,
software, model, lock or credential installation is performed by `prepare`.
The stage includes its exact deployment tool and fixed-host manifest. Copy the
whole stage into a root-owned private directory on the host for the later review;
activation rejects user-writable paths or artifact/tool drift.
`client_known_hosts` is the public Ed25519 pin obtained through the previously
authenticated administrative SSH connection; copy it into private client state
with mode 0600. It contains no administrative credential.

Activation requires root on the exact host, the exact reviewed plan digest and an
explicit `--legacy-work-finished` acknowledgment. It records the original owned
paths, unit enablement/state and firewall/SSH fingerprints in a private transaction;
closes legacy network admission; holds and stops the original backend; proves
its original process tree exited; installs the reviewed artifacts; and starts a
new guarded generation. Existing work must actually have finished first. A quiet
GPU is not evidence for this acknowledgment. Never run the notes' installer.

```sh
# These are later, explicitly reviewed host actions, not part of the CPU gate.
sudo python3 backend-host-deploy.py activate --stage /private/staging/playtime-backend \
  --reviewed-plan EXACT_PLAN_SHA256 --legacy-work-finished
sudo python3 backend-host-deploy.py deactivate --stage /private/staging/playtime-backend \
  --reviewed-plan EXACT_PLAN_SHA256 --legacy-work-finished
```

Rollback first closes guard admission and proves original termination through
held cgroup evidence. It detects artifact/table drift and restores only its exact
owned changes. It retains journals, pending requests and private backups. A partial
activation with uncertain ownership stays closed and requires inspection and
reconciliation; re-running a command never resets its transaction. No timeout
alone restores Python/LAN access. Legacy mode is restored only after proof and
revocation; parent experiment recovery is a separate operation.

## Validation and remaining live gate

```sh
./scripts/test-rust-backend-host.sh cpu
./scripts/test-rust-backend-guard.sh
./scripts/test-rust-backend-host.sh systemd
python3 -m unittest discover -s tests -v
```

Default CI is CPU-only. The explicitly invoked systemd gate fails if its user
manager/cgroup prerequisites are unavailable; it never substitutes the #60 fake
manager. It starts only unique disposable `playtime-*-fixture-*` units, tests real
original process-tree exit, request deadline, disconnected owner, owner SIGKILL
and SIGSTOP/watchdog, and explicit replacement generation. It never modifies
`ollama.service`, uses the GPU or establishes production host access control.
CPU deployment tests use a disposable filesystem and mock host commands. A
separate systemd rollback case holds the original real cgroup, demonstrates
refusal while descendants remain alive, then restores only the disposable owned
files after actual process-tree exit. Firewall/install operations remain stubbed
in that fixture; it is **not** evidence of production firewall installation or
restoration of real legacy access.

Before calling #67 complete, review the exact stage and run the dedicated-host
gate: restricted SSH and forwarding denials; direct LAN and unprivileged local
IPv4/IPv6 denials; contention from both checkouts; bounded guarded inference;
timeout/disconnect cleanup; original cgroup termination; explicit new generation
and full lock reinspection; failed activation and rollback; and restoration of
legacy access only after revocation/cleanup. Record denied probes as denied, not
successful inference. These live steps remain outstanding. #23, #20, #21 and #22
also remain open; this component is not the Docker/Ollama run-and-view integration.
