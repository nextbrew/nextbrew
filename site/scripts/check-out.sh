#!/usr/bin/env bash
# Fails unless the static export has every page and file the spec promises.
set -euo pipefail
out="${1:-out}"
root="$(cd "$(dirname "$0")/../.." && pwd)"
for f in en.html ja.html en/docs.html ja/docs.html \
         en/docs/quick-start.html en/docs/commands/install.html en/docs/reference/compatibility.html \
         en/docs/installation.html ja/docs/installation.html \
         en/benchmarks.html ja/benchmarks.html \
         install _routes.json _headers favicon.svg brand/mark.svg _pagefind/pagefind.js \
         llms.txt llms-full.txt en/docs/installation.md; do
  [[ -e "$out/$f" ]] || { echo "missing: $out/$f" >&2; exit 1; }
done
cmp -s "$out/install" "$root/install.sh" || { echo "out/install differs from install.sh" >&2; exit 1; }
{ grep -q 'English' "$out/en.html" && grep -q '日本語' "$out/en.html"; } || { echo "no language switch" >&2; exit 1; }
content="$root/site/content"
diff <(cd "$content/en" && find . -name '*.mdx' -o -name '_meta.js' | sort) \
     <(cd "$content/ja" && find . -name '*.mdx' -o -name '_meta.js' | sort) >&2 \
  || { echo "en and ja pages differ" >&2; exit 1; }
# Internal links must resolve (or be locale-less, which functions/_middleware.js redirects).
broken=$(cd "$out" && grep -rhoE 'href="/[^"]*"' --include='*.html' . | sed -E 's/^href="//; s/"$//; s/[#?].*$//' | sort -u | while read -r p; do
  [[ "$p" == / || "$p" == //* ]] && continue
  for c in "$p" "$p.html" "$p/index.html"; do [[ -f ".$c" ]] && continue 2; done
  if [[ ! "$p" =~ ^/(en|ja)(/|$) ]]; then
    for c in "/en$p" "/en$p.html"; do [[ -f ".$c" ]] && continue 2; done
  fi
  echo "$p"
done)
[[ -z "$broken" ]] || { echo "broken internal links:" >&2; echo "$broken" >&2; exit 1; }
echo "export ok"
