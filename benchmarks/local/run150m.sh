#!/bin/bash
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A published workload on the local 150M database:
#
#   run150m.sh IMAGE LABEL STYLE UPDATES [SECONDS]
#
# Same as `workload.sh 150m ...`; see workload.sh for the environment.
set -euo pipefail
exec bash "$(dirname "${BASH_SOURCE[0]}")/workload.sh" 150m "$@"
