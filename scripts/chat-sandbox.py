#!/usr/bin/env python3
"""Run the chat console in a private tmux session against a scripted model.

One directory holds the tmux socket, the scripted Strata endpoint with its
reply queue and request log, a sample project, the console's private state and
the logs. No real model is involved, and other tmux servers are untouched.

  start [--preset P] [--project DIR] [--size WxH] [--pace-ms N] [-- ARGS...]
                                  build, then start the model and the console
  restart [--no-build] [-- ARGS...]
                                  stop the console and start it again
  stop                            stop everything; the directory stays
  status                          the console, model, queue and paths
  screen [--all] [--ansi]         the visible screen, or with its history
  say TEXT                        type TEXT, then press Enter
  type TEXT                       type TEXT without pressing Enter
  key KEY...                      keys one at a time: Enter, Escape, C-c, Up
  paste TEXT | --file FILE        a bracketed paste, as a terminal sends it
  wait TEXT [--timeout S] [--gone] [--regex] [--all]
                                  wait until the screen shows TEXT
  reply [--think T] [--text T] [--exec CMD] [--read PATH]
        [--write PATH CONTENT] [--call NAME JSON] [--end E] [--pace-ms N]
                                  queue the model's next reply
  replies                         queued and used replies
  requests [N] [--body]           what the console sent the model
  resize WxH                      resize the terminal
  raw [--bytes N]                 the end of the console's raw output
  attach                          print the command that watches the session

ARGS after -- go to playtime-chat; restart -- --session DIR reopens a saved
session. DIR defaults to /tmp/playtime-chat-$UID; keep it short because of
Unix socket path limits.
"""
import argparse
import json
import os
import re
import shlex
import signal
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TARGET = 'chat'
# tmux drops trailing spaces, so the prompt is matched without its space.
PROMPT = re.compile(r'^you>', re.MULTILINE)
CONFIG = '''\
set -g remain-on-exit on
set -g status off
set -g escape-time 0
set -g history-limit 100000
'''
# Names that tmux spells differently.
KEYS = {
    'backspace': 'BSpace',
    'ctrl-c': 'C-c',
    'ctrl-d': 'C-d',
    'delete': 'DC',
    'esc': 'Escape',
    'return': 'Enter',
}
SPEC_FIELDS = {'think', 'text', 'calls', 'end', 'pace_ms', 'usage'}
SAMPLE = {
    'README.md': '# Sandbox project\n\nA small project for trying the chat console.\n',
    'greet.py': 'def greet(name):\n    return "Hello, " + name\n',
    'test_greet.py': (
        'import unittest\n\nfrom greet import greet\n\n\n'
        'class GreetTest(unittest.TestCase):\n'
        '    def test_greets_by_name(self):\n'
        '        self.assertEqual(greet("Ada"), "Hello, Ada!")\n\n\n'
        'if __name__ == "__main__":\n    unittest.main()\n'
    ),
}


class Failure(Exception):
    pass


def owned(pid, program):
    """Whether pid still runs the named program."""
    try:
        argv0 = Path(f'/proc/{pid}/cmdline').read_bytes().split(b'\0', 1)[0]
    except OSError:
        return False
    return Path(os.fsdecode(argv0)).name == program


class Sandbox:
    def __init__(self, directory):
        self.dir = Path(directory).absolute()
        self.socket = self.dir / 'tmux.sock'
        self.model = self.dir / 'model'
        self.logs = self.dir / 'logs'
        self.pids = self.dir / 'pids'
        self.settings = self.dir / 'sandbox.json'

    def tmux(self, *args, input=None):
        result = subprocess.run(['tmux', '-S', str(self.socket), *args], input=input,
                                text=True, capture_output=True)
        if result.returncode != 0:
            raise Failure(f'tmux {args[0]}: {result.stderr.strip() or "failed"}')
        return result.stdout

    def running(self):
        if not self.socket.exists():
            return False
        result = subprocess.run(['tmux', '-S', str(self.socket), 'has-session', '-t', TARGET],
                                capture_output=True)
        return result.returncode == 0

    def pane(self):
        fields = ['pane_pid', 'pane_dead', 'pane_dead_status', 'pane_dead_signal',
                  'cursor_x', 'cursor_y', 'pane_width', 'pane_height']
        line = self.tmux('display-message', '-p', '-t', TARGET,
                         '\t'.join(f'#{{{field}}}' for field in fields))
        return dict(zip(fields, line.rstrip('\n').split('\t')))

    def load(self):
        if not self.settings.exists():
            raise Failure(f'no sandbox in {self.dir}; run start first')
        return json.loads(self.settings.read_text())

    def require_session(self):
        if not self.running():
            raise Failure(f'no console session in {self.dir}; run start first')

    def model_pid(self):
        path = self.pids / 'scripted-strata'
        if path.exists():
            pid = int(path.read_text())
            if owned(pid, 'scripted-strata'):
                return pid
        return None

    def capture(self, history=False, ansi=False):
        args = ['capture-pane', '-p', '-t', TARGET]
        if history:
            args += ['-S', '-']
        if ansi:
            args.append('-e')
        return self.tmux(*args)

    def console_state(self, pane):
        if pane['pane_dead'] != '1':
            return f'running (pid {pane["pane_pid"]})'
        if pane['pane_dead_signal']:
            return f'exited on signal {pane["pane_dead_signal"]}'
        return f'exited with status {pane["pane_dead_status"]}'


def parse_size(text):
    match = re.fullmatch(r'(\d+)x(\d+)', text)
    if not match or min(int(match[1]), int(match[2])) < 10:
        raise Failure(f'size must look like 120x40, got {text!r}')
    return match[1], match[2]


def build():
    subprocess.run(['cargo', 'build', '--locked', '-q', '-p', 'playtime-chat', '--bin',
                    'playtime-chat', '-p', 'playtime-test-support', '--bin', 'scripted-strata'],
                   cwd=REPO, check=True)


def binaries():
    metadata = subprocess.run(['cargo', 'metadata', '--format-version', '1', '--no-deps'],
                              cwd=REPO, check=True, capture_output=True, text=True)
    target = Path(json.loads(metadata.stdout)['target_directory']) / 'debug'
    return target / 'playtime-chat', target / 'scripted-strata'


def client_args(arguments):
    return arguments[1:] if arguments[:1] == ['--'] else arguments


def show(sandbox, history=False, ansi=False, out=sys.stdout):
    lines = sandbox.capture(history, ansi).split('\n')
    if not ansi:
        lines = [line.rstrip() for line in lines]
    while lines and not lines[-1].strip():
        lines.pop()
    pane = sandbox.pane()
    print('\n'.join(lines), file=out)
    print(f'── {pane["pane_width"]}×{pane["pane_height"]}, cursor at row '
          f'{int(pane["cursor_y"]) + 1} column {int(pane["cursor_x"]) + 1}, console '
          f'{sandbox.console_state(pane)}', file=out)


def wait_for(sandbox, pattern, timeout, gone=False, history=False):
    deadline = time.monotonic() + timeout
    while True:
        if bool(pattern.search(sandbox.capture(history))) != gone:
            return
        if sandbox.pane()['pane_dead'] == '1':
            show(sandbox, history=True, out=sys.stderr)
            raise Failure('the console exited first')
        if time.monotonic() > deadline:
            show(sandbox, history, out=sys.stderr)
            raise Failure(f'timed out after {timeout} s')
        time.sleep(0.1)


def launch(sandbox, command, timeout):
    """Starts the console in the session's pane and waits for its prompt."""
    sandbox.tmux('clear-history', '-t', TARGET)
    sandbox.tmux('respawn-pane', '-k', '-t', TARGET, '--', *command)
    deadline = time.monotonic() + timeout
    while not PROMPT.search(sandbox.capture()) and sandbox.pane()['pane_dead'] != '1':
        if time.monotonic() > deadline:
            show(sandbox, history=True)
            raise Failure(f'no prompt after {timeout} s')
        time.sleep(0.1)
    show(sandbox, history=True)
    pane = sandbox.pane()
    if pane['pane_dead'] == '1' and (pane['pane_dead_signal'] or pane['pane_dead_status'] != '0'):
        raise Failure(f'the console {sandbox.console_state(pane)}')


def stop_console(sandbox, timeout=90):
    """Asks a running console to clean up and exit, as SIGTERM does."""
    pane = sandbox.pane()
    if pane['pane_dead'] == '1':
        return
    pid = int(pane['pane_pid'])
    if not owned(pid, 'playtime-chat'):
        return
    os.kill(pid, signal.SIGTERM)
    deadline = time.monotonic() + timeout
    while sandbox.pane()['pane_dead'] != '1':
        if time.monotonic() > deadline:
            raise Failure(f'the console was still cleaning up after {timeout} s')
        time.sleep(0.1)


def reset_model(model):
    """Removes the previous run's queue, requests and endpoint, and nothing else."""
    for directory, pattern in [(model / 'replies', r'\.?\d+\.json(\.used)?'),
                               (model / 'requests', r'\d+\.json')]:
        if directory.is_dir():
            for path in directory.iterdir():
                if re.fullmatch(pattern, path.name):
                    path.unlink()
    for name in ['endpoint', '.endpoint', 'requests.jsonl', 'model.lock.json']:
        (model / name).unlink(missing_ok=True)


def start(sandbox, args):
    if sandbox.running() or sandbox.model_pid():
        raise Failure(f'already running in {sandbox.dir}; stop it first')
    for tool in ['tmux', 'cargo']:
        if subprocess.run(['which', tool], capture_output=True).returncode != 0:
            raise Failure(f'missing {tool}')
    width, height = parse_size(args.size)
    if not args.no_build:
        build()
    client, server = binaries()
    for path in [sandbox.dir, sandbox.logs, sandbox.pids, sandbox.model, sandbox.dir / 'state']:
        path.mkdir(mode=0o700, parents=True, exist_ok=True)
    project = Path(args.project).absolute() if args.project else sandbox.dir / 'project'
    if not args.project and not project.exists():
        project.mkdir(mode=0o700)
        for name, text in SAMPLE.items():
            (project / name).write_text(text)

    reset_model(sandbox.model)
    with open(sandbox.logs / 'scripted-strata.log', 'ab') as log:
        process = subprocess.Popen([str(server), str(sandbox.model), '--pace-ms',
                                    str(args.pace_ms)], stdin=subprocess.DEVNULL, stdout=log,
                                   stderr=subprocess.STDOUT, start_new_session=True)
    (sandbox.pids / 'scripted-strata').write_text(str(process.pid))
    deadline = time.monotonic() + 10
    while not (sandbox.model / 'endpoint').exists():
        if process.poll() is not None:
            raise Failure(f'scripted-strata exited; see {sandbox.logs}/scripted-strata.log')
        if time.monotonic() > deadline:
            raise Failure('timed out waiting for scripted-strata')
        time.sleep(0.05)

    settings = {
        'client': str(client),
        'endpoint': (sandbox.model / 'endpoint').read_text().strip(),
        'lock': str(sandbox.model / 'model.lock.json'),
        'project': str(project),
        'state': str(sandbox.dir / 'state'),
        'preset': args.preset,
        'args': client_args(args.client_args),
    }
    sandbox.settings.write_text(json.dumps(settings, indent=2) + '\n')
    config = sandbox.dir / 'tmux.conf'
    config.write_text(CONFIG)
    # A placeholder holds the pane until the output log is attached.
    sandbox.tmux('-f', str(config), 'new-session', '-d', '-s', TARGET, '-x', width, '-y',
                 height, '--', 'sleep', 'infinity')
    # A manual size keeps an attached terminal from resizing the window.
    sandbox.tmux('resize-window', '-t', TARGET, '-x', width, '-y', height)
    sandbox.tmux('pipe-pane', '-o', '-t', TARGET,
                 f'cat >> {shlex.quote(str(sandbox.logs / "terminal.log"))}')
    launch(sandbox, command(settings), args.timeout)
    print(f'Watch it with: tmux -S {sandbox.socket} attach -r')


def command(settings, extra=()):
    preset = ['--preset', 'coding-host'] if settings['preset'] == 'coding-host' else []
    return [settings['client'], '--endpoint', settings['endpoint'], '--model-lock',
            settings['lock'], '--workspace', settings['project'], '--state-dir',
            settings['state'], *preset, *settings['args'], *extra]


def restart(sandbox, args):
    settings = sandbox.load()
    sandbox.require_session()
    if not args.no_build:
        build()
    stop_console(sandbox)
    launch(sandbox, command(settings, client_args(args.client_args)), args.timeout)


def stop(sandbox, args):
    if sandbox.running():
        stop_console(sandbox)
        sandbox.tmux('kill-server')
    pid = sandbox.model_pid()
    if pid:
        os.kill(pid, signal.SIGTERM)
        deadline = time.monotonic() + 5
        while owned(pid, 'scripted-strata') and time.monotonic() < deadline:
            time.sleep(0.05)
        if owned(pid, 'scripted-strata'):
            raise Failure(f'scripted-strata ({pid}) is still running')
    (sandbox.pids / 'scripted-strata').unlink(missing_ok=True)
    print(f'Stopped; {sandbox.dir} keeps the logs, requests and console state.')


def status(sandbox, args):
    settings = sandbox.load()
    console = sandbox.console_state(sandbox.pane()) if sandbox.running() else 'no session'
    pid = sandbox.model_pid()
    queue = sandbox.model / 'replies'
    waiting = sorted(p.name for p in queue.glob('*.json')) if queue.is_dir() else []
    log = sandbox.model / 'requests.jsonl'
    served = len(log.read_text().splitlines()) if log.exists() else 0
    print(f'console: {console}')
    print(f'model: {f"running (pid {pid})" if pid else "stopped"} at {settings["endpoint"]}')
    print(f'replies waiting: {len(waiting)}; requests served: {served}')
    print(f'preset: {settings["preset"]}; project: {settings["project"]}')
    print(f'console state: {settings["state"]}')
    print(f'watch: tmux -S {sandbox.socket} attach -r')


def type_text(sandbox, text):
    if text:
        # A buffer sends the exact bytes; send-keys would treat a final ';' as a
        # command separator. Without -p, the bytes arrive as typed keys.
        sandbox.tmux('load-buffer', '-b', 'sandbox-type', '-', input=text)
        sandbox.tmux('paste-buffer', '-d', '-b', 'sandbox-type', '-t', TARGET)


def send_keys(sandbox, names, gap):
    for index, name in enumerate(names):
        if index:
            # The console reads Escape followed within 40 ms by another byte as
            # an Alt-key sequence, so each key arrives on its own.
            time.sleep(gap)
        sandbox.tmux('send-keys', '-t', TARGET, KEYS.get(name.lower(), name))


def reply(sandbox, args):
    queue = sandbox.model / 'replies'
    if not queue.is_dir():
        raise Failure('no scripted model; run start first')
    if args.spec:
        spec = json.loads(Path(args.spec).read_text())
        if not isinstance(spec, dict) or not set(spec) <= SPEC_FIELDS:
            raise Failure(f'a reply is an object with only these fields: {sorted(SPEC_FIELDS)}')
    else:
        spec = {}
        for field in ['think', 'text', 'pace_ms']:
            if getattr(args, field) is not None:
                spec[field] = getattr(args, field)
        if args.calls:
            spec['calls'] = args.calls
        if args.end != 'completed':
            spec['end'] = args.end
        if args.usage:
            spec['usage'] = json.loads(args.usage)
    numbers = [int(m[1]) for p in queue.iterdir()
               if (m := re.fullmatch(r'(\d+)\.json(?:\.used)?', p.name))]
    number = max(numbers, default=0) + 1
    partial = queue / f'.{number:04}.json'
    partial.write_text(json.dumps(spec, ensure_ascii=False, indent=2) + '\n')
    partial.rename(queue / f'{number:04}.json')
    print(f'Queued reply {number:04}; {len(list(queue.glob("*.json")))} waiting.')


class AddCall(argparse.Action):
    """Keeps --exec, --read, --write and --call in the order given."""

    def __call__(self, parser, namespace, values, option_string=None):
        if option_string == '--exec':
            call = {'name': 'exec', 'arguments': {'command': values}}
        elif option_string == '--read':
            call = {'name': 'read_file', 'arguments': {'path': values}}
        elif option_string == '--write':
            call = {'name': 'write_file', 'arguments': {'path': values[0], 'content': values[1]}}
        else:
            try:
                call = {'name': values[0], 'arguments': json.loads(values[1])}
            except json.JSONDecodeError as error:
                parser.error(f'--call arguments must be JSON: {error}')
        namespace.calls = (namespace.calls or []) + [call]


def replies(sandbox, args):
    queue = sandbox.model / 'replies'
    for path in sorted(queue.iterdir()) if queue.is_dir() else []:
        match = re.fullmatch(r'(\d+)\.json(\.used)?', path.name)
        if match:
            summary = json.dumps(json.loads(path.read_text()), ensure_ascii=False)
            print(f'{match[1]}  {"used   " if match[2] else "waiting"}  {summary[:100]}')


def requests(sandbox, args):
    log = sandbox.model / 'requests.jsonl'
    entries = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
    if args.number is None:
        for entry in entries:
            print(f'{entry["request"]:>4}  {entry["reply"]:<16} {entry["input_items"]:>3} items  '
                  f'{entry["last_user"]}')
        return
    entry = next((e for e in entries if e['request'] == args.number), None)
    if entry is None:
        raise Failure(f'no request {args.number}')
    body = sandbox.model / 'requests' / f'{args.number:04}.json'
    print(json.dumps(entry, ensure_ascii=False, indent=2))
    print(f'body: {body}')
    if args.body:
        print(body.read_text())


def raw(sandbox, args):
    data = (sandbox.logs / 'terminal.log').read_bytes()[-args.bytes:]
    names = {'\n': '␊\n', '\r': '␍', '\x1b': '␛', '\t': '␉'}
    print(''.join(names.get(c) or (f'^{chr(ord(c) ^ 0x40)}' if ord(c) < 0x20 or c == '\x7f'
                                   else c) for c in data.decode('utf-8', 'replace')))


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--dir', default=os.environ.get('PLAYTIME_CHAT_SANDBOX')
                        or f'/tmp/playtime-chat-{os.getuid()}')
    commands = parser.add_subparsers(dest='command', required=True)

    p = commands.add_parser('start')
    p.add_argument('--preset', choices=['workshop', 'coding-host'], default='workshop')
    p.add_argument('--project', help='an existing project instead of the sample one')
    p.add_argument('--size', default='120x40')
    p.add_argument('--pace-ms', type=int, default=6, help='default delay between events')
    p.add_argument('--timeout', type=float, default=90)
    p.add_argument('--no-build', action='store_true')
    p.add_argument('client_args', nargs=argparse.REMAINDER)

    p = commands.add_parser('restart')
    p.add_argument('--timeout', type=float, default=90)
    p.add_argument('--no-build', action='store_true')
    p.add_argument('client_args', nargs=argparse.REMAINDER)

    commands.add_parser('stop')
    commands.add_parser('status')

    p = commands.add_parser('screen')
    p.add_argument('--all', action='store_true', help='include the history')
    p.add_argument('--ansi', action='store_true', help='keep colours and attributes')

    p = commands.add_parser('say')
    p.add_argument('text')
    p = commands.add_parser('type')
    p.add_argument('text')
    p = commands.add_parser('key')
    p.add_argument('keys', nargs='+')
    p.add_argument('--gap', type=float, default=0.1, help='seconds between keys')
    p = commands.add_parser('paste')
    source = p.add_mutually_exclusive_group(required=True)
    source.add_argument('text', nargs='?')
    source.add_argument('--file')

    p = commands.add_parser('wait')
    p.add_argument('text')
    p.add_argument('--timeout', type=float, default=30)
    p.add_argument('--gone', action='store_true', help='wait until TEXT is no longer shown')
    p.add_argument('--regex', action='store_true')
    p.add_argument('--all', action='store_true', help='search the history too')

    p = commands.add_parser('reply')
    p.add_argument('--think')
    p.add_argument('--text')
    p.add_argument('--exec', action=AddCall, dest='calls', metavar='COMMAND')
    p.add_argument('--read', action=AddCall, dest='calls', metavar='PATH')
    p.add_argument('--write', action=AddCall, dest='calls', nargs=2, metavar=('PATH', 'CONTENT'))
    p.add_argument('--call', action=AddCall, dest='calls', nargs=2, metavar=('NAME', 'JSON'))
    p.add_argument('--end', choices=['completed', 'incomplete', 'failed', 'stall', 'drop'],
                   default='completed')
    p.add_argument('--pace-ms', type=int)
    p.add_argument('--usage', help='JSON fields for the reported usage')
    p.add_argument('--spec', help='a whole reply as a JSON file')

    commands.add_parser('replies')
    p = commands.add_parser('requests')
    p.add_argument('number', type=int, nargs='?')
    p.add_argument('--body', action='store_true')
    p = commands.add_parser('resize')
    p.add_argument('size')
    p = commands.add_parser('raw')
    p.add_argument('--bytes', type=int, default=4000)
    commands.add_parser('attach')

    args = parser.parse_args()
    sandbox = Sandbox(args.dir)
    try:
        if args.command == 'start':
            start(sandbox, args)
        elif args.command == 'restart':
            restart(sandbox, args)
        elif args.command == 'stop':
            stop(sandbox, args)
        elif args.command == 'status':
            status(sandbox, args)
        elif args.command == 'reply':
            reply(sandbox, args)
        elif args.command == 'replies':
            replies(sandbox, args)
        elif args.command == 'requests':
            requests(sandbox, args)
        elif args.command == 'raw':
            raw(sandbox, args)
        elif args.command == 'attach':
            print(f'tmux -S {sandbox.socket} attach -r    # without -r you can type too')
        else:
            sandbox.require_session()
            if args.command == 'screen':
                show(sandbox, args.all, args.ansi)
            elif args.command == 'say':
                type_text(sandbox, args.text)
                time.sleep(0.1)
                send_keys(sandbox, ['Enter'], 0)
            elif args.command == 'type':
                type_text(sandbox, args.text)
            elif args.command == 'key':
                send_keys(sandbox, args.keys, args.gap)
            elif args.command == 'paste':
                text = Path(args.file).read_text() if args.file else args.text
                sandbox.tmux('load-buffer', '-b', 'sandbox-paste', '-', input=text)
                sandbox.tmux('paste-buffer', '-p', '-d', '-b', 'sandbox-paste', '-t', TARGET)
            elif args.command == 'wait':
                pattern = re.compile(args.text if args.regex else re.escape(args.text), re.MULTILINE)
                wait_for(sandbox, pattern, args.timeout, args.gone, args.all)
            elif args.command == 'resize':
                width, height = parse_size(args.size)
                sandbox.tmux('resize-window', '-t', TARGET, '-x', width, '-y', height)
    except Failure as error:
        print(f'chat-sandbox: {error}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
