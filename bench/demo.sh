#!/bin/zsh
# README demo: nb and brew install the same formulae side by side, in real
# time, in two tmux panes recorded by asciinema:
#   bench/demo.sh <out-dir> <formula>...
# Needs nb, brew, tmux, asciinema and agg on PATH. It uninstalls everything in
# Homebrew and resets nextbrew, so run it on a throwaway machine. Both start
# cold: nothing installed, no downloads cached, formula index refreshed.
# nb skips its PATH shadow check, which brew's links in the other pane trip.
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ANALYTICS=1 HOMEBREW_NO_ENV_HINTS=1 HOMEBREW_NO_INSTALL_CLEANUP=1
export NEXTBREW_AUTO_INIT=true
setopt nullglob
OUT=${1:A}; shift; mkdir -p $OUT
f=(/opt/homebrew/Cellar/*(:t)); (( $#f )) && brew uninstall --force --ignore-dependencies $f
rm -rf "$(brew --cache)/downloads"
nb reset -y >/dev/null 2>&1; nb update >/dev/null

# pane <title> <command>: types the command, runs it, marks the pane done.
pane=$OUT/pane.sh
cat > $pane <<'EOF'
#!/bin/bash
TIMEFORMAT=$'\n\e[1m%Rs\e[0m'
printf '\e[1;32m$\e[0m '; s="$2"
for ((i = 0; i < ${#s}; i++)); do printf '%s' "${s:i:1}"; sleep 0.04; done; echo
# stdin off the tty: brew otherwise waits forever on the recording's terminal.
time $2 </dev/null
touch "$3/$1.done"
while [[ ! -e $3/nextbrew.done || ! -e $3/Homebrew.done ]]; do sleep 0.2; done
sleep 3
tmux kill-server
EOF
chmod +x $pane
TERM=xterm-256color asciinema rec --headless --overwrite --window-size 120x14 -c "tmux -f /dev/null new-session \
  'sleep 0.5; HOMEBREW_NO_PATH_SHADOW_CHECK=1 $pane nextbrew \"nb install $*\" $OUT' \; \
  set status off \; set pane-border-status top \; set pane-border-format ' #{pane_title} ' \; \
  select-pane -T nextbrew \; \
  split-window -h 'sleep 0.5; $pane Homebrew \"brew install $*\" $OUT' \; select-pane -T Homebrew" $OUT/demo.cast
# Stop before tmux leaves the alternate screen so the GIF ends on both times.
awk '/1049l/ { exit } { print }' $OUT/demo.cast > $OUT/demo.trim.cast
agg --font-size 20 --theme monokai $OUT/demo.trim.cast $OUT/demo.gif
