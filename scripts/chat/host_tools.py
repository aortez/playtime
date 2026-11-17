"""Trusted same-user host tools. Each command owns an explicit systemd user scope.

The JSON control stream is private to playtime-chat. EOF stops all owned scopes;
no model argument chooses a unit, executable helper, identity, or cleanup target.
This is process supervision, not a filesystem or network sandbox.
"""
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import selectors
import stat
import subprocess
import sys
import termios
import time
import uuid

MAX_REQUEST = 300_000
MAX_JOBS = 8


class HostTools:
    def __init__(self, root, state, output_bytes):
        self.root = Path(root)
        self.state = Path(state)
        self.cap = output_bytes
        self.selector = selectors.DefaultSelector()
        self.selector.register(sys.stdin.fileno(), selectors.EVENT_READ, None)
        self.input = bytearray()
        self.jobs = {}
        self.pending = None
        self.eof = False
        self.units = []
        self.owner = uuid.uuid4().hex
        self.fence = self.state / 'active'
        self.record = {'profile': 'host-coding-v1', 'owner': self.owner, 'units': self.units}
        self.publish(create=True)

    def publish(self, create=False):
        data = (json.dumps(self.record) + '\n').encode()
        if create:
            fd = os.open(self.fence, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, 'wb') as target:
                target.write(data)
                target.flush()
                os.fsync(target.fileno())
        else:
            temporary = self.state / ('host-' + self.owner + '.tmp')
            fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, 'wb') as target:
                target.write(data)
                target.flush()
                os.fsync(target.fileno())
            os.replace(temporary, self.fence)
        fd = os.open(self.state, os.O_DIRECTORY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)

    def reply(self, value):
        print(json.dumps(value), flush=True)

    def path(self, value):
        path = Path(value)
        return path if path.is_absolute() else self.root / path

    def open_job(self, args, persistent):
        if len(self.jobs) >= MAX_JOBS:
            raise ValueError('process limit reached; stop an existing process first')
        job_id = uuid.uuid4().hex
        unit = 'playtime-host-' + job_id + '.scope'
        ready = self.state / ('ready-' + job_id)
        # Reserve before dispatch. A controller/helper crash retains the unit names.
        self.units.append(unit)
        self.publish()
        command = ['/usr/bin/systemd-run', '--user', '--scope', '--quiet',
                   '--unit=' + unit, '--property=KillMode=control-group', '--',
                   '/usr/bin/python3', '-I', '-S', '-c',
                   'import os,sys; open(sys.argv[1],"x").close(); os.execv("/bin/bash", ["bash","--noprofile","--norc","-o","pipefail","-c",sys.argv[2]])',
                   str(ready), args['command']]
        tty = bool(args.get('tty', False))
        slave = None
        master = None
        try:
            if tty:
                master, slave = pty.openpty()
                fcntl.ioctl(slave, termios.TIOCSWINSZ, b'\x18\x00\x78\x00\x00\x00\x00\x00')
                # This supervisor is single-threaded. Establish a controlling TTY
                # before systemd-run moves the child into its owned scope.
                def terminal():
                    os.setsid()
                    fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
                process = subprocess.Popen(command, cwd=self.path(args.get('cwd', '.')),
                                           stdin=slave, stdout=slave, stderr=slave,
                                           preexec_fn=terminal)
                stdin = master
                streams = [(master, 'stdout')]
            else:
                process = subprocess.Popen(command, cwd=self.path(args.get('cwd', '.')),
                                           stdin=subprocess.PIPE if persistent else subprocess.DEVNULL,
                                           stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                           start_new_session=True)
                stdin = process.stdin
                streams = [(process.stdout, 'stdout'), (process.stderr, 'stderr')]
        except Exception:
            self.units.remove(unit)
            self.publish()
            if master is not None:
                os.close(master)
            raise
        finally:
            if slave is not None:
                os.close(slave)
        job = {'id': job_id, 'unit': unit, 'process': process, 'stdin': stdin,
               'tty': tty, 'stdout': bytearray(), 'stderr': bytearray(),
               'truncated': False, 'streams': [], 'persistent': persistent, 'stopped': False, 'admission_uncertain': True}
        self.jobs[job_id] = job
        for stream, name in streams:
            fd = stream if isinstance(stream, int) else stream.fileno()
            os.set_blocking(fd, False)
            self.selector.register(fd, selectors.EVENT_READ, (job_id, name, stream))
            job['streams'].append(fd)
        # The trusted child marks readiness before executing model command text.
        # Wait for admission to settle before accepting a cancellation/stop.
        until = time.monotonic() + 5
        while not ready.exists() and process.poll() is None and time.monotonic() < until:
            time.sleep(.005)
        if ready.exists():
            job['admission_uncertain'] = False
            ready.unlink()
        elif process.poll() is None:
            raise RuntimeError('host scope admission timed out; cleanup required')
        elif process.returncode >= 0:
            job['admission_uncertain'] = False
        return job

    def drain(self, timeout=0):
        for key, _ in self.selector.select(timeout):
            if key.data is None:
                chunk = os.read(key.fd, 65536)
                if not chunk:
                    self.eof = True
                    self.selector.unregister(key.fd)
                else:
                    self.input.extend(chunk)
                    if len(self.input) > MAX_REQUEST:
                        raise ValueError('host request exceeds bound')
                continue
            job_id, name, stream = key.data
            job = self.jobs[job_id]
            try:
                chunk = os.read(key.fd, 65536)
            except BlockingIOError:
                continue
            except OSError as error:
                if error.errno != errno.EIO or not job['tty']:
                    raise
                chunk = b''
            if not chunk:
                self.selector.unregister(key.fd)
                job['streams'].remove(key.fd)
                if isinstance(stream, int):
                    os.close(stream)
                    job['stdin'] = None
                else:
                    stream.close()
                continue
            buffer = job[name]
            buffer.extend(chunk)
            if len(buffer) > self.cap:
                del buffer[:len(buffer) - self.cap]
                job['truncated'] = True

    def stopped_scope(self, unit, deadline):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return False
        state = subprocess.run(['/usr/bin/systemctl', '--user', 'show', unit,
                                '--property=ActiveState', '--property=LoadState'],
                               capture_output=True, text=True, timeout=min(2, remaining))
        observed = dict(line.split('=', 1) for line in state.stdout.splitlines() if '=' in line)
        return (observed.get('LoadState') == 'not-found'
                or observed.get('ActiveState') in ('inactive', 'failed'))

    def stop_scopes(self, units):
        if not units:
            return
        deadline = time.monotonic() + 8
        # Request termination without blocking on systemd's much longer default
        # stop timeout. Every scope shares this grace and verification deadline.
        subprocess.run(['/usr/bin/systemctl', '--user', '--no-ask-password',
                        'stop', '--no-block', *units],
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=2)
        grace = time.monotonic() + 1
        pending = list(units)
        killed = False
        while pending and time.monotonic() < deadline:
            pending = [unit for unit in pending if not self.stopped_scope(unit, deadline)]
            if pending and not killed and time.monotonic() >= grace:
                subprocess.run(['/usr/bin/systemctl', '--user', 'kill', '--kill-whom=all',
                                '--signal=KILL', *pending], stdout=subprocess.DEVNULL,
                               stderr=subprocess.PIPE, timeout=min(2, max(.01, deadline-time.monotonic())))
                killed = True
            if pending:
                self.drain(.02)
        if pending:
            raise RuntimeError('could not confirm host scope cleanup: ' + ', '.join(pending))

    def stop(self, job, already_stopped=False):
        if job['stopped']:
            return
        if not already_stopped:
            self.stop_scopes([job['unit']])
        elif not self.stopped_scope(job['unit'], time.monotonic() + 2):
            raise RuntimeError('could not confirm host scope cleanup: ' + job['unit'])
        try:
            job['process'].wait(timeout=3)
        except subprocess.TimeoutExpired:
            # systemd-run is our direct child; kill only this known launcher.
            job['process'].kill()
            job['process'].wait(timeout=3)
        until = time.monotonic() + 1
        while job['streams'] and time.monotonic() < until:
            self.drain(.01)
        if job['streams']:
            raise RuntimeError('host command retained output descriptors after scope cleanup')
        if job['admission_uncertain']:
            raise RuntimeError('host scope admission unresolved; private fence retained: ' + job['unit'])
        job['stopped'] = True
        if job['stdin'] is not None and not isinstance(job['stdin'], int):
            job['stdin'].close()
            job['stdin'] = None
        self.units.remove(job['unit'])
        self.publish()

    def snapshot(self, job, consume=True):
        self.drain(0)
        value = {'process_id': job['id'], 'running': job['process'].poll() is None,
                 'exit_code': job['process'].poll(), 'tty': job['tty'],
                 'stdout': bytes(job['stdout']).decode('utf-8', errors='replace'),
                 'stderr': bytes(job['stderr']).decode('utf-8', errors='replace'),
                 'output_truncated': job['truncated']}
        if consume:
            job['stdout'].clear()
            job['stderr'].clear()
            job['truncated'] = False
        return value

    def file_tool(self, name, args):
        path = self.path(args['path'])
        if name == 'read_file':
            fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
            with os.fdopen(fd, 'rb') as source:
                if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
                    raise ValueError('read_file requires a regular file')
                offset = args.get('offset_bytes', 0)
                source.seek(offset)
                data = source.read(self.cap + 1)
            truncated = len(data) > self.cap
            data = data[:self.cap]
            # Avoid splitting the last UTF-8 character when slicing by bytes.
            while True:
                try:
                    content = data.decode('utf-8')
                    break
                except UnicodeDecodeError as error:
                    if truncated and error.end == len(data) and len(data) - error.start <= 3:
                        data = data[:error.start]
                    else:
                        raise ValueError('read_file requires UTF-8 text') from error
            return {'path': str(path), 'content': content, 'offset_bytes': offset,
                    'next_offset_bytes': offset + len(data), 'truncated': truncated}
        path.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_NONBLOCK, 0o666)
        with os.fdopen(fd, 'wb') as target:
            if not stat.S_ISREG(os.fstat(target.fileno()).st_mode):
                raise ValueError('write_file requires a regular file')
            if args.get('append', False):
                target.seek(0, os.SEEK_END)
            else:
                target.truncate(0)
            data = args['content'].encode('utf-8')
            target.write(data)
        return {'path': str(path), 'bytes_written': len(data)}

    def request(self, request):
        op = request.get('op')
        if op == 'cancel':
            if self.pending:
                job, _, kind = self.pending
                if kind == 'exec':
                    self.stop(job)
                result = self.snapshot(job)
                result['error'] = 'host tool cancelled'
                self.reply(result)
                self.pending = None
                if kind == 'exec':
                    del self.jobs[job['id']]
            return
        if self.pending:
            raise ValueError('host request while another is pending')
        if op == 'shutdown':
            self.eof = True
            return
        name, args = request['name'], request['arguments']
        if name in ('read_file', 'write_file'):
            self.reply(self.file_tool(name, args))
        elif name in ('exec', 'start_process'):
            job = self.open_job(args, name == 'start_process')
            if name == 'exec':
                seconds = min(float(args.get('timeout_seconds', request['timeout_seconds'])), request['timeout_seconds'])
                self.pending = (job, time.monotonic() + max(.001, seconds), 'exec')
            else:
                self.reply(self.snapshot(job))
        elif name == 'poll_process' and 'process_id' not in args:
            self.reply({'processes': [{'process_id': job['id'], 'running': job['process'].poll() is None,
                                       'exit_code': job['process'].poll(), 'tty': job['tty']}
                                      for job in self.jobs.values() if job['persistent']]})
        else:
            job = self.jobs.get(args['process_id'])
            if job is None or not job['persistent']:
                raise ValueError('unknown process_id in this live CLI session')
            if name == 'stop_process':
                self.stop(job)
                self.reply(self.snapshot(job))
                del self.jobs[job['id']]
            elif name == 'poll_process':
                self.pending = (job, time.monotonic() + min(10, max(0, float(args.get('wait_seconds', 0)))), 'poll')
            elif name == 'write_process':
                if job['stdin'] is None or job['process'].poll() is not None:
                    raise ValueError('process input is closed')
                if args.get('close_stdin', False) and job['tty']:
                    raise ValueError('use a terminal EOF character for a TTY')
                fd = job['stdin'] if isinstance(job['stdin'], int) else job['stdin'].fileno()
                os.set_blocking(fd, False)
                data = args['text'].encode()
                count = os.write(fd, data)
                if args.get('close_stdin', False):
                    job['stdin'].close()
                    job['stdin'] = None
                self.reply({'process_id': job['id'], 'bytes_written': count,
                            'remaining_bytes': len(data) - count})
            else:
                raise ValueError('unsupported host tool')

    def run(self):
        self.reply({'ready': True, 'profile': 'host-coding-v1'})
        while not self.eof:
            self.drain(.02)
            while b'\n' in self.input and not self.eof:
                line, _, rest = self.input.partition(b'\n')
                self.input = bytearray(rest)
                try:
                    self.request(json.loads(line))
                except (OSError, ValueError) as error:
                    self.reply({'error': str(error)})
            if self.pending:
                job, deadline, kind = self.pending
                exited = job['process'].poll() is not None
                ready = exited or time.monotonic() >= deadline
                if kind == 'poll':
                    ready |= bool(job['stdout'] or job['stderr'])
                if ready:
                    if kind == 'exec' or exited:
                        self.stop(job)
                    value = self.snapshot(job)
                    if kind == 'exec' and not exited:
                        value['error'] = 'host command timed out'
                    self.reply(value)
                    self.pending = None
                    if kind == 'exec':
                        del self.jobs[job['id']]
            for job in self.jobs.values():
                if job['persistent'] and job['process'].poll() is not None and not job['stopped']:
                    self.stop(job)

    def cleanup(self):
        errors = []
        if self.units:
            try:
                self.stop_scopes(list(self.units))
            except Exception as error:
                errors.append(str(error))
        for job in self.jobs.values():
            try:
                self.stop(job, already_stopped=True)
            except Exception as error:
                errors.append(str(error))
        if self.units:
            errors.append('unresolved host scopes: ' + ', '.join(self.units))
        if not errors:
            if json.loads(self.fence.read_text()).get('owner') != self.owner:
                raise RuntimeError('host cleanup fence changed')
            self.fence.unlink()
            fd = os.open(self.state, os.O_DIRECTORY)
            os.fsync(fd)
            os.close(fd)
        return errors


def main():
    host = HostTools(sys.argv[1], sys.argv[2], int(sys.argv[3]))
    errors = []
    try:
        host.run()
    except Exception as error:
        errors.append(str(error))
    finally:
        errors.extend(host.cleanup())
    try:
        host.reply({'closed': not errors, 'errors': errors})
    except BrokenPipeError:
        pass
    return bool(errors)


if __name__ == '__main__':
    sys.exit(main())
