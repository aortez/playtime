# Live model, called directly

The `live-two-session` catalog recipe runs the same packaged worker, controller,
writer and Docker workshop as [run-and-view](rust-docker-run-and-view.md). Its
model requests go to the locked `gpt-oss:20b` through the `playtime-ollama`
adapter, directly over plain HTTP. CPU and CI tests use scripted model effects.
Host enforcement and the guarded composition (#67, #68 as written, #70) are not
part of the MVP; only the owner and Playtime use the model host.

## Run it

```sh
target/debug/playtimed --root /absolute/service-root --bundle /absolute/new-bundle \
  --docker-scripted --ollama-endpoint http://oldman-bird.local:11434
target/debug/playtime --connection-file /absolute/service-root/.playtime/connection.json \
  run live-two-session --mode live_smoke --idempotency-key my-retained-key --wait
```

The workbench offers the same recipe. `--ollama-endpoint` needs
`--docker-scripted`, a plain `http://` URL with a host name or IP address, and
`XDG_RUNTIME_DIR`.

## Endpoint, identity and the live-run lock

- **Endpoint.** The static musl worker can't resolve mDNS names such as
  `oldman-bird.local`. The daemon resolves the endpoint at startup and freezes the
  addresses (IPv4, or IPv6 other than link-local) into the profile. An IP literal
  such as `http://192.0.2.10:11434` needs no resolver: it is frozen as it is,
  without a lookup. The worker connects only to those addresses through
  `Resolution::Pinned`; the host name stays in the URL and `Host` header. Each
  admission resolves again and refuses if the addresses changed: restart the
  daemon between runs after the host moves. No address is ever hard-coded.
- **Identity.** The profile freezes the exact model lock, a copy of
  `models/gpt-oss-20b.lock.json` tied to it by a unit test. The daemon inspects
  the model at startup and at each admission, and the worker inspects it at each
  session start. A mismatch refuses the admission or stops the run before the
  session's first request. Inspection is read-only metadata (version, tags,
  show); nothing on the model host changes.
- **Live-run lock.** Every checkout's daemon and worker share
  `$XDG_RUNTIME_DIR/playtime/live-model.lock`. The worker takes it before any
  Docker or model work and holds it, close-on-exec, for its whole lifetime;
  without it the worker fails before Begin. The daemon try-locks at admission
  and releases at once, so a second live admission from any checkout gets
  `run_busy` before a worker starts.

The worker build has no TLS: `playtime-ollama`'s `tls` feature is on by default
but off for the packaged worker and the daemon, so the static build needs no C
crypto toolchain. HTTPS endpoints are refused without it.

## Requests and results

Request settings are frozen with the profile: context 32,768, `think: "low"`,
seed 1042 (advanced per request by the controller), temperature 0.8, top-p 0.95,
top-k 40, repeat penalty 1.0 and `keep_alive: "5m"`. Catalog recipes choose their
own settings, including `think` levels `low`, `medium` and `high`; see
[recipes](rust-recipes.md). Streamed records reach the
controller as chunks while the request runs, so the workbench shows partial
thinking and content live, and a failed request keeps what arrived. Native
records arrive about once per token, so the worker coalesces their message deltas
into at most one chunk every half second or 16 KiB of text; the terminal record
is kept verbatim. The first live smoke recorded 1,332 events, one per token, and
the daemon's event stream needed about 465 seconds to deliver them, because each
read handled one record and rescanned the archive. Stream reads now take as many
records as the client's queue can hold (7.5 seconds for the same archive), and
coalescing keeps long runs inside their event-log budget.

The existing rules hold. Nothing retries after a possible dispatch, and only a
tool-parse failure lets the controller ask again. An incomplete stream or a
transport, HTTP, timeout or byte-limit failure stops the run with its own reason.
Usage stays unknown unless the terminal record reports it, and the full request
cap is charged then.

After each request the worker records one timing chunk: the latency it observed
and Ollama's load, prompt, generation and total durations. The run view and
workbench flag possible GPU contention from the server's durations, as #72
defines it:

- **Model reload.** A load takes over two seconds on any request after the first,
  so another model or context size was loaded in between. Warm loads report
  about 0.3 seconds.
- **Queue wait.** A request's total exceeds its load, prompt and generation time
  by over two seconds, so it waited behind other work. A queued request's wait
  shows up inside its total, not in the latency the worker observes.

The flags are advisory and never fail a run. On the retained clean runs, warm
loads took 0.22–0.33 seconds and the unexplained part of each total was at most
0.062 seconds.

## Profile and capabilities

The live profile is `docker_live_no_seed_self_v1`, worker kind `docker_live`, run
mode `live_smoke`. It reuses the scripted profile's workshop, capture and storage
limits. Its smoke limits follow #68's draft (two sessions, six turns per session,
120 work seconds, 4,096 tokens, 512 per request, 256 per handoff, 30-second model
and five-second command caps, 4,096 output bytes, 2,048 memento bytes, 8 MiB
log), with two differences:

- `tool_parse_retries = 2`. With none, about half of all smokes would fail at the
  observed 6.4% parse-error rate per request.
- Context 32,768 instead of 8,192. The controller's context guard always reserves
  8,192 tokens plus the handoff cap and output bytes before each request, so an
  8,192-token context would hand off before any work. The scripted profile uses
  32,768 for the same reason.

Current bundles advertise `real_model` and a `docker_profile` digest covering
the scripted smoke, live smoke, reference catalog recipe, and recipe bounds.
A live admission needs both; a bundle without
`real_model` still runs scripted Docker and simulation. Older bundles reject the
live worker kind.

## Evidence

```sh
./scripts/test-rust-service-e2e.sh --case run-and-view --backend live
```

The live gate refuses to start while any Playtime worker unit is active. It
launches one run from the CLI, closes and reopens the workbench while it runs, and
checks that another daemon root gets `run_busy` while the worker holds the lock.
It then requires agent completion, two checkpoints, `result.txt` containing
`persistence-ok`, a 32×32 PNG, non-empty mementos, a fresh second conversation
that carries the delivered memento, a copied archive, and exact Docker cleanup.
Every live response must follow streamed chunks, and any response that generated
for 1.5 seconds or more must show its first chunk at least 300 ms earlier. Batches
commit every 250 ms, so that chunk was readable while the request ran. Every
request must record timing, with no contention flagged.
Finally it launches the same recipe from the workbench and records that run's
outcome. Failed attempts are kept as records; the gate never loops. Set
`PLAYTIME_LIVE_ENDPOINT` to use another endpoint.

The first live attempt (run `20261018T214653Z-67973162`, evidence retained
outside version control) ran end to end in 28 seconds: two checkpoints, complete
cleanup, and two tool-parse errors recovered by the retries. It failed the gate
anyway. The event stream was too slow, as described above, and the model redid
session 1's steps in session 2, so the run ended at the session limit without
`result.txt`. In the second attempt (run `20261018T221255Z-cd78e60f`, 221 events
after coalescing) the stream and every infrastructure check passed. The model
retried a broken `bash -c` heredoc five times in session 1, then reported success
in session 2 without writing `result.txt`. This smoke tests the live path, not
the model's shell skills, so the prompt now gives each session two short commands
and their expected output. The third attempt (run `20261018T221517Z-679794a1`)
completed the task in 8.4 work seconds and passed every check except an earlier
version of the streaming check. That version timed records at the client, which
began following only after the busy probe, and each response had generated in
under half a second, so there was no partial output to show. The gate now uses
the archive timing above, and a CPU test checks commits directly.

CPU tests drive the live port through a fake transport and the real controller
and writer. They cover identity checks before each session, archived chunks, an
identity change that stops the run before another request, a cut stream that
keeps partial output while usage stays unknown, and a slow stream whose first
partial output is committed before the response.
