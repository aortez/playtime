#!/usr/bin/env python3
"""Fast terminal acceptance against the production UI's offline driver.

Build with cargo build -p playtime-chat --locked, then run this file. No Docker,
network, inference, terminal Python dependencies, or desktop settings required.
PLAYTIME_CONSOLE_BIN selects the separately installed candidate.
"""
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import tempfile
import termios
import time
import unittest

BINARY = Path(os.environ.get('PLAYTIME_CONSOLE_BIN', 'target/debug/playtime-console')).resolve()


class Terminal:
    def __init__(self, root, *args):
        self.master, self.slave = pty.openpty()
        self.original = termios.tcgetattr(self.slave)
        self.resize(38, 18)
        self.log = Path(root) / 'actions.jsonl'
        self.output = bytearray()
        self.process = subprocess.Popen(
            [str(BINARY), '--actions', str(self.log), *([] if '--tick-ms' in args else ['--tick-ms', '8']), *args],
            stdin=self.slave, stdout=self.slave, stderr=self.slave,
            env={**os.environ, 'TERM': 'xterm-256color'}, start_new_session=True)
        try:
            self.wait_for(b'Inline composer')
        except BaseException:
            self.process.terminate()
            self.process.wait(timeout=3)
            os.close(self.master)
            os.close(self.slave)
            raise

    def resize(self, columns, rows):
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack('HHHH', rows, columns, 0, 0))
        if hasattr(self, 'process'):
            self.process.send_signal(signal.SIGWINCH)

    def pump(self, seconds=.08):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            if select.select([self.master], [], [], .01)[0]:
                data = os.read(self.master, 65536)
                self.output.extend(data)
                if b'\x1b[6n' in data:
                    os.write(self.master, b'\x1b[1;1R')

    def send(self, data):
        os.write(self.master, data.encode() if isinstance(data, str) else data)
        self.pump()

    def paste(self, text):
        self.send('\x1b[200~' + text + '\x1b[201~')

    def wait_for(self, text, seconds=4):
        end = time.monotonic() + seconds
        while text not in self.output:
            if time.monotonic() > end:
                raise AssertionError(f'terminal did not emit {text!r}: {self.output[-1500:]!r}')
            self.pump()

    def actions(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def wait_submitted(self, count):
        deadline = time.monotonic() + 3
        while len(self.submitted()) < count:
            if time.monotonic() > deadline:
                raise AssertionError(f'missing submission: {self.actions()!r}')
            self.pump()

    def submitted(self):
        return [a['text'] for a in self.actions() if a['type'] == 'submit']

    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
        end = time.monotonic() + 3
        while self.process.poll() is None and time.monotonic() < end:
            self.pump()
        if self.process.poll() is None:
            self.process.kill()
            self.process.wait()
            raise AssertionError('console did not exit after SIGTERM')
        self.pump()
        restored = termios.tcgetattr(self.slave)
        os.close(self.master)
        os.close(self.slave)
        if restored != self.original:
            raise AssertionError('terminal attributes not restored')
        if b'\x1b[?1049h' in self.output:
            raise AssertionError('console entered alternate screen')
        if b'\x1b[?2004l' not in self.output:
            raise AssertionError('bracketed paste was not disabled')
        if self.process.returncode != 0:
            raise AssertionError(f'console exit {self.process.returncode}: {self.output[-1500:]!r}')


class ConsolePtyTests(unittest.TestCase):
    def terminal(self, *args):
        root = tempfile.TemporaryDirectory(prefix='playtime-console-pty-')
        self.addCleanup(root.cleanup)
        terminal = Terminal(root.name, *args)
        self.addCleanup(terminal.close)
        return terminal

    def test_wrapped_backspace_unicode_paste_resize_and_history(self):
        t = self.terminal()
        text = 'wrapped ' + 'abcdefghij ' * 8 + '界 café🦀'
        t.send(text)
        t.send(b'\x7f\x7f')
        expected = text[:-2]
        t.paste('\r\nfn main() {\r\n    ok();\r\n}\n/stop is pasted data')
        expected += '\nfn main() {\n    ok();\n}\n/stop is pasted data'
        self.assertEqual(t.submitted(), [])
        t.resize(23, 12)
        t.pump()
        t.resize(90, 30)
        t.send(b'\x1b[D\x7fX')
        expected = expected[:-2] + 'X' + expected[-1:]
        t.send(b'\r')
        t.wait_submitted(1)
        self.assertEqual(t.submitted(), [expected])
        t.wait_for(b'Fixture complete')
        t.pump(.1)
        t.send('kept draft')
        t.send(b'\x1b[1;3A')  # Alt-Up: history
        t.send(b'\x1b[1;3B')  # Alt-Down: restore draft
        t.send(b'\r')
        t.wait_submitted(2)
        self.assertEqual(t.submitted(), [expected, 'kept draft'])

    def test_artifact_actions_are_explicit_and_never_submit_a_model_prompt(self):
        t = self.terminal()
        t.resize(100, 30)
        t.paste('/artifact retain "screen test.png" --caption "Screen check"')
        self.assertEqual(t.actions(), [])
        t.send(b'\r')
        t.wait_for(b'--artifact-session')
        self.assertEqual(t.submitted(), [])
        self.assertEqual(t.actions(), [{'type': 'artifact', 'text': {
            'operation': 'retain', 'file': 'screen test.png', 'caption': 'Screen check', 'source': None}}])
        t.paste('/artifacts')
        t.send(b'\r')
        self.assertEqual(t.actions()[-1], {'type': 'artifact', 'text': {'operation': 'list'}})

    def test_streaming_never_submits_draft_and_stop_keeps_it(self):
        t = self.terminal('--tick-ms', '25')
        t.send('first')
        t.send(b'\r')
        t.paste('follow-up\n界 and e\u0301 👩\u200d💻')
        t.send(b'\x7f')  # Delete a complete grapheme cluster.
        t.send(b'\r')
        self.assertEqual(t.submitted(), ['first'])
        t.resize(17, 9)
        t.send(b'\x03')
        t.wait_for(b'Fixture stopped')
        t.resize(80, 24)
        t.send(b'\r')
        t.wait_submitted(2)
        self.assertEqual(t.submitted(), ['first', 'follow-up\n界 and e\u0301 '])
        self.assertIn({'type': 'stop'}, t.actions())

    def test_newline_undo_delete_and_clean_idle_exit(self):
        t = self.terminal()
        t.send('abc')
        t.send(b'\x1b[D\x1b[3~')
        t.send(b'\x1a')  # Ctrl-Z undo
        t.send(b'\x19')  # Ctrl-Y redo
        t.send(b'\x0a')  # Ctrl-J: portable newline
        t.send('second')
        t.send(b'\r')
        t.wait_submitted(1)
        self.assertEqual(t.submitted(), ['ab\nsecond'])
        t.send(b'\x03')
        t.wait_for(b'Fixture stopped')
        t.send(b'\x04')
        self.assertIn({'type': 'quit'}, t.actions())

    def test_enter_buffered_behind_output_cannot_submit_after_completion(self):
        with tempfile.TemporaryDirectory(prefix='playtime-console-backpressure-') as root:
            replay = Path(root) / 'replay.jsonl'
            replay.write_text(json.dumps({'type': 'notice', 'text': 'output row\n' * 2000}) + '\n')
            t = self.terminal('--replay', str(replay), '--tick-ms', '100')
            os.write(t.master, b'first\r')
            # Let output fill the PTY while the driver queues its completion.
            # Input must keep its original readiness even if rendering drains later.
            time.sleep(.3)
            os.write(t.master, b'\x1b[200~buffered draft\x1b[201~\r')
            time.sleep(.1)
            t.pump(2)
            self.assertEqual(t.submitted(), ['first'])
            t.send(b'\r')
            t.wait_submitted(2)
            self.assertEqual(t.submitted(), ['first', 'buffered draft'])

    def test_cancellation_reaches_driver_while_terminal_output_is_blocked(self):
        with tempfile.TemporaryDirectory(prefix='playtime-console-cancel-') as root:
            replay = Path(root) / 'replay.jsonl'
            replay.write_text('\n'.join(json.dumps(event) for event in [
                {'type': 'notice', 'text': 'output row\n' * 2000},
                {'type': 'notice', 'text': 'UNEXPECTED_POST_CANCEL_OUTPUT'},
            ]) + '\n')
            t = self.terminal('--replay', str(replay), '--tick-ms', '1000')
            os.write(t.master, b'first\r')
            time.sleep(.3)
            os.write(t.master, b'\x03')
            # The next replay event would be queued while stdout is blocked if
            # cancellation waited for the renderer to process the key.
            time.sleep(1.1)
            t.pump(1)
            self.assertNotIn(b'UNEXPECTED_POST_CANCEL_OUTPUT', t.output)
            self.assertEqual(t.submitted(), ['first'])

    def test_sigterm_restores_terminal_and_leaves_unsent_draft_visible(self):
        t = self.terminal('--autoplay')
        t.send('unsent sentinel')
        t.process.terminate()
        t.wait_for(b'Unsent draft')
        self.assertEqual(t.submitted(), [])

    def test_terminal_hangup_exits_without_waiting_for_an_input_thread(self):
        with tempfile.TemporaryDirectory(prefix='playtime-console-hangup-') as root:
            t = Terminal(root, '--autoplay')
            os.close(t.master)
            t.process.send_signal(signal.SIGHUP)
            try:
                t.process.wait(timeout=3)
            finally:
                if t.process.poll() is None:
                    t.process.kill()
                    t.process.wait()
                os.close(t.slave)

    def test_plain_and_json_replay_are_bounded_and_have_no_terminal_controls(self):
        result = subprocess.run([str(BINARY), '--plain'], capture_output=True, check=True, timeout=3)
        self.assertNotIn(b'\x1b', result.stdout)
        for text in [b'Assistant', b'no example target', b'omitted from preview', b'truncated upstream', b'usage unavailable']:
            self.assertIn(text, result.stdout)
        result = subprocess.run([str(BINARY), '--plain', '--json'], capture_output=True, check=True, timeout=3)
        for line in result.stdout.splitlines():
            self.assertEqual(json.loads(line)['console_observation_version'], 1)


if __name__ == '__main__':
    unittest.main(verbosity=2)
