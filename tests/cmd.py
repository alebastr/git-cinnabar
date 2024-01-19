from __future__ import absolute_import, unicode_literals
import os
import unittest
from cinnabar.cmd.util import Version as CmdVersion
from cinnabar.git import (
    Git,
    split_ls_tree,
)
from cinnabar.util import StrictVersion, one


class Version(StrictVersion):
    def __init__(self, v):
        if isinstance(v, bytes):
            v = v.decode('ascii')
        if v.endswith('a'):
            v += '0'
        StrictVersion.__init__(self, v)


class TestVersion(unittest.TestCase):
    def test_cinnabar_version(self):
        desc = one(Git.iter('describe', '--tags', 'HEAD'))
        version = Version(CmdVersion.cinnabar_version())
        if b'-' in desc:
            last_tag, n, sha1 = desc.rsplit(b'-', 2)
            self.assertGreater(version, Version(last_tag))
        else:
            self.assertEqual(version, Version(desc))

    def test_module_version(self):
        module = one(Git.iter(
            'ls-tree', 'HEAD', 'cinnabar',
            cwd=os.path.join(os.path.dirname(__file__), '..')))
        self.assertEqual(CmdVersion.module_version(),
                         split_ls_tree(module)[2].decode('ascii'))

    def test_helper_version(self):
        helper = one(Git.iter(
            'ls-tree', 'HEAD', 'helper',
            cwd=os.path.join(os.path.dirname(__file__), '..')))
        self.assertEqual(CmdVersion.helper_version()[1],
                         split_ls_tree(helper)[2].decode('ascii'))
