#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Run a command while holding the machine-wide pgrx lock.

    script/pgrx-lock.py -- cargo pgrx test pg18 -p stannum

`cargo pgrx test` and `cargo pgrx install` replace the extension installed in
the server's directories, and `cargo pgrx test` shares one test server (port
28818) between every checkout on the machine. Wrap every command that
installs the extension, or uses the installed one, so that two sessions do
not replace the library under each other.

The lock is an exclusive flock(2) on `$STANNUM_PGRX_LOCK` (default
`/tmp/stannum-pgrx.lock`). It is reentrant through the environment: the
command runs with `STANNUM_PGRX_LOCK_HELD` naming the lock file, and a nested
call for the same file runs its command without locking again, so scripts
such as `script/test-all` can take the lock for each step and still run
inside an outer hold.
"""
import fcntl
import os
import subprocess
import sys


def main(argv):
    if argv and argv[0] == '--':
        argv = argv[1:]
    if not argv:
        sys.exit(__doc__)
    path = os.environ.get('STANNUM_PGRX_LOCK') or '/tmp/stannum-pgrx.lock'
    if os.environ.get('STANNUM_PGRX_LOCK_HELD') == path:
        return subprocess.call(argv)
    with open(path, 'a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        return subprocess.call(argv, env=dict(os.environ, STANNUM_PGRX_LOCK_HELD=path))


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
