#!/bin/zsh
# Install times of Homebrew-compatible clients on the same formulae:
#   bench/clients.sh <out-dir> <formula>...
# Needs brew, nb, zb and wax on PATH and nanobrew at $NANOBREW (its binary is
# also called nb). It uninstalls everything in Homebrew and resets nextbrew,
# zerobrew and nanobrew, so run it on a throwaway machine such as a VM.
# bench/report.py turns one or more out-dirs into the published tables.
#
# For each formula and tool: cold = nothing installed and no downloads cached
# (the tool's bulk formula index is refreshed untimed beforehand), warm = nothing
# installed, downloads from the cold run kept. Tool order rotates per formula so
# no tool is always first to hit the CDN.
[[ -f ~/.vmenv ]] && . ~/.vmenv
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ANALYTICS=1 HOMEBREW_NO_ENV_HINTS=1 HOMEBREW_NO_INSTALL_CLEANUP=1
export NEXTBREW_AUTO_INIT=true ZEROBREW_AUTO_INIT=true
# nanobrew would otherwise install some formulae from upstream release
# binaries instead of Homebrew bottles; telemetry off for all.
export NANOBREW_DISABLE_UPSTREAM=1 NANOBREW_NO_TELEMETRY=1
zmodload zsh/datetime zsh/mathfunc
setopt nullglob
NANO=${NANOBREW:-~/nanobrew-bin/nb}
OUT=$1; shift; mkdir -p $OUT
LOG=$OUT/install.log
TOOLS=(${=TOOLS:-brew nb zb nanobrew wax})
export HOMEBREW_CACHE=~/bench-brew-cache; mkdir -p $HOMEBREW_CACHE

brew_rm() { local f=(/opt/homebrew/Cellar/*(:t)); (( $#f )) && brew uninstall --force --ignore-dependencies $f; }
nano_rm() { local f=($($NANO list --names 2>/dev/null)); (( $#f )) && $NANO remove $f; }

# prep_<tool> cold|warm: put the tool in the state for that run.
prep_brew() {
  brew_rm
  [[ $1 == cold ]] && find $HOMEBREW_CACHE -mindepth 1 -maxdepth 1 ! -name api -exec rm -rf {} +
  return 0
}
prep_nb() { if [[ $1 == cold ]]; then nb reset -y; nb update; else nb uninstall --all; fi; }
prep_zb() { if [[ $1 == cold ]]; then zb reset -y; zb update; else zb uninstall --all; fi; }
prep_nanobrew() {
  nano_rm
  [[ $1 == cold ]] && rm -rf /opt/nanobrew/store/* /opt/nanobrew/cache/blobs/* /opt/nanobrew/cache/api/* /opt/nanobrew/cache/tmp/*
  return 0
}
prep_wax() { brew_rm; echo '{}' > ~/.wax/installed.json; }
done_brew() { brew_rm; }
done_nb() { nb reset -y; }
done_zb() { zb reset -y; }
done_nanobrew() { prep_nanobrew cold; }
done_wax() { prep_wax; }

run_brew() { brew install --formula $1; }
run_nb() { nb install $1; }
run_zb() { zb install $1; }
run_nanobrew() { $NANO install $1; }
run_wax() { wax install $1; }

cellar_brew=/opt/homebrew/Cellar; cellar_wax=/opt/homebrew/Cellar
cellar_nb=/opt/nextbrew/Cellar; cellar_zb=/opt/zerobrew/Cellar
cellar_nanobrew=/opt/nanobrew/prefix/Cellar

{
  echo "date	$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "os	macOS $(sw_vers -productVersion) $(uname -m)"
  echo "cpu	$(sysctl -n machdep.cpu.brand_string) $(sysctl -n hw.ncpu) cores $(( $(sysctl -n hw.memsize) / 1073741824 ))GB"
  echo "brew	$(brew --version | head -1)"
  echo "nb	$(nb --version)"
  echo "zb	$(zb --version)"
  echo "nanobrew	nanobrew $($NANO version 2>/dev/null)"
  echo "wax	$(wax --version)"
} > $OUT/env.tsv

# Bandwidth estimate: download the go bottle through brew.
brew info --formula go >/dev/null 2>&1
t0=$EPOCHREALTIME
if brew fetch --force --formula go >/dev/null 2>&1; then
  f=$(brew --cache --formula go); s=$(( EPOCHREALTIME - t0 ))
  printf 'bandwidth\t~%.0f Mbit/s\n' $(( $(stat -f %z $f) * 8 / s / 1e6 )) >> $OUT/env.tsv
fi
find $HOMEBREW_CACHE -mindepth 1 -maxdepth 1 ! -name api -exec rm -rf {} +

for t in $TOOLS; do done_$t >/dev/null 2>&1; done
i=0
for p in "$@"; do
  order=(${TOOLS:$i} ${TOOLS:0:$i}); i=$(( (i + 1) % $#TOOLS ))
  for t in $order; do
    for phase in cold warm; do
      print "### $p $t $phase" >> $LOG
      prep_$t $phase >> $LOG 2>&1
      t0=$EPOCHREALTIME
      run_$t $p >> $LOG 2>&1; rc=$?
      ms=$(( int((EPOCHREALTIME - t0) * 1000) ))
      cellar=cellar_$t
      [[ $rc == 0 && ! -d ${(P)cellar}/$p ]] && rc=missing
      kegs=(${(P)cellar}/*(/N))
      print "$p\t$t\t$phase\t$ms\t$rc\t$#kegs" | tee -a $OUT/results.tsv
    done
    done_$t >> $LOG 2>&1
  done
done
print BENCHDONE
