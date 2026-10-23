#!/usr/bin/env bash
# Fails unless the static export has every page and file the spec promises.
set -euo pipefail
out="${1:-out}"
root="$(cd "$(dirname "$0")/../.." && pwd)"
for f in en.html ja.html en/docs.html ja/docs.html \
         en/docs/installation.html ja/docs/installation.html \
         en/benchmarks.html ja/benchmarks.html \
         install _redirects _headers favicon.svg brand/mark.svg _pagefind/pagefind.js \
         llms.txt llms-full.txt en/docs/installation.md; do
  [[ -e "$out/$f" ]] || { echo "missing: $out/$f" >&2; exit 1; }
done
cmp -s "$out/install" "$root/install.sh" || { echo "out/install differs from install.sh" >&2; exit 1; }
grep -q 'English' "$out/en.html" && grep -q '日本語' "$out/en.html" || { echo "no language switch" >&2; exit 1; }
echo "export ok"
