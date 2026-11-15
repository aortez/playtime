#!/usr/bin/env python3
"""Drive the actual MATE Terminal on a private X display and retain screenshots.

Requires mate-terminal, Xvfb, xclip, python3-pil and python3-xlib.
Uses the offline driver. Does not touch desktop settings/clipboard, inference
hosts, or the normal installation. Artifact mode explicitly retains and replaces
the supplied disposable project's --artifact-file to verify persistence.
"""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


def wait(predicate, timeout=5):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        result = predicate()
        if result:
            return result
        time.sleep(.04)
    raise RuntimeError('timed out waiting for MATE acceptance condition')


def child(binary, output, artifact_session=None, workspace=None):
    import termios
    before = termios.tcgetattr(0)
    extra = ['--artifact-session', str(artifact_session), '--workspace', str(workspace)] if artifact_session else []
    code = subprocess.call([binary, '--actions', str(output / 'actions.jsonl'), '--tick-ms', '35', *extra])
    restored = before == termios.tcgetattr(0)
    (output / 'restored.json').write_text(json.dumps({'exit_code': code, 'termios_restored': restored}))
    print('\nTerminal restored. Restarting the same console.\n', flush=True)
    code = subprocess.call([binary, '--actions', str(output / 'restart-actions.jsonl'), *extra])
    (output / 'restart.json').write_text(json.dumps({'exit_code': code, 'termios_restored': before == termios.tcgetattr(0)}))
    print('\nRestart exited cleanly.', flush=True)
    time.sleep(2)


def run(args):
    from Xlib import X, XK, display
    from Xlib.ext import xtest
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    with (output / 'display').open('w+') as displayfd:
        xvfb = subprocess.Popen(['Xvfb', '-displayfd', str(displayfd.fileno()), '-screen', '0', '1500x1100x24', '-nolisten', 'tcp'],
                                pass_fds=[displayfd.fileno()], stdout=subprocess.DEVNULL, stderr=(output / 'xvfb.log').open('w'))
        term = None
        clipboard = None
        try:
            def number():
                displayfd.seek(0)
                return displayfd.read().strip()
            number = wait(number)
            env = {**os.environ, 'DISPLAY': ':' + number, 'GDK_BACKEND': 'x11', 'GSETTINGS_BACKEND': 'memory',
                   'GIO_USE_VFS': 'local', 'NO_AT_BRIDGE': '1', 'LIBGL_ALWAYS_SOFTWARE': '1'}
            env.pop('NO_COLOR', None)
            command = [sys.executable, str(Path(__file__).resolve()), '--child', '--binary', str(args.binary.resolve()), '--output', str(output)]
            if args.artifact_session:
                command += ['--artifact-session', str(args.artifact_session.resolve()), '--workspace', str(args.workspace.resolve())]
            import shlex
            term = subprocess.Popen(['dbus-run-session', '--', 'mate-terminal', '--disable-factory', '--sm-client-disable',
                                     '--hide-menubar', '--geometry=100x36', '--title=Playtime console acceptance', '-e', shlex.join(command)],
                                    env=env, stdout=(output / 'mate.stdout').open('w'), stderr=(output / 'mate.stderr').open('w'), start_new_session=True)
            d = display.Display(env['DISPLAY'])
            def window():
                for w in d.screen().root.query_tree().children:
                    if w.get_wm_name() == 'Playtime console acceptance' and w.get_attributes().map_state == X.IsViewable:
                        return w
            w = wait(window)
            w.set_input_focus(X.RevertToParent, X.CurrentTime)
            d.sync()
            time.sleep(.4)

            def codepress(codes):
                for code in codes:
                    if not code:
                        raise ValueError('key is not available on this keymap')
                    xtest.fake_input(d, X.KeyPress, code)
                for code in reversed(codes):
                    xtest.fake_input(d, X.KeyRelease, code)
                d.sync()
                time.sleep(.015)

            def key(*names):
                codepress([d.keysym_to_keycode(XK.string_to_keysym(name)) for name in names])

            def text(value):
                for char in value:
                    code = d.keysym_to_keycode(ord(char))
                    shift = d.keycode_to_keysym(code, 0) != ord(char)
                    codepress(([d.keysym_to_keycode(XK.string_to_keysym('Shift_L'))] if shift else []) + [code])

            def paste(value):
                nonlocal clipboard
                if clipboard is not None and clipboard.poll() is None:
                    clipboard.terminate()
                    clipboard.wait(timeout=2)
                clipboard = subprocess.Popen(['xclip', '-selection', 'clipboard', '-silent'], env=env, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                clipboard.stdin.write(value.encode())
                clipboard.stdin.close()
                time.sleep(.1)
                key('Control_L', 'Shift_L', 'v')
                time.sleep(.15)

            def shot(name, target=None):
                time.sleep(.15)
                from PIL import Image
                target = target or w
                geometry = target.get_geometry()
                pixels = target.get_image(0, 0, geometry.width, geometry.height, X.ZPixmap, 0xffffffff)
                Image.frombytes('RGB', (geometry.width, geometry.height), pixels.data, 'raw', 'BGRX').save(output / (name + '.png'))

            def actions(name='actions.jsonl'):
                path = output / name
                return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

            if args.artifact_session:
                import hashlib
                source = (args.workspace / args.artifact_file).resolve()
                assert source.is_relative_to(args.workspace.resolve())
                original_digest = hashlib.sha256(source.read_bytes()).hexdigest()
                caption = 'Matchbox: retained screen test'
                catalog = args.artifact_session / 'artifacts-v1.json'
                previous_ids = {r['id'] for r in json.loads(catalog.read_text())['artifacts']} if catalog.exists() else set()
                paste('/artifact retain ' + shlex.quote(args.artifact_file) + ' --caption ' + shlex.quote(caption))
                key('Return')
                def retained():
                    if catalog.exists():
                        records = json.loads(catalog.read_text())['artifacts']
                        return next((r for r in reversed(records) if r['id'] not in previous_ids and r['caption'] == caption), None)
                record = wait(retained)
                artifact_id = record['id']
                assert record['file']['sha256'] == original_digest
                paste('/artifacts')
                key('Return')
                time.sleep(.3)
                shot('01-artifact-list')

                def viewer():
                    for window in d.screen().root.query_tree().children:
                        if artifact_id in (window.get_wm_name() or '') and window.get_attributes().map_state == X.IsViewable:
                            return window

                def open_image(name):
                    w.set_input_focus(X.RevertToParent, X.CurrentTime)
                    paste('/artifact open ' + artifact_id)
                    key('Return')
                    view = wait(viewer)
                    view.configure(x=680, y=140, width=760, height=760)
                    d.sync()
                    time.sleep(.5)
                    shot(name, d.screen().root)
                    view.set_input_focus(X.RevertToParent, X.CurrentTime)
                    key('Control_L', 'w')
                    wait(lambda: viewer() is None)
                    w.set_input_focus(X.RevertToParent, X.CurrentTime)

                open_image('02-artifact-open')
                key('Control_L', 'd')
                wait(lambda: (output / 'restored.json').exists())
                assert json.loads((output / 'restored.json').read_text()) == {'exit_code': 0, 'termios_restored': True}
                source.write_bytes(b'Replaced by a later disposable test build.\n')
                wait(lambda: (output / 'restart-actions.jsonl').exists())
                time.sleep(.4)
                open_image('03-artifact-reopened')
                retained_file = args.artifact_session / ('artifact-' + artifact_id + '.png')
                assert hashlib.sha256(retained_file.read_bytes()).hexdigest() == original_digest
                assert hashlib.sha256(source.read_bytes()).hexdigest() != original_digest
                key('Control_L', 'd')
                wait(lambda: (output / 'restart.json').exists())
                assert json.loads((output / 'restart.json').read_text()) == {'exit_code': 0, 'termios_restored': True}
                assert all(a['type'] != 'submit' for a in actions() + actions('restart-actions.jsonl'))
                (output / 'report.json').write_text(json.dumps({
                    'terminal': 'MATE Terminal / VTE, private X11 display', 'binary': str(args.binary.resolve()),
                    'artifact_id': artifact_id, 'sha256': original_digest,
                    'checks': ['explicit UI retain', 'session artifact list', 'external image viewer', 'source replacement',
                               'saved browser restart', 'retained image reopened', 'no model submission', 'terminal restoration'],
                    'result': 'passed'}, indent=2) + '\n')
                print(output / 'report.json')
                return

            prompt = 'Review a wrapped prompt: ' + 'editing across rows ' * 7
            text(prompt)
            key('BackSpace')
            key('BackSpace')
            paste('\nfn greet() {\n    println!("café 界 🦀");\n}')
            shot('01-composer')
            key('Return')
            wait(lambda: any(a['type'] == 'submit' for a in actions()))
            draft = 'Keep this unsent draft while Markdown and tool results stream.'
            paste(draft)
            key('Return')  # Work must not queue or submit this draft.
            time.sleep(.6)
            shot('02-streaming')
            time.sleep(2.5)
            shot('03-tools-and-status')
            assert len([a for a in actions() if a['type'] == 'submit']) == 1
            w.configure(width=530, height=540)
            d.sync()
            time.sleep(.2)
            key('End')
            key('BackSpace')
            shot('04-narrow-resize')
            w.configure(width=1150, height=800)
            d.sync()
            time.sleep(.3)
            key('Return')
            wait(lambda: len([a for a in actions() if a['type'] == 'submit']) == 2)
            assert [a for a in actions() if a['type'] == 'submit'][1]['text'] == draft[:-1]
            key('Control_L', 'c')
            wait(lambda: any(a['type'] == 'stop' for a in actions()))
            time.sleep(.2)
            text('/raw 1')
            key('Return')
            time.sleep(.3)
            shot('05-raw-tool-result')
            key('Control_L', 'd')
            wait(lambda: (output / 'restored.json').exists())
            restored = json.loads((output / 'restored.json').read_text())
            assert restored == {'exit_code': 0, 'termios_restored': True}, restored
            time.sleep(.5)
            shot('06-restarted')
            key('Control_L', 'd')
            wait(lambda: (output / 'restart.json').exists())
            restarted = json.loads((output / 'restart.json').read_text())
            assert restarted == {'exit_code': 0, 'termios_restored': True}, restarted
            (output / 'report.json').write_text(json.dumps({'terminal': 'MATE Terminal / VTE, private X11 display', 'binary': str(args.binary.resolve()),
                'checks': ['wrapped backspace', 'bracketed multiline Unicode paste', 'streaming with draft', 'no follow-up queue', 'narrow/wide resize', 'Ctrl-C stop', 'raw tool result', 'terminal restoration', 'restart'], 'result': 'passed'}, indent=2) + '\n')
            print(output / 'report.json')
        finally:
            if clipboard is not None and clipboard.poll() is None:
                clipboard.terminate()
                clipboard.wait(timeout=2)
            if term is not None and term.poll() is None:
                os.killpg(term.pid, signal.SIGTERM)
                term.wait(timeout=3)
            xvfb.terminate()
            xvfb.wait(timeout=3)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/debug/playtime-console'))
    parser.add_argument('--output', type=Path, required=True, help='new evidence directory')
    parser.add_argument('--child', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--artifact-session', type=Path, help='completed disposable session for artifact acceptance')
    parser.add_argument('--workspace', type=Path, help='disposable project for artifact acceptance')
    parser.add_argument('--artifact-file', help='relative PNG test file to retain, then replace (artifact mode only)')
    args = parser.parse_args()
    if args.child:
        child(str(args.binary), args.output, args.artifact_session, args.workspace)
    else:
        if args.artifact_session and (not args.workspace or not args.artifact_file):
            parser.error('--artifact-session requires --workspace and --artifact-file')
        run(args)
