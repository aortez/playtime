#!/usr/bin/env python3
"""Prepare and select project-owned development images for standalone chat.

Preparation is explicit. Normal launches only read private locks and definition
hashes; they never execute repository lifecycle hooks or rebuild an image.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile
import uuid

PACKAGE = Path(__file__).resolve().parent
MAX_LOCK = 65536
WORKSPACE_MOUNT = 'source=${localWorkspaceFolder},target=/work,type=bind'


def sha(data):
    return hashlib.sha256(data).hexdigest()


def private_directory(path):
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError(f'private caller-owned directory required: {path}')
    return path


def read_private(path):
    info = path.parent.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError('environment lock parent must be private and caller-owned')
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as source:
        info = os.fstat(source.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077 or info.st_size > MAX_LOCK:
            raise ValueError('environment lock must be private, regular, caller-owned and bounded')
        return json.loads(source.read(MAX_LOCK + 1))


def write_private(path, value):
    data = (json.dumps(value, sort_keys=True, indent=2) + '\n').encode()
    if len(data) > MAX_LOCK:
        raise ValueError('environment metadata exceeds 64 KiB')
    private_directory(path.parent)
    fd, temporary = tempfile.mkstemp(prefix='.environment-', dir=path.parent)
    try:
        with os.fdopen(fd, 'wb') as output:
            output.write(data)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
        fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def contained(root, path):
    path = path.resolve(strict=True)
    if not path.is_relative_to(root):
        raise ValueError('development environment paths must stay inside the project')
    return path


def opted_in(root):
    """Say whether the project asks for its own Playtime environment.

    Only a strict-JSON devcontainer.json with customizations.playtime opts in.
    Devcontainers written for other tools, often JSONC with comments, keep the
    general workshop they had before project environments existed.
    """
    path = root / '.devcontainer/devcontainer.json'
    if not path.exists() and not path.is_symlink():
        return False
    path = contained(root, path)
    if path.stat().st_size > MAX_LOCK:
        raise ValueError('devcontainer.json exceeds 64 KiB')
    data = path.read_bytes()
    try:
        config = json.loads(data)
    except ValueError:
        # A definition meant for Playtime must not fall back over a syntax error.
        if b'"playtime"' in data:
            raise ValueError('devcontainer.json mentions playtime but is not strict JSON') from None
        return False
    custom = config.get('customizations') if isinstance(config, dict) else None
    return isinstance(custom, dict) and 'playtime' in custom


def definition(workspace):
    root = workspace.resolve(strict=True)
    path = contained(root, root / '.devcontainer/devcontainer.json')
    if path.stat().st_size > MAX_LOCK:
        raise ValueError('devcontainer.json exceeds 64 KiB')
    config = json.loads(path.read_text())
    allowed = {'name', 'image', 'build', 'workspaceFolder', 'workspaceMount', 'remoteUser', 'customizations'}
    if not isinstance(config, dict) or set(config) - allowed:
        raise ValueError('unsupported devcontainer properties; this pilot supports only a single image or Dockerfile')
    if config.get('workspaceFolder', '/work') != '/work' or config.get('workspaceMount', WORKSPACE_MOUNT) != WORKSPACE_MOUNT:
        raise ValueError('Playtime requires the selected project mounted at /work')
    if ('image' in config) == ('build' in config):
        raise ValueError('select exactly one devcontainer image or build')
    custom = config.get('customizations', {})
    if not isinstance(custom, dict) or set(custom) - {'playtime'}:
        raise ValueError('unsupported devcontainer customizations')
    options = custom.get('playtime', {})
    if not isinstance(options, dict) or set(options) - {'inputs'}:
        raise ValueError('unsupported Playtime environment customization')
    inputs = options.get('inputs', [])
    if not isinstance(inputs, list) or any(not isinstance(p, str) or Path(p).is_absolute() for p in inputs):
        raise ValueError('environment inputs must be project-relative paths')
    selected = [path]
    command = None
    if 'build' in config:
        build = config['build']
        if not isinstance(build, dict) or set(build) - {'dockerfile', 'context', 'target', 'args'}:
            raise ValueError('unsupported devcontainer build properties')
        dockerfile = contained(root, path.parent / build['dockerfile'])
        context = contained(root, path.parent / build.get('context', '.'))
        if not dockerfile.is_file() or not context.is_dir():
            raise ValueError('Dockerfile must be a file and build context a directory')
        command = ['docker', 'build', '--file', str(dockerfile)]
        if 'target' in build:
            if not isinstance(build['target'], str) or not re.fullmatch(r'[A-Za-z0-9_.-]+', build['target']):
                raise ValueError('invalid Docker build target')
            command += ['--target', build['target']]
        arguments = build.get('args', {})
        if not isinstance(arguments, dict):
            raise ValueError('build args must be a string map')
        for key, value in arguments.items():
            if not re.fullmatch(r'[A-Za-z_][A-Za-z0-9_]*', key) or not isinstance(value, str) or len(value) > 4096:
                raise ValueError('invalid Docker build argument')
            command += ['--build-arg', f'{key}={value}']
        selected.append(dockerfile)
        for ignore in (context / '.dockerignore', Path(str(dockerfile) + '.dockerignore')):
            if ignore.exists():
                selected.append(contained(root, ignore))
        command.append(str(context))
    elif not isinstance(config['image'], str) or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_./:@-]{0,511}', config['image']):
        raise ValueError('invalid development image reference')
    selected.extend(contained(root, root / item) for item in inputs)
    files = set()
    for item in selected:
        candidates = item.rglob('*') if item.is_dir() else [item]
        for candidate in candidates:
            if candidate.is_symlink():
                raise ValueError('environment inputs cannot contain symbolic links')
            if candidate.is_file():
                files.add(contained(root, candidate))
            if len(files) > 512:
                raise ValueError('too many environment input files')
    hashes = {}
    remaining = 16 * 1024 * 1024
    for item in sorted(files):
        with item.open("rb") as source:
            data = source.read(remaining + 1)
        remaining -= len(data)
        if remaining < 0:
            raise ValueError('environment definition inputs exceed 16 MiB')
        hashes[str(item.relative_to(root))] = sha(data)
    return config, command, hashes


def project_store(workspace):
    root = workspace.resolve(strict=True)
    info = root.stat()
    key = sha(json.dumps([str(root), info.st_dev, info.st_ino]).encode())
    return Path.home() / '.local/state/playtime-environments' / key


def workspace_arguments(args):
    # Recognize chat's options, including prompts that contain what looks like a
    # flag. An unknown option is refused here, so each new chat option must be
    # added to these sets.
    values = {'--workspace', '--state-dir', '--session', '--recover-compaction', '--recover-interrupted',
              '--inspect-interrupted', '--prompt', '--prompt-file', '--completion-receipt', '--launch-id', '--context-tokens', '--wall-seconds', '--tokens-per-call',
              '--generated-tokens', '--summary-tokens', '--output-bytes', '--agent-prompt', '--preset'}
    switches = {'--json', '--stream', '--select-session', '--list-sessions', '--acknowledge-uncertain-tools'}
    parsed = {}
    i = 0
    while i < len(args):
        key, sep, value = args[i].partition('=')
        if key in values:
            if not sep:
                i += 1
                if i == len(args):
                    raise ValueError(f'missing value for {key}')
                value = args[i]
            if key in parsed:
                raise ValueError(f'duplicate option: {key}')
            parsed[key] = value
        elif key not in switches:
            raise ValueError(f'unsupported chat option: {key}')
        i += 1
    root = Path(parsed.get('--workspace', '.')).resolve(strict=True)
    sources = [parsed[k] for k in ('--session', '--recover-compaction', '--recover-interrupted', '--inspect-interrupted') if k in parsed]
    if len(sources) > 1:
        raise ValueError('select only one saved session operation')
    return root, Path(sources[0]) if sources else None


def resolve(args, package=PACKAGE, selection=None):
    root, source = workspace_arguments(args)
    if selection == 'general':
        return package / 'general.lock.json'
    if selection is not None:
        path = Path(selection).resolve(strict=True)
        if path.is_relative_to(root):
            raise ValueError('prepared environment locks must stay outside the project')
        lock = read_private(path)
        if lock.get('project_environment', {}).get('workspace') != str(root):
            raise ValueError('prepared environment belongs to another workspace')
        return path
    if source is not None and (source / 'config.json').exists():
        path = source / 'environment.lock.json'
        if path.is_symlink() or path.exists():
            path = path.absolute()
            if path.resolve().is_relative_to(root):
                raise ValueError('saved environment must stay outside the project')
            read_private(path)
            return path
        # Pre-environment sessions used the packaged general workshop.
        return package / 'general.lock.json'
    if not opted_in(root):
        return package / 'general.lock.json'
    try:
        _, _, hashes = definition(root)
    except ValueError as error:
        raise ValueError(f'{error}; pass --environment general to use the general workshop') from error
    index_path = project_store(root) / 'current.json'
    try:
        index = read_private(index_path)
    except FileNotFoundError as error:
        raise ValueError('project environment is not prepared; run --prepare-environment first, '
                         'or pass --environment general to use the general workshop') from error
    if index.get('schema_version') != 1 or not isinstance(index.get('lock'), str):
        raise ValueError('invalid prepared environment index')
    if index.get('inputs') != hashes:
        raise ValueError('project environment inputs changed; run --prepare-environment again, or explicitly resume the saved environment')
    lock_path = index_path.parent / index['lock']
    if lock_path.parent != index_path.parent or lock_path.name != index['lock']:
        raise ValueError('invalid prepared environment index')
    lock = read_private(lock_path)
    if lock.get('project_environment', {}).get('workspace') != str(root):
        raise ValueError('prepared environment belongs to another workspace')
    return lock_path


PROBE = r'''
import hashlib,json,os,pwd,shutil,subprocess,sys
versions={}
for name,args in {'python3':['--version'],'bash':['--version'],'git':['--version'],'rg':['--version'],'rustc':['--version'],'cargo':['--version'],'rustfmt':['--version'],'cargo-clippy':['--version'],'clang-format':['--version'],'go':['version'],'cc':['--version'],'cmake':['--version'],'west':['--version'],'node':['--version'],'make':['--version'],'ps':['--version']}.items():
    if shutil.which(name):
        result=subprocess.run([name,*args],capture_output=True,text=True,timeout=3)
        if result.returncode == 0: versions[name]=(result.stdout or result.stderr).splitlines()[0][:240]
print(json.dumps({'uid':os.getuid(),'gid':os.getgid(),'user':pwd.getpwuid(os.getuid()).pw_name,'python':sys.version_info[:2],'python_executable':os.path.realpath(sys.executable),'bridge_sha256':hashlib.sha256(open('/opt/playtime/tool.py','rb').read()).hexdigest(),'tools':versions,'home':os.environ.get('HOME')}))
'''


def probe(image):
    name = 'playtime-environment-' + uuid.uuid4().hex
    try:
        result = subprocess.run(['docker', 'run', '--rm', '--name', name, '--pull', 'never', '--network', 'none',
                                 '--read-only', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges',
                                 '--user', '1000:1000', '--cpus', '1', '--memory', '256m', '--pids-limit', '64',
                                 '--tmpfs', '/tmp:rw,nosuid,nodev,size=16m', '--entrypoint', '/usr/local/bin/python3.12',
                                 image, '-I', '-S', '-B', '-c', PROBE], capture_output=True, text=True, timeout=60)
        if result.returncode:
            raise RuntimeError('environment probe failed: ' + result.stderr[-2000:])
        return json.loads(result.stdout)
    finally:
        removed = subprocess.run(['docker', 'rm', '--force', name], capture_output=True, text=True, timeout=30)
        if removed.returncode and 'No such container' not in removed.stderr:
            raise RuntimeError(f'could not confirm probe cleanup for {name}: {removed.stderr[-1000:]}')


def image_identity(reference):
    image = json.loads(subprocess.check_output(['docker', 'image', 'inspect', reference], text=True))[0]
    if image['Os'] != 'linux' or image['Architecture'] != 'amd64' or image['Config'].get('Volumes') or image['Config'].get('OnBuild'):
        raise ValueError('prepared image must be linux/amd64 without implicit volumes or OnBuild hooks')
    if not re.fullmatch(r'sha256:[0-9a-f]{64}', image['Id']):
        raise ValueError('invalid Docker image identity')
    if any(key.startswith('io.playtime.') for key in (image['Config'].get('Labels') or {})):
        raise ValueError('project images cannot define Playtime ownership labels')
    return image['Id']


def prepare(workspace, package=PACKAGE):
    workspace = workspace.resolve(strict=True)
    if not opted_in(workspace):
        raise ValueError('to prepare a Playtime environment, add customizations.playtime '
                         'to .devcontainer/devcontainer.json')
    config, command, hashes = definition(workspace)
    store = private_directory(project_store(workspace))
    if store.resolve().is_relative_to(workspace):
        raise ValueError('environment records must stay outside the project')
    bridge = (package / 'tool.py').read_bytes()
    with tempfile.TemporaryDirectory(prefix='prepare-', dir=store) as temporary:
        staging = Path(temporary)
        if command:
            subprocess.run([*command[:-1], '--iidfile', str(staging/'base.id'), command[-1]], check=True)
            base = (staging/'base.id').read_text().strip()
        else:
            base = config['image']
        base = image_identity(base)
        base_tag = "playtime-project-base:" + base.removeprefix("sha256:")
        subprocess.run(["docker", "tag", base, base_tag], check=True)
        (staging/'tool.py').write_bytes(bridge)
        (staging/'Dockerfile').write_text(f'''FROM {base_tag}
USER 0:0
COPY tool.py /opt/playtime/tool.py
RUN chmod 0755 /opt/playtime && chmod 0444 /opt/playtime/tool.py && if [ ! -x /usr/local/bin/python3.12 ]; then test -x /usr/bin/python3.12 && cp -L /usr/bin/python3.12 /usr/local/bin/python3.12; fi
USER 1000:1000
''')
        subprocess.run(['docker','build','--iidfile',str(staging/'image.id'),str(staging)], check=True)
        image = image_identity((staging/'image.id').read_text().strip())
        observed = probe(image)
        if (observed['uid'] != 1000 or observed['gid'] != 1000 or observed['python'] != [3, 12]
                or observed['python_executable'] != '/usr/local/bin/python3.12' or observed['bridge_sha256'] != sha(bridge)):
            raise ValueError('development image does not satisfy the tool bridge contract')
        if config.get('remoteUser') not in (None, '1000', observed['user']):
            raise ValueError('remoteUser must identify uid 1000 in the development image')
        if any(name not in observed['tools'] for name in ('bash', 'git', 'rg')):
            raise ValueError('development image needs Bash, Git and ripgrep')
        if definition(workspace)[2] != hashes:
            raise ValueError('environment inputs changed during preparation; result not published')
        inventory = 'Verified development tools: ' + '; '.join(observed['tools'].values()) + '. '
        inventory += f"Container user: {observed['user']} (uid 1000). HOME={observed['home']}. Consult the project's .devcontainer/README.md for build and test commands."
        lock = {'schema':1,'os':'linux','architecture':'amd64','image_id':image,'build_sources':{'tool.py':sha(bridge)},
                'project_environment':{'schema_version':1,'workspace':str(workspace),'name':config.get('name','Project environment'),
                                       'base_image_id':base,'inputs':hashes,'inventory':inventory,'observed':observed}}
        name = 'environment-' + sha(json.dumps(lock,sort_keys=True).encode()) + '.lock.json'
        path = store / name
        if path.exists():
            if read_private(path) != lock: raise ValueError('existing environment lock differs')
        else:
            write_private(path, lock)
        # Retain an image name as well as its exact ID; normal Docker dangling-image
        # cleanup must not strand sessions after a later environment is prepared.
        subprocess.run(['docker','tag',image,'playtime-project:'+name.removeprefix('environment-').removesuffix('.lock.json')],check=True)
        write_private(store / 'current.json', {'schema_version':1,'lock':name,'inputs':hashes})
    print(f'Prepared project environment: {path}', file=sys.stderr)
    return path


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--workspace', type=Path, default=Path.cwd())
    args=parser.parse_args()
    print(prepare(args.workspace))


if __name__ == '__main__':
    os.umask(0o077)
    try:
        main()
    except Exception as error:
        print(f'Playtime environment: {error}', file=sys.stderr)
        sys.exit(1)
