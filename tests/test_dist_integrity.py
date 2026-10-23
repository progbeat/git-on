#!/usr/bin/env python3
"""Cross-job integrity regressions; fixture archives are never executed."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import runpy
import tempfile
import unittest
from unittest import mock

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('verify_local',ROOT/'scripts/verify_local_dist.py')
mod=importlib.util.module_from_spec(spec);spec.loader.exec_module(mod)
SHA='a'*40


class NativeIntegrityTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name)
        for target in mod.TARGETS:
            name='git-on-'+target+'.tar.gz';payload=b'fixture '+target.encode()
            (self.root/name).write_bytes(payload);digest=hashlib.sha256(payload).hexdigest()
            (self.root/(name+'.sha256')).write_text(digest+' *'+name+'\n')
            (self.root/(target+'-dist-manifest.json')).write_text(json.dumps({'artifacts':{name:{'checksums':{'sha256':digest}}}}))
            (self.root/('provenance-'+target+'.json')).write_text(json.dumps({'commit':SHA,'target':target}))

    def test_verified_native_set(self): mod.verify(self.root,SHA)

    def test_cargo_dist_checksum_trailing_blank_line(self):
        path=self.root/('git-on-'+mod.TARGETS[0]+'.tar.gz.sha256')
        path.write_text(path.read_text()+'\n')  # actual 0.33.0 native output
        mod.verify(self.root,SHA)

    def test_corrupt_archive_cannot_be_blessed_by_global_rehash(self):
        (self.root/('git-on-'+mod.TARGETS[0]+'.tar.gz')).write_bytes(b'tampered')
        with self.assertRaisesRegex(ValueError,'changed across jobs'):mod.verify(self.root,SHA)

    def test_missing_and_malformed_checksums_fail(self):
        checksum=self.root/('git-on-'+mod.TARGETS[0]+'.tar.gz.sha256')
        checksum.write_text('missing\n')
        with self.assertRaisesRegex(ValueError,'invalid native'):mod.verify(self.root,SHA)
        checksum.unlink()
        with self.assertRaises(FileNotFoundError):mod.verify(self.root,SHA)

    def test_foreign_commit_and_manifest_hash_fail(self):
        with self.assertRaisesRegex(ValueError,'source/target mismatch'):mod.verify(self.root,'b'*40)
        name='git-on-'+mod.TARGETS[0]+'.tar.gz'
        (self.root/(mod.TARGETS[0]+'-dist-manifest.json')).write_text(json.dumps({'artifacts':{name:{'checksums':{'sha256':'0'*64}}}}))
        with self.assertRaisesRegex(ValueError,'manifest mismatch'):mod.verify(self.root,SHA)


class ReleaseMetadataTests(unittest.TestCase):
    def test_actual_assembly_distinguishes_macos_startup_and_mount_requirements(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary)
            source=root/'target/distrib';source.mkdir(parents=True)
            for name in ('git-on-aarch64-apple-darwin.tar.gz','git-on-x86_64-unknown-linux-gnu.tar.gz','git-on-installer.sh','source.tar.gz'):
                (source/name).write_bytes(b'fixture; never executed')
            previous=Path.cwd()
            try:
                os.chdir(root)
                with mock.patch('subprocess.check_output',side_effect=[json.dumps({'packages':[{'version':'1.3.0'}]}),SHA]):
                    runpy.run_path(str(ROOT/'scripts/assemble_dist.py'),run_name='__main__')
            finally:
                os.chdir(previous)
            metadata=json.loads((root/'dist/release.json').read_text())
            mac=metadata['targets']['aarch64-apple-darwin']
            self.assertEqual(mac['baseline'],'macOS 13+ (startup and explicit-all materialization)')
            self.assertIs(mac['mounts']['supported'], False)
            self.assertIn('#12',mac['mounts']['reason'])
            self.assertNotIn('GitOn.app',json.dumps(mac))
            inventory=(root/'dist/SHA256SUMS').read_text()
            self.assertIn(hashlib.sha256((root/'dist/release.json').read_bytes()).hexdigest()+'  release.json\n',inventory)


if __name__=='__main__':unittest.main(verbosity=2)
