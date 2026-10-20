---
name: run-workbench
description: Run the Rust workbench (playtime-ui) on a private headless display against a sandbox daemon, take screenshots and press its controls by name. Use when asked to run, screenshot, click through or demo the workbench, or to check a UI change in the real app rather than only in kittest tests.
---

# Run and drive the workbench

`scripts/workbench-sandbox.sh` starts an isolated setup in one private directory,
and `scripts/workbench-drive.py` drives it. Nothing appears on the desktop, and
the desktop's settings never change.

## Start and stop

```sh
./scripts/workbench-sandbox.sh start    # the first start also builds a worker bundle
./scripts/workbench-sandbox.sh status
./scripts/workbench-sandbox.sh stop     # keeps the directory, its runs and screenshots
```

The directory defaults to `/tmp/playtime-workbench-$UID`. Put `--dir DIR` before
the subcommand to pick another, and keep it short for Unix socket paths.
`start --bundle DIR` reuses a worker bundle, and `start --state-dir DIR` reuses
client state such as launch receipts and the layout.

`start` runs Xvfb on a free display, a private D-Bus session whose accessibility
launcher keeps settings in memory, `playtimed --cpu-test-supervisor` and the
workbench with the CPU Vulkan renderer. The daemon runs the scripted two-session
simulation, with no Docker, model or GPU, so any control is safe to press there,
including Launch and Cancel.

## Drive

```sh
D="python3 scripts/workbench-drive.py"
$D tree                                  # role, accessible name, screen rect, actions
$D shot launcher                         # prints the PNG path; look at the image
$D press Recipe --role 'combo box'
$D press 'Two-session controller simulation'
$D wait 'Ready to launch'
$D press 'Launch experiment'
$D wait 'Cancel run' && $D press 'Cancel run'
./scripts/workbench-sandbox.sh cli runs list
./scripts/workbench-sandbox.sh cli runs release RUN_ID --reason "why"
$D press Memory && $D press Files
$D press 'Download destination' --role entry && $D type /tmp/out.md
$D key Escape
$D scroll 670 500 down 6
$D resize 900 600                        # the narrow layout
./scripts/workbench-sandbox.sh ui        # close and reopen; the run carries on
```

Names match exactly. `--contains` matches a substring, `--index N` picks among
duplicates, and `tree TEXT` filters the listing. Timeline rows exist in the tree
only while they are laid out, so scroll them into view first.

## Rules

- Look at every screenshot you take. A blank or black frame means a failed start.
- To view a real daemon, such as the one `./go.sh` starts, ask the owner first,
  then run `start --connection-file FILE` with the file go.sh prints, by default
  `/home/data2/playtime/root/.playtime/connection.json`. The driver refuses
  Launch, Retry and Cancel there without `--force`, and live inference always
  needs the owner's go.
- Never set accessibility properties on the desktop's session bus.
  at-spi-bus-launcher writes them through GSettings, and with the real HOME that
  once switched on the desktop's `toolkit-accessibility`. `start` checks that the
  private launcher uses in-memory settings before it sets anything.
- Stop with `stop`, which signals only the processes it recorded. Don't kill by
  pattern.

## Troubleshooting

- Empty tree: AccessKit publishes the tree only while `ScreenReaderEnabled` is
  set on the private bus, which `start` does. `ui` restarts the window.
- Typing does nothing: there is no window manager, so `type` and `key` focus the
  window first. Click the field before typing.
- AT-SPI reports disabled buttons as enabled (upstream in
  `accesskit_atspi_common` 0.18.1), so check the screenshot.
- Red outlines while following a live run come from egui's debug-build
  `show_unaligned` overlay.
- Wheel scrolling animates, so `scroll` waits half a second before returning.
- Logs are in `DIR/logs`. For regression tests, prefer `egui_kittest` in
  `crates/playtime-ui/tests`, which drives the same widgets in process.
