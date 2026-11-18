#!/usr/bin/env bash
# Restore the lab's own run directories, and nothing else.
#
# `git checkout -- lab/` was used to make a tree clean after a run and it also reverted
# `lab/*.sh` edits that had not been committed yet, twice: the commit then recorded a lab that
# still asserted the opposite of what the change did. This restores only tracked files under
# `lab/run-*`, which are the ones a run writes.
set -euo pipefail
cd "$(dirname "$0")/.."
git ls-files -m -- lab | grep '^lab/run-' | while read -r path; do
  git checkout -- "$path"
done
