"""Real same-user scopes and PTYs; enable with PLAYTIME_TEST_HOST_TOOLS=1."""
import json
import os
from pathlib import Path
import select
import shlex
import subprocess
import tempfile
import time
import unittest


@unittest.skipUnless(os.environ.get('PLAYTIME_TEST_HOST_TOOLS') == '1', 'explicit host process gate')
class HostToolsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='playtime-host-test-')
        self.root = Path(self.temp.name)
        self.project = self.root / 'project'; self.project.mkdir()
        self.state = self.root / 'state'; self.state.mkdir(mode=0o700)
        self.proc = subprocess.Popen(['/usr/bin/python3', '-I', '-S', '-u',
                                     str(Path(__file__).with_name('host_tools.py')),
                                     str(self.project), str(self.state), '4096'],
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        self.assertTrue(self.receive()['ready'])

    def receive(self):
        self.assertTrue(select.select([self.proc.stdout], [], [], 25)[0], 'host reply deadline')
        line = self.proc.stdout.readline()
        self.assertTrue(line, 'host helper exited without a reply')
        return json.loads(line)

    def send(self, value):
        self.proc.stdin.write(json.dumps(value) + '\n'); self.proc.stdin.flush()

    def call(self, name, **args):
        self.send({'name': name, 'arguments': args, 'timeout_seconds': 10})
        result = self.receive()
        self.assertNotIn('closed', result, result)
        return result

    def python(self, source):
        return '/usr/bin/python3 -u -c ' + shlex.quote(source)

    def tearDown(self):
        if not self.proc.stdin.closed:
            self.proc.stdin.close()
            reply = self.receive()
            self.assertTrue(reply['closed'], reply)
        self.assertEqual(self.proc.wait(timeout=25), 0)
        self.proc.stdout.close()
        self.assertFalse((self.state / 'active').exists())
        self.temp.cleanup()

    def test_host_paths_toolchains_and_special_file_errors(self):
        result = self.call('exec', command='pwd; command -v cargo; cargo --version')
        self.assertEqual(result['exit_code'], 0, result)
        self.assertIn(str(self.project), result['stdout'])
        target = self.root / 'outside.txt'
        self.call('write_file', path=str(target), content='host access\n')
        (self.project / 'link').symlink_to(target)
        self.assertEqual(self.call('read_file', path='link')['content'], 'host access\n')
        os.mkfifo(self.project / 'fifo')
        self.assertIn('regular file', self.call('read_file', path='fifo')['error'])
        self.assertIn('error', self.call('write_file', path='fifo', content='x'))
        result = self.call('exec', command=self.python('print("x" * 100000)'))
        self.assertTrue(result['output_truncated'])
        self.assertLessEqual(len(result['stdout']), 4096)

    def test_persistent_network_process_is_reachable_across_calls(self):
        source = 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); s.listen(); print(s.getsockname()[1],flush=True); c,_=s.accept(); c.sendall(b"host-network-ok"); c.close()'
        job = self.call('start_process', command=self.python(source))
        output = job['stdout']
        for _ in range(20):
            if output.strip(): break
            output += self.call('poll_process', process_id=job['process_id'], wait_seconds=.1)['stdout']
        port = int(output.strip())
        result = self.call('exec', command=self.python(f'import socket; print(socket.create_connection(("127.0.0.1",{port})).recv(100).decode())'))
        self.assertIn('host-network-ok', result['stdout'])
        self.assertEqual(self.call('poll_process')['processes'][0]['process_id'], job['process_id'])
        self.call('stop_process', process_id=job['process_id'])
        self.assertEqual(self.call('poll_process')['processes'], [])

    def test_pty_input_output_and_stop(self):
        job = self.call('start_process', command=self.python('import os; print("TTY="+str(os.isatty(0))); print("REPLY="+input()); input()'), tty=True)
        output = job['stdout']
        for _ in range(20):
            if 'TTY=True' in output: break
            output += self.call('poll_process', process_id=job['process_id'], wait_seconds=.1)['stdout']
        self.assertIn('TTY=True', output)
        self.call('write_process', process_id=job['process_id'], text='hello\n')
        output = ''
        for _ in range(20):
            output += self.call('poll_process', process_id=job['process_id'], wait_seconds=.1)['stdout']
            if 'REPLY=hello' in output: break
        self.assertIn('REPLY=hello', output)
        self.assertFalse(self.call('stop_process', process_id=job['process_id'])['running'])

    def test_cancel_stops_detached_descendants_and_preserves_other_job(self):
        other = self.call('start_process', command='sleep 60')
        pidfile = self.project / 'child.pid'
        code = 'import os,time; child=os.fork();\nif child == 0:\n os.setsid(); open("child.pid","w").write(str(os.getpid())); time.sleep(60)\nelse: time.sleep(60)'
        self.send({'name': 'exec', 'arguments': {'command': self.python(code)}, 'timeout_seconds': 60})
        until = time.monotonic() + 10
        while not pidfile.exists() and time.monotonic() < until: time.sleep(.02)
        self.assertTrue(pidfile.exists())
        pid = int(pidfile.read_text())
        self.send({'op': 'cancel'})
        self.assertIn('cancelled', self.receive()['error'])
        procpath = Path(f'/proc/{pid}/stat')
        self.assertTrue(not procpath.exists() or procpath.read_text().split()[2] == 'Z')
        self.assertTrue(self.call('poll_process', process_id=other['process_id'])['running'])
        self.call('stop_process', process_id=other['process_id'])

    def test_stubborn_process_is_killed_and_supervisor_remains_usable(self):
        code = 'import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); print("READY",flush=True); time.sleep(60)'
        job = self.call('start_process', command=self.python(code))
        output = job['stdout']
        while 'READY' not in output:
            output += self.call('poll_process', process_id=job['process_id'], wait_seconds=.1)['stdout']
        started = time.monotonic()
        result = self.call('stop_process', process_id=job['process_id'])
        self.assertFalse(result['running'])
        self.assertLess(time.monotonic()-started, 8)
        self.assertEqual(self.call('exec', command='printf still-usable')['stdout'], 'still-usable')

    def test_exit_kills_multiple_stubborn_scopes_under_one_deadline(self):
        for _ in range(3):
            code = 'import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); print("READY",flush=True); time.sleep(60)'
            job = self.call('start_process', command=self.python(code))
            output = job['stdout']
            while 'READY' not in output:
                output += self.call('poll_process', process_id=job['process_id'], wait_seconds=.1)['stdout']
        started = time.monotonic()
        self.proc.stdin.close()
        self.assertTrue(self.receive()['closed'])
        self.assertLess(time.monotonic()-started, 10)
        self.assertEqual(self.proc.wait(timeout=5), 0)

    def test_timeout_and_control_eof_cleanup(self):
        result = self.call('exec', command='sleep 60', timeout_seconds=.1)
        self.assertIn('timed out', result['error'])
        self.call('start_process', command='sleep 60')
        units = json.loads((self.state / 'active').read_text())['units']
        self.proc.stdin.close()
        self.assertTrue(self.receive()['closed'])
        self.assertEqual(self.proc.wait(timeout=25), 0)
        for unit in units:
            result = subprocess.run(['systemctl', '--user', 'is-active', unit], capture_output=True, text=True)
            self.assertNotEqual(result.stdout.strip(), 'active')

    def test_invalid_exec_timeout_is_refused_before_the_command_starts(self):
        for timeout in ('30s', None):
            result = self.call('exec', command='touch started', timeout_seconds=timeout)
            self.assertIn('error', result)
            time.sleep(.5)
            self.assertFalse((self.project / 'started').exists(), timeout)
        self.assertEqual(self.call('exec', command='true')['exit_code'], 0)


if __name__ == '__main__':
    unittest.main()
