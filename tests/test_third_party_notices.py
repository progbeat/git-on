#!/usr/bin/env python3
"""Artifact notices must contain normal/build license texts, excluding dev-only code."""
import importlib.util
from pathlib import Path
import tempfile
import unittest
spec=importlib.util.spec_from_file_location('notices',Path(__file__).resolve().parents[1]/'scripts/third_party_notices.py')
module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)


class NoticeTests(unittest.TestCase):
    def setUp(self):
        self.temporary=tempfile.TemporaryDirectory();self.addCleanup(self.temporary.cleanup);self.root=Path(self.temporary.name)
        self.metadata={'packages':[{'id':'root','source':None}], 'resolve':{'root':'root','nodes':[{'id':'root','deps':[]}]}}
    def add(self,name,kind,license_text):
        root=self.root/name;root.mkdir();(root/'LICENSE').write_text(license_text)
        self.metadata['packages'].append({'id':name,'name':name,'version':'1.0.0','source':'registry+fixture','license':'MIT','license_file':None,'manifest_path':str(root/'Cargo.toml')})
        self.metadata['resolve']['nodes'][0]['deps'].append({'pkg':name,'dep_kinds':[{'kind':kind}]})
        self.metadata['resolve']['nodes'].append({'id':name,'deps':[]})
    def test_normal_and_build_texts_are_present_dev_only_is_excluded(self):
        self.add('runtime',None,'Copyright runtime\nMIT license text')
        self.add('builder','build','Copyright builder\nBSD license text')
        self.add('example','dev','GPL example not linked')
        result=module.collect(self.metadata)
        self.assertIn('Copyright runtime',result);self.assertIn('BSD license text',result);self.assertNotIn('GPL example',result)
    def test_missing_text_or_outside_package_path_fails_closed(self):
        self.add('runtime',None,'License text');(self.root/'runtime/LICENSE').unlink()
        with self.assertRaisesRegex(ValueError,'missing license text'):module.collect(self.metadata)
        self.metadata['packages'][-1]['license_file']='../../outside-license'
        with self.assertRaisesRegex(ValueError,'escapes package'):module.collect(self.metadata)


if __name__=='__main__':unittest.main(verbosity=2)
