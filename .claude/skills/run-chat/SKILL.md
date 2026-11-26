---
name: run-chat
description: Run the interactive chat console (playtime-chat) in a private tmux session against a scripted model, then type, press keys, paste, read the screen and queue the model's replies. Use when asked to run, try, demo or UX-test the chat console, or to check a console change in the real binary rather than only in tests.
---

# Run and drive the chat console

`scripts/chat-sandbox.py` runs the real `playtime-chat` in a private tmux session
against `scripted-strata`, a scripted Strata endpoint. The controller, records
and tools are real; by default tools run in the Docker workshop. Only the model
is scripted, so no GPU host is involved, and other tmux servers are untouched.

## Start and stop

```sh
C="python3 scripts/chat-sandbox.py"
$C start                     # builds, starts the model and console
$C status
$C restart                   # rebuilds and starts the console in a new session
$C restart -- --session DIR  # reopens a saved session instead
$C stop                      # the directory keeps its logs, requests and state
```

The directory defaults to `/tmp/playtime-chat-$UID`. Put `--dir DIR` before the
subcommand to pick another, and keep it short for the tmux socket path.

`start` makes a sample project with a failing test unless `--project DIR` names
one. A project must be owned by you, not world-writable, and outside this
checkout. `--size WxH` sets the terminal, 120x40 by default. Arguments after `--`
go to `playtime-chat`. `--preset coding-host` runs queued commands on the host
as you, without a sandbox, so queue only commands you'd run yourself.

## Drive

```sh
$C say 'Please fix the failing test.'  # types, then presses Enter
$C type 'a draft'                      # without Enter
$C key Escape                          # tmux key names, such as C-c, C-u and Up
$C paste $'line one\nline two'         # a bracketed paste, as a terminal sends it
$C wait AgentFinished --timeout 30     # also --regex (multiline), --gone, --all
$C wait '^you>\s*$' --regex            # back at the prompt
$C screen                              # --all adds history, --ansi keeps colours
$C resize 80x24
$C raw                                 # the end of the raw output, escapes shown
```

Look at the screen after each step. `wait` prints it to stderr when it fails.

## Script the model

Each model request takes the next queued reply. With none queued, a short reply
quotes your last message.

```sh
$C reply --think 'Run the tests first.' --exec 'python3 -m unittest -q'
$C reply --read greet.py
$C reply --write greet.py $'def greet(name):\n    return "Hello, " + name + "!"\n'
$C reply --text 'Fixed: greet now ends with "!".'
$C reply --think 'Hmm.' --end stall          # hangs until the console leaves
$C reply --text 'Partial' --end drop         # closes without a terminal event
$C reply --end failed                        # or --end incomplete
$C reply --pace-ms 50 --text 'Slowly'        # start --pace-ms sets the default, 6
$C reply --usage '{"input_tokens": 150000}'  # reported usage, for compaction
$C replies
$C requests                                  # each request; N --body shows one
```

A reply streams its thinking, then its text, then its calls. The workshop's
tools are `exec` (command), `read_file` (path) and `write_file` (path, content);
commands start in `/work`. `--call NAME JSON` sends any other call, and
`--spec FILE` a whole reply. Usage counts about four bytes a token, so context
figures look realistic.

## Watching together

`$C attach` prints a read-only `tmux attach` command that the user can run in
their own terminal to watch. Without `-r` they can type too. The terminal size
stays fixed while they're attached.

## What to expect

- After a failed turn, or Escape during inference, the console exits: remote
  inference needs owner shutdown, and a direct endpoint has no launcher to
  restart it. `restart` starts a new session. Escape during a tool returns to an
  in-console recovery prompt, and `/recover` continues in a fork.
- When the console exits, tmux adds a "Pane is dead" line, which scrolls the top
  line into history. Use `screen --all`.
- `key` sends keys 0.1 s apart. The console reads Escape followed within 40 ms
  by another byte as an Alt-key sequence.

## Rules

- This tool never reaches a real model. A live session goes through the owner
  launcher on Framework or Bird: ask the user first every time, and check that
  no other client, such as Devbot, is using the host.
- Stop with `stop`, which signals only the processes it recorded. Don't kill by
  pattern.
- For regression tests, prefer Rust tests over the console's pieces. The sandbox
  is for seeing the real binary the way a user does.
