# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""The x86-64 loader's choice of library (postgres/loader/cpu_level.h).

Compiles postgres/loader/cpu_probe.c, which runs the loader's choice on a
CPU described on its command line (or the real one), and checks it against
hand-built CPUs, against rustc's own list of what an x86-64-v4 build may use,
and on an x86-64 Linux host against /proc/cpuinfo. Skips what needs a C
compiler, rustc or such a host when they are missing.
"""

import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
LOADER = ROOT / 'postgres' / 'loader'
ON_X86_LINUX = platform.system() == 'Linux' and platform.machine() in ('x86_64', 'AMD64')

# A CPU with every x86-64-v4 feature: (leaf, register) -> bits.
LEAF1_ECX = {'sse3': 0, 'ssse3': 9, 'fma': 12, 'cmpxchg16b': 13, 'sse4.1': 19, 'sse4.2': 20,
             'movbe': 22, 'popcnt': 23, 'xsave': 26, 'osxsave': 27, 'avx': 28, 'f16c': 29}
LEAF7_EBX = {'bmi1': 3, 'avx2': 5, 'bmi2': 8, 'avx512f': 16, 'avx512dq': 17, 'avx512cd': 28,
             'avx512bw': 30, 'avx512vl': 31}
EXT1_ECX = {'sahf': 0, 'lzcnt': 5}
XCR0_V4 = 0xE7  # x87, SSE, AVX, opmask, ZMM_Hi256, Hi16_ZMM


def bits(names, table):
    return sum(1 << table[name] for name in names)


def cpu(without=(), max_leaf=0x20, max_ext=0x80000008, xcr0=XCR0_V4):
    """A v4 CPU (as --cpuid/--xcr0 arguments) lacking the features WITHOUT."""
    def have(table):
        return bits([name for name in table if name not in without], table)
    return ['--cpuid', f'0={max_leaf},0,0,0',
            '--cpuid', f'1=0,0,{have(LEAF1_ECX)},0',
            '--cpuid', f'7=0,{have(LEAF7_EBX)},0,0',
            '--cpuid', f'0x80000000={max_ext},0,0,0',
            '--cpuid', f'0x80000001=0,0,{have(EXT1_ECX)},0',
            '--xcr0', hex(xcr0)]


def find_rustc():
    if shutil.which('rustc'):
        return 'rustc'
    channel = re.search(r'channel\s*=\s*"([^"]+)"', (ROOT / 'rust-toolchain.toml').read_text()).group(1)
    home = Path(os.environ.get('RUSTUP_HOME', Path.home() / '.rustup'))
    for rustc in sorted(home.glob(f'toolchains/{channel}-*/bin/rustc')):
        return str(rustc)
    return None


@unittest.skipUnless(shutil.which(os.environ.get('CC', 'cc')), 'needs a C compiler')
class LoaderChoiceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        cls.probe = Path(cls.tmp.name) / 'cpu_probe'
        subprocess.run([os.environ.get('CC', 'cc'), '-std=c11', '-O1', '-Wall', '-Wextra', '-Werror',
                        '-o', str(cls.probe), str(LOADER / 'cpu_probe.c')], check=True)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def choose(self, *args):
        """(library or None, missing feature or None, error or None)."""
        run = subprocess.run([str(self.probe), 'choose', *args], capture_output=True, text=True)
        out = dict(line.split(' ', 1) for line in run.stdout.splitlines() if ' ' in line)
        self.assertEqual(run.returncode != 0, 'error' in out, run.stdout + run.stderr)
        missing = out.get('missing')
        return out.get('library'), None if missing == '-' else missing, out.get('error')

    def test_a_v4_cpu_loads_the_v4_library(self):
        self.assertEqual(self.choose(*cpu()), ('x86-64-v4', None, None))
        for level in ('', 'auto', 'AUTO'):
            self.assertEqual(self.choose(*cpu(), '--level', level)[0], 'x86-64-v4')

    def test_any_missing_feature_loads_the_baseline(self):
        for name in [*LEAF1_ECX, *LEAF7_EBX, *EXT1_ECX]:
            with self.subTest(feature=name):
                library, missing, _ = self.choose(*cpu(without=[name]))
                self.assertEqual(library, 'x86-64')
                self.assertEqual(missing, name)

    def test_vector_state_the_os_does_not_save_loads_the_baseline(self):
        # AVX-512 registers off in XCR0 (a hypervisor or kernel that hides them).
        self.assertEqual(self.choose(*cpu(xcr0=0x07))[:2], ('x86-64', 'xcr0'))
        self.assertEqual(self.choose(*cpu(xcr0=0x03))[:2], ('x86-64', 'xcr0'))

    def test_cpus_without_the_leaves_load_the_baseline(self):
        library, missing, _ = self.choose(*cpu(max_leaf=6))
        self.assertEqual(library, 'x86-64')
        self.assertIn(missing, LEAF7_EBX)
        library, missing, _ = self.choose(*cpu(max_ext=0x80000000))
        self.assertEqual(library, 'x86-64')
        self.assertIn(missing, EXT1_ECX)

    def test_hidden_features_count_as_missing(self):
        self.assertEqual(self.choose(*cpu(), '--hide', 'avx512bw')[:2], ('x86-64', 'avx512bw'))
        self.assertEqual(self.choose(*cpu(), '--hide', 'popcnt,avx512f')[0], 'x86-64')
        self.assertEqual(self.choose(*cpu(), '--hide', 'avx512')[0], 'x86-64-v4')  # names match whole
        self.assertEqual(self.choose(*cpu(), '--hide', '')[0], 'x86-64-v4')

    def test_a_forced_level(self):
        self.assertEqual(self.choose(*cpu(), '--level', 'x86-64'), ('x86-64', None, None))
        self.assertEqual(self.choose(*cpu(without=['avx2']), '--level', 'x86-64')[0], 'x86-64')
        self.assertEqual(self.choose(*cpu(), '--level', 'x86-64-v4')[0], 'x86-64-v4')
        # v4 code on a CPU without it would die of SIGILL: refuse, naming why.
        library, _, error = self.choose(*cpu(without=['avx512vl']), '--level', 'x86-64-v4')
        self.assertIsNone(library)
        self.assertIn('avx512vl', error)
        library, _, error = self.choose(*cpu(), '--level', 'x86-64-v3')
        self.assertIsNone(library)
        self.assertIn('x86-64-v3', error)

    def test_checks_everything_rustc_enables_for_x86_64_v4(self):
        rustc = find_rustc()
        if not rustc:
            self.skipTest('needs rustc')

        def features(*flags):
            out = subprocess.run([rustc, '--print', 'cfg', '--target', 'x86_64-unknown-linux-gnu', *flags],
                                 capture_output=True, text=True, check=True).stdout
            return set(re.findall(r'^target_feature="([^"]+)"$', out, re.M))

        enabled = features('-C', 'target-cpu=x86-64-v4') - features()
        self.assertTrue(enabled, 'rustc lists no x86-64-v4 features')
        checked = set(subprocess.run([str(self.probe), 'features'], capture_output=True, text=True,
                                     check=True).stdout.split())
        self.assertEqual(enabled - checked, set(), 'x86-64-v4 enables features the loader does not check')

    @unittest.skipUnless(ON_X86_LINUX, 'needs an x86-64 Linux host')
    def test_the_real_cpu_agrees_with_proc_cpuinfo(self):
        flags = set(re.search(r'^flags\s*:(.*)$', Path('/proc/cpuinfo').read_text(), re.M).group(1).split())
        # /proc/cpuinfo's names for the features the loader checks.
        names = {'sse3': 'pni', 'sse4.1': 'sse4_1', 'sse4.2': 'sse4_2', 'cmpxchg16b': 'cx16',
                 'sahf': 'lahf_lm', 'lzcnt': 'abm'}
        # (OSXSAVE is not listed there; the kernel clears what it does not save.)
        need = [names.get(name, name) for name in [*LEAF1_ECX, *LEAF7_EBX, *EXT1_ECX] if name != 'osxsave']
        expected = 'x86-64-v4' if all(flag in flags for flag in need) else 'x86-64'
        self.assertEqual(self.choose('--real')[0], expected)
        self.assertEqual(self.choose('--real', '--hide', 'avx512f')[0], 'x86-64')


if __name__ == '__main__':
    unittest.main()
