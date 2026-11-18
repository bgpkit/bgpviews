#!/usr/bin/env bash
# The documentation is a deliverable, so it is checked like one.
#
# Four things a reader depends on, and one guard against the failure mode this repository has
# already hit: a table citing a lab that does not exist, which reads as proof and is not.
#   1. CHANGELOG.md carries an Unreleased section, because nothing here is tagged yet
#   2. the README says how to install, and links the design and the features table
#   3. the README's open rows still name the rows that are actually open
#   4. every `lab/<name>.sh` named in docs/features.md exists
#   5. every design section number cited in the features table exists in docs/design.md
set -euo pipefail
cd "$(dirname "$0")/.."

fail() { echo "FAIL: $*"; exit 1; }

[ -f CHANGELOG.md ] || fail "CHANGELOG.md is missing"
grep -qE '^#+ (Unreleased|\[Unreleased\])' CHANGELOG.md \
  || fail "CHANGELOG.md has no Unreleased section, and nothing in this tree is tagged"
entries=$(grep -cE '^- ' CHANGELOG.md)
[ "$entries" -ge 10 ] || fail "CHANGELOG.md lists only $entries entries"

[ -f README.md ] || fail "README.md is missing"
grep -q 'cargo install' README.md || fail "the README does not say how to install"
grep -q 'docs/design.md' README.md || fail "the README does not link the design"
grep -q 'docs/features.md' README.md || fail "the README does not link the features table"

grep -qi 'known open rows\|open rows' README.md \
  || fail "the README's open rows section is gone; a reader would take the tree for finished"
grep -q 'loc-rib' README.md || fail "the Loc-RIB open row left the README"
grep -q 'journal' README.md || fail "the journal-format open row left the README"

[ -f docs/features.md ] || fail "docs/features.md is missing"
[ -f docs/design.md ] || fail "docs/design.md is missing"

# A cited lab that does not exist is worse than no citation: it reads as evidence.
missing=""
for name in $(grep -oE '\./lab/[a-z0-9_]+\.sh' docs/features.md | sed 's|^\./||' | sort -u); do
  [ -f "$name" ] || missing="$missing $name"
done
[ -z "$missing" ] || fail "docs/features.md cites lab scripts that do not exist:$missing"

# Same rule for the design sections: a row that cites section 14 of a document with 13 sections
# points the reader at nothing.
sections=$(grep -cE '^## [0-9]+\.' docs/design.md)
for n in $(grep -oE '\| [0-9]+(, [0-9]+)* \|' docs/features.md | tr -d '|,' ); do
  [ "$n" -le "$sections" ] || fail "docs/features.md cites design section $n, but the design has $sections"
done

echo "ok: changelog, install path, open rows, and every lab and section the features table cites"
