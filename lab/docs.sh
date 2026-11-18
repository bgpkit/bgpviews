#!/usr/bin/env bash
# The documentation is a deliverable, so it is checked like one.
#
# Four things a reader depends on, and one guard against the failure mode this repository has
# already hit: a table citing a lab that does not exist, which reads as proof and is not.
#   1. CHANGELOG.md carries an Unreleased section or a dated version heading
#   2. the README says how to install, and links the design and the features table
#   3. the README's open rows still name the rows that are actually open
#   4. every `lab/<name>.sh` named in docs/features.md exists
#   5. every design section number cited in the features table exists in docs/design.md
set -euo pipefail
cd "$(dirname "$0")/.."

fail() { echo "FAIL: $*"; exit 1; }

[ -f CHANGELOG.md ] || fail "CHANGELOG.md is missing"
grep -qE '^#+ (Unreleased|\[Unreleased\])' CHANGELOG.md \
  || grep -qE '^## v[0-9]+\.[0-9]+\.[0-9]+ - [0-9]{4}-[0-9]{2}-[0-9]{2}$' CHANGELOG.md \
  || fail "CHANGELOG.md has neither an Unreleased section nor a dated version heading"
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

# The published page is a deliverable too, and it has already drifted once: it kept saying
# "nothing is implemented yet" while v0.1.0 was tagged. The page declares the release it
# describes in one meta tag, and that number has to be the manifest's number.
if [ -f site/index.html ]; then
  page_version=$(sed -n 's/.*<meta name="bgpviews-version" content="\([^"]*\)".*/\1/p' site/index.html | head -1)
  [ -n "$page_version" ] \
    || fail "site/index.html has no <meta name=\"bgpviews-version\"> tag"
  manifest_version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
  [ "$page_version" = "$manifest_version" ] \
    || fail "site/index.html says v$page_version while Cargo.toml says v$manifest_version"
fi

echo "ok: changelog, install path, open rows, site version, and every lab and section the features table cites"
