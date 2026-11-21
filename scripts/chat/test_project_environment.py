"""Project image admission/selection fixtures; never build containers or use SSH."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('project_environment', Path(__file__).with_name('project_environment.py'))
env = importlib.util.module_from_spec(spec)
spec.loader.exec_module(env)


class Environments(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.workspace = self.root / 'project'
        self.workspace.mkdir()
        self.package = self.root / 'package'
        self.package.mkdir()
        (self.package/'general.lock.json').write_text('{}')
        (self.workspace/'.devcontainer').mkdir()
        (self.workspace/'Dockerfile').write_text('FROM fixture\n')
        (self.workspace/'deps.lock').write_text('one\n')
        self.config = {'name':'fixture','build':{'dockerfile':'../Dockerfile','context':'..'},
                       'workspaceFolder':'/work','customizations':{'playtime':{'inputs':['deps.lock']}}}
        self.write_config()
        self.home = patch.object(Path, 'home', return_value=self.root)
        self.home.start()
        self.args = ['--workspace',str(self.workspace)]

    def tearDown(self):
        self.home.stop()
        self.temp.cleanup()

    def write_config(self):
        (self.workspace/'.devcontainer/devcontainer.json').write_text(json.dumps(self.config))

    def publish(self):
        _, _, hashes = env.definition(self.workspace)
        store = env.private_directory(env.project_store(self.workspace))
        path = store/'environment-fixture.lock.json'
        lock = {'project_environment':{'schema_version':1,'workspace':str(self.workspace)}}
        env.write_private(path,lock)
        env.write_private(store/'current.json',{'schema_version':1,'lock':path.name,'inputs':hashes})
        return path

    def test_declared_dependency_changes_require_preparation_but_source_edits_do_not(self):
        with self.assertRaisesRegex(ValueError, 'not prepared'):
            env.resolve(self.args,self.package)
        path = self.publish()
        self.assertEqual(env.resolve(self.args,self.package),path)
        (self.workspace/'source.rs').write_text('fn main() {}')
        self.assertEqual(env.resolve(self.args,self.package),path)
        (self.workspace/'deps.lock').write_text('two\n')
        with self.assertRaisesRegex(ValueError, 'inputs changed'):
            env.resolve(self.args,self.package)
        self.assertEqual(env.resolve(self.args,self.package,selection='general'),self.package/'general.lock.json')

    def test_resume_uses_the_saved_environment_even_after_project_definition_changes(self):
        path = self.publish()
        session = env.private_directory(self.root/'session')
        env.write_private(session/'config.json',{})
        env.write_private(session/'environment.lock.json',env.read_private(path))
        self.config['features'] = {'unsupported':{}}
        self.write_config()
        for operation in ('--session','--recover-interrupted','--recover-compaction','--inspect-interrupted'):
            self.assertEqual(env.resolve([*self.args,operation,str(session)],self.package),session/'environment.lock.json')
        (session/'environment.lock.json').unlink()
        self.assertEqual(env.resolve([*self.args,'--session',str(session)],self.package),self.package/'general.lock.json')

    def test_unsupported_execution_properties_are_not_silently_ignored(self):
        for key,value in [('runArgs',['--privileged']),('mounts',['/tmp:/host']),('initializeCommand','touch /tmp/not-run'),('features',{}),('postCreateCommand','curl example.com')]:
            self.config[key]=value
            self.write_config()
            with self.assertRaisesRegex(ValueError,'unsupported'):
                env.definition(self.workspace)
            del self.config[key]
        self.config['workspaceFolder']='/workspace'
        self.write_config()
        with self.assertRaisesRegex(ValueError,'/work'):
            env.definition(self.workspace)

    def test_only_a_playtime_devcontainer_opts_in(self):
        general = self.package/'general.lock.json'
        path = self.workspace/'.devcontainer/devcontainer.json'
        # Devcontainers written for other tools keep the general workshop.
        path.write_text('// VS Code\n{"image": "x", "customizations": {"vscode": {}}}\n')
        self.assertEqual(env.resolve(self.args,self.package),general)
        path.write_text(json.dumps({'image':'x','features':{'node':{}},'workspaceFolder':'/workspaces/p'}))
        self.assertEqual(env.resolve(self.args,self.package),general)
        with self.assertRaisesRegex(ValueError,'customizations.playtime'):
            env.prepare(self.workspace,self.package)
        # A definition meant for Playtime never falls back over a syntax error.
        path.write_text('// note\n{"image": "x", "customizations": {"playtime": {}}}\n')
        with self.assertRaisesRegex(ValueError,'not strict JSON'):
            env.resolve(self.args,self.package)
        # An opted-in definition still refuses what it can't honor, and names the way out.
        self.config['features'] = {}
        self.write_config()
        with self.assertRaisesRegex(ValueError,'unsupported.*--environment general'):
            env.resolve(self.args,self.package)

    def test_project_cannot_supply_outside_paths_or_a_writable_lock(self):
        outside=self.root/'outside'
        outside.write_text('FROM outside')
        self.config['build']['dockerfile']='../../outside'
        self.write_config()
        with self.assertRaisesRegex(ValueError,'inside the project'):
            env.definition(self.workspace)
        outside.chmod(0o600)
        link=self.root/'lock-link'
        link.symlink_to(outside)
        with self.assertRaises(OSError): env.read_private(link)
        outside.chmod(0o644)
        with self.assertRaises(ValueError): env.read_private(outside)

    def test_prompt_file_options_preserve_flag_looking_values(self):
        for extra in (["--prompt", "--prompt-file", *self.args],
                      ["--prompt-file", "--workspace", *self.args],
                      [*self.args, "--prompt-file", "prompt.md"],
                      ["--completion-receipt", "--workspace", "--launch-id", "run-1", *self.args]):
            with self.subTest(extra=extra):
                root, source = env.workspace_arguments(extra)
                self.assertEqual(root, self.workspace)
                self.assertIsNone(source)

    def test_flag_looking_prompt_values_do_not_change_environment_selection(self):
        root,source=env.workspace_arguments([*self.args,'--prompt','--session','--wall-seconds','120'])
        self.assertEqual(root,self.workspace)
        self.assertIsNone(source)
        with self.assertRaisesRegex(ValueError,'duplicate'):
            env.workspace_arguments([*self.args,*self.args])

    def test_index_cannot_escape_its_private_store(self):
        self.publish()
        store=env.project_store(self.workspace)
        index=env.read_private(store/'current.json')
        index['lock']='../../outside'
        env.write_private(store/'current.json',index)
        with self.assertRaisesRegex(ValueError,'invalid prepared'):
            env.resolve(self.args,self.package)

    def test_missing_or_mismatched_workspace_lock_is_not_selected(self):
        path=self.publish()
        lock=env.read_private(path)
        lock['project_environment']['workspace']='/another-project'
        env.write_private(path,lock)
        with self.assertRaisesRegex(ValueError,'another workspace'):
            env.resolve(self.args,self.package)

    def test_failed_probe_does_not_publish_a_prepared_environment(self):
        (self.package/'tool.py').write_text('bridge')
        # A prepared local image avoids simulating Docker build's iidfile protocol.
        self.config.pop('build')
        self.config['image']='fixture:local'
        self.write_config()
        def fake_build(command,**kwargs):
            if '--iidfile' in command:
                Path(command[command.index('--iidfile')+1]).write_text('sha256:'+'1'*64)
        with patch.object(env,'image_identity',return_value='sha256:'+'1'*64), patch.object(env.subprocess,'run',side_effect=fake_build), patch.object(env,'probe',side_effect=RuntimeError('probe failed')):
            with self.assertRaisesRegex(RuntimeError,'probe failed'):
                env.prepare(self.workspace,self.package)
        self.assertFalse((env.project_store(self.workspace)/'current.json').exists())

    def test_base_image_metadata_cannot_claim_playtime_ownership(self):
        image = {'Id': 'sha256:'+'1'*64, 'Os':'linux', 'Architecture':'amd64',
                 'Config': {'Labels': {'org.opencontainers.image.version':'24.04'}}}
        with patch.object(env.subprocess, 'check_output', return_value=json.dumps([image])):
            self.assertEqual(env.image_identity('fixture:local'), image['Id'])
        image['Config']['Labels']['io.playtime.service'] = 'foreign'
        with patch.object(env.subprocess, 'check_output', return_value=json.dumps([image])):
            with self.assertRaisesRegex(ValueError, 'ownership labels'):
                env.image_identity('fixture:local')


if __name__=='__main__': unittest.main()
