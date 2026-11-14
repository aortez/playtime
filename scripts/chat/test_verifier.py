"""Hash-cache regression tests; no inference server or GPU is used."""

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("strata_verifier", Path(__file__).with_name("verify-strata-host.py"))
verifier = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verifier)


class HashTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.artifact = self.root / "model.bin"
        self.artifact.write_bytes(b"reviewed model")
        self.record = {"path": str(self.artifact), "size": 14,
                       "sha256": hashlib.sha256(b"reviewed model").hexdigest()}
        self.host = self.host_for(self.record)
        self.directory = self.root / "cache"

    def host_for(self, record):
        return {"artifacts_manifest_sha256": verifier.digest_json([record])}

    def new(self, full=False):
        return verifier.ArtifactHashes(self.directory, self.host, full)

    def seed(self):
        hashes = self.new()
        result = hashes.hash_file(self.artifact, reusable=True)
        hashes.finish(self.host_for(result))
        self.assertEqual(result, self.record)
        return hashes

    def test_successful_hashes_are_private_and_reused_without_reading_bytes(self):
        cold = self.seed()
        self.assertEqual(cold.stats['hashed_bytes'], 14)
        self.assertEqual(cold.stats['reused_files'], 0)
        self.assertEqual(cold.path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.directory.stat().st_mode & 0o777, 0o700)
        warm = self.new()
        with patch.object(verifier.hashlib, 'file_digest', side_effect=AssertionError('unexpected read')):
            result = warm.hash_file(self.artifact, reusable=True)
            warm.finish(self.host_for(result))
        self.assertEqual(result, self.record)
        self.assertEqual(warm.stats['reused_bytes'], 14)
        self.assertEqual(warm.stats['hashed_files'], 0)

    def test_full_mode_and_nonreusable_files_always_read_the_bytes(self):
        self.seed()
        for full, reusable in [(True, True), (False, False)]:
            with self.subTest(full=full, reusable=reusable):
                hashes = self.new(full)
                with patch.object(verifier.hashlib, 'file_digest', wraps=hashlib.file_digest) as read:
                    result = hashes.hash_file(self.artifact, reusable=reusable)
                self.assertEqual(read.call_count, 1)
                self.assertEqual(result, self.record)
                self.assertEqual(hashes.stats['reused_files'], 0)

    def test_same_size_rewrite_with_restored_mtime_invalidates_cache(self):
        seed = self.seed()
        published = seed.path.read_bytes()
        before = self.artifact.stat()
        time.sleep(.01)
        self.artifact.write_bytes(b"replaced model")
        os.utime(self.artifact, ns=(before.st_atime_ns, before.st_mtime_ns))
        self.assertEqual(self.artifact.stat().st_size, before.st_size)
        self.assertEqual(self.artifact.stat().st_mtime_ns, before.st_mtime_ns)
        self.assertNotEqual(self.artifact.stat().st_ctime_ns, before.st_ctime_ns)
        hashes = self.new()
        result = hashes.hash_file(self.artifact, reusable=True)
        self.assertEqual(hashes.stats['hashed_files'], 1)
        self.assertNotEqual(result['sha256'], self.record['sha256'])
        with self.assertRaisesRegex(RuntimeError, 'differ from the reviewed'):
            hashes.finish(self.host_for(result))
        self.assertEqual(seed.path.read_bytes(), published)

    def test_replacement_and_retargeted_symlink_invalidate_cache(self):
        for symlink in (False, True):
            with self.subTest(symlink=symlink):
                self.seed()
                before = self.artifact.stat()
                replacement = self.root / 'replacement'
                replacement.write_bytes(b'replaced model')
                os.utime(replacement, ns=(before.st_atime_ns, before.st_mtime_ns))
                if symlink:
                    self.artifact.unlink()
                    self.artifact.symlink_to(replacement)
                else:
                    replacement.replace(self.artifact)
                hashes = self.new()
                result = hashes.hash_file(self.artifact, reusable=True)
                self.assertEqual(hashes.stats['hashed_files'], 1)
                self.assertEqual(result['sha256'], hashlib.sha256(b'replaced model').hexdigest())
                self.artifact.unlink()
                self.artifact.write_bytes(b'reviewed model')

    def test_invalid_cache_is_ignored_and_rebuilt(self):
        for problem in ('json', 'version', 'host', 'digest', 'permissions'):
            with self.subTest(problem=problem):
                seed = self.seed()
                cache = json.loads(seed.path.read_text())
                if problem == 'version':
                    cache['schema_version'] = 99
                elif problem == 'host':
                    cache['host_digest'] = 'wrong'
                elif problem == 'digest':
                    cache['entries'][str(self.artifact)]['sha256'] = 'not a digest'
                seed.path.write_text('{' if problem == 'json' else json.dumps(cache))
                if problem == 'permissions':
                    seed.path.chmod(0o644)
                hashes = self.new()
                result = hashes.hash_file(self.artifact, reusable=True)
                self.assertEqual(hashes.stats['hashed_files'], 1)
                hashes.finish(self.host_for(result))
                self.assertTrue(hashes.stats['cache_saved'])
                self.assertEqual(seed.path.stat().st_mode & 0o777, 0o600)

    def test_cache_symlink_does_not_read_or_overwrite_its_target(self):
        seed = self.seed()
        external = self.root / 'external'
        external.write_text('leave this alone')
        seed.path.unlink()
        seed.path.symlink_to(external)
        hashes = self.new()
        result = hashes.hash_file(self.artifact, reusable=True)
        hashes.finish(self.host_for(result))
        self.assertEqual(hashes.stats['hashed_files'], 1)
        self.assertEqual(external.read_text(), 'leave this alone')
        self.assertFalse(seed.path.is_symlink())

    def test_shared_cache_directory_falls_back_to_full_hash_without_writing(self):
        seed = self.seed()
        published = seed.path.read_bytes()
        self.directory.chmod(0o755)
        hashes = self.new()
        result = hashes.hash_file(self.artifact, reusable=True)
        hashes.finish(self.host_for(result))
        self.assertEqual(hashes.stats['hashed_files'], 1)
        self.assertFalse(hashes.stats['cache_saved'])
        self.assertEqual(seed.path.read_bytes(), published)

    def test_changes_during_hashing_or_before_publication_are_rejected(self):
        real_digest = hashlib.file_digest

        def changing_digest(source, algorithm):
            result = real_digest(source, algorithm)
            self.artifact.write_bytes(b'changed')
            return result

        hashes = self.new()
        with patch.object(verifier.hashlib, 'file_digest', side_effect=changing_digest):
            with self.assertRaisesRegex(RuntimeError, 'changed while verifying'):
                hashes.hash_file(self.artifact, reusable=True)
        self.assertFalse(hashes.path.exists())
        self.artifact.write_bytes(b'reviewed model')
        seed = self.seed()
        published = seed.path.read_bytes()
        warm = self.new()
        result = warm.hash_file(self.artifact, reusable=True)
        self.artifact.write_bytes(b'changed after cached check')
        with self.assertRaisesRegex(RuntimeError, 'changed during verification'):
            warm.finish(self.host_for(result))
        self.assertEqual(seed.path.read_bytes(), published)

    def test_valid_looking_wrong_digest_is_rejected_and_full_verification_repairs_it(self):
        seed = self.seed()
        cache = json.loads(seed.path.read_text())
        cache['entries'][str(self.artifact)]['sha256'] = '0' * 64
        seed.path.write_text(json.dumps(cache))
        hashes = self.new()
        result = hashes.hash_file(self.artifact, reusable=True)
        with self.assertRaisesRegex(RuntimeError, 'differ from the reviewed'):
            hashes.finish(self.host_for(result))
        forced = self.new(full=True)
        result = forced.hash_file(self.artifact, reusable=True)
        forced.finish(self.host_for(result))
        self.assertEqual(result, self.record)


class HostVerificationTests(unittest.TestCase):
    def test_warm_report_keeps_lock_identity_and_rechecks_engine_source_and_config(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            engine = root / '.local/share/strata/components/engine/engine-hip-fixture'
            engine.parent.mkdir(parents=True)
            shutil.copyfile(sys.executable, engine)
            engine.chmod(0o700)
            native = root / 'native'
            native.mkdir()
            (native / 'dense.bin').write_bytes(b'model data')
            responses = root / 'serve/responses.py'
            responses.parent.mkdir()
            responses.write_text('# fixture source\n')
            argv = [str(engine), '-c', 'import time; time.sleep(60)', '--serve',
                    '--native', str(native), '--draft', 'dflash2', '--draft-fallback', 'none']
            config = {'args': argv, 'backend': 'hip', 'drafting': 'dflash2', 'cwd': str(root)}
            (root / 'engine-config.json').write_text(json.dumps(config))
            artifact = {'path': str(native / 'dense.bin'), 'size': 10,
                        'sha256': hashlib.sha256(b'model data').hexdigest()}
            environment = {k: v for k, v in os.environ.items()
                           if k.startswith(('STRATA_', 'CUDA_', 'HIP_', 'HSA_', 'ROCR_'))}
            expected = {'backend': 'hip', 'quantization': 'TQ2_0',
                        'engine_sha256': hashlib.sha256(engine.read_bytes()).hexdigest(),
                        'artifacts_manifest_sha256': verifier.digest_json([artifact]),
                        'responses_source_sha256': hashlib.sha256(responses.read_bytes()).hexdigest(),
                        'launch_configuration_sha256': verifier.digest_json({'config': config, 'argv': argv, 'environment': environment})}
            with subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as process:
                try:
                    with patch.object(verifier.Path, 'home', return_value=root):
                        cold = verifier.verify(root, expected)
                        warm = verifier.verify(root, expected)
                        full = verifier.verify(root, expected, full=True)
                        self.assertEqual(cold['host'], warm['host'])
                        self.assertEqual(full['host'], warm['host'])
                        self.assertEqual(warm['host'], expected)
                        self.assertEqual(warm['verification']['reused_files'], 1)
                        self.assertEqual(warm['verification']['hashed_files'], 2)
                        self.assertEqual(full['verification']['reused_files'], 0)
                        self.assertEqual(full['verification']['hashed_files'], 3)
                        # Warm artifacts cannot hide a changed Responses source.
                        responses.write_text('# changed source\n')
                        with self.assertRaisesRegex(RuntimeError, 'differ from the reviewed'):
                            verifier.verify(root, expected)
                        responses.write_text('# fixture source\n')
                        # Re-enumerate directories: the cached list must not hide
                        # a removed artifact or a newly added file.
                        (native / 'dense.bin').rename(root / 'moved.bin')
                        with self.assertRaisesRegex(RuntimeError, 'differ from the reviewed'):
                            verifier.verify(root, expected)
                        (root / 'moved.bin').rename(native / 'dense.bin')
                        (native / 'extra.bin').write_bytes(b'new artifact')
                        with self.assertRaisesRegex(RuntimeError, 'differ from the reviewed'):
                            verifier.verify(root, expected)
                        (native / 'extra.bin').unlink()
                        config['changed_setting'] = True
                        (root / 'engine-config.json').write_text(json.dumps(config))
                        with self.assertRaisesRegex(RuntimeError, 'differ from the reviewed'):
                            verifier.verify(root, expected)
                finally:
                    process.terminate()
                    process.communicate(timeout=5)


if __name__ == '__main__':
    unittest.main()
