#!/usr/bin/env bash
# [proto] proto/etcd/ against the official etcd repo, byte for byte: pulls that repo's proto
# files only, and fails on any difference. proto/etcd/ mirrors its tree, so a file's path
# there is its path here. At its newest release unless told otherwise: main has API no
# release has yet. The workflow proto.yml runs it; by hand: .github/proto-compare.sh [ref]
set -uo pipefail
REPO=https://github.com/etcd-io/etcd.git
ROOT="$(cd "$(dirname "$0")/.." && pwd)/proto/etcd"
REF="${1:-}"
if [ -z "$REF" ]; then
    REF="$(git ls-remote --tags --refs "$REPO" 'v3.*' | sed 's|.*refs/tags/||' | grep -E '^v3\.[0-9]+\.[0-9]+$' | sort -V | tail -1)"
    [ -n "$REF" ] || { echo "no release found in $REPO" >&2; exit 2; }
fi
mapfile -t FILES < <(cd "$ROOT" && find . -name '*.proto' | sed 's|^\./||' | sort)
[ "${#FILES[@]}" -gt 0 ] || { echo "no proto file under $ROOT" >&2; exit 2; }
UP="$(mktemp -d)"
trap 'rm -rf "$UP"' EXIT
git clone -q --depth 1 --filter=blob:none --sparse --branch "$REF" "$REPO" "$UP" 2>/dev/null \
    || { echo "cannot pull $REPO at $REF" >&2; exit 2; }
git -C "$UP" sparse-checkout set --no-cone "${FILES[@]/#//}" || exit 2
echo "etcd-io/etcd $REF at $(git -C "$UP" rev-parse --short HEAD)"
bad=0
for f in "${FILES[@]}"; do
    if ! [ -f "$UP/$f" ]; then
        echo "::error file=proto/etcd/$f::no longer in etcd-io/etcd $REF"
        bad=1
    elif diff -u --label "ours: proto/etcd/$f" --label "etcd $REF: $f" "$ROOT/$f" "$UP/$f"; then
        echo "same: $f"
    else
        echo "::error file=proto/etcd/$f::differs from etcd-io/etcd $REF"
        bad=1
    fi
done
exit "$bad"
