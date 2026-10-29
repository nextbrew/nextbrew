#!/bin/zsh
# Article demo: Homebrew has some formulae and a cask, and `nb migrate` moves
# them over, recorded by asciinema in one tmux pane:
#   bench/demo-migrate.sh <out-dir> <formula>... cask:<cask>...
# Needs nb, brew, tmux, asciinema and agg on PATH. It uninstalls everything in
# Homebrew and resets nextbrew, so run it on a throwaway machine. The prompts
# are answered with `y` through tmux, so they show as typed.
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ANALYTICS=1 HOMEBREW_NO_ENV_HINTS=1 HOMEBREW_NO_INSTALL_CLEANUP=1
export NEXTBREW_AUTO_INIT=true
setopt nullglob
OUT=${1:A}; shift; mkdir -p $OUT
f=(/opt/homebrew/Cellar/*(:t)); (( $#f )) && brew uninstall --force --ignore-dependencies $f
c=(/opt/homebrew/Caskroom/*(:t)); (( $#c )) && brew uninstall --cask --force $c
nb reset -y >/dev/null 2>&1; nb update >/dev/null
formulae=(${@:#cask:*}) casks=(${${(M)@:#cask:*}#cask:})
(( $#formulae )) && brew install -q $formulae
(( $#casks )) && brew install -q --cask $casks

run=$OUT/run.sh
cat > $run <<'EOF'
#!/bin/bash
for s in "brew list" "nb migrate"; do
  printf '\e[1;32m$\e[0m '
  for ((i = 0; i < ${#s}; i++)); do printf '%s' "${s:i:1}"; sleep 0.04; done; echo
  HOMEBREW_NO_PATH_SHADOW_CHECK=1 $s
  echo; sleep 1.5
done
sleep 2
tmux kill-server
EOF
chmod +x $run

# Answers each [y/N] prompt a second after it shows up.
answer() {
  local n=0
  until tmux has-session 2>/dev/null; do sleep 0.2; done
  until (( n == 2 )) || ! tmux has-session 2>/dev/null; do
    if tmux capture-pane -p | grep -q '\[y/N\] *$'; then
      sleep 1; tmux send-keys y; sleep 0.3; tmux send-keys Enter; (( n++ ))
      sleep 1
    fi
    sleep 0.2
  done
}
answer &

TERM=xterm-256color asciinema rec --headless --overwrite --window-size 100x32 \
  -c "tmux -f /dev/null new-session 'sleep 0.5; $run' \; set status off" $OUT/migrate.cast
wait
awk '/1049l/ { exit } { print }' $OUT/migrate.cast | LC_ALL=C sed $'s/\xef\xb8\x8e//g' > $OUT/migrate.trim.cast
agg --font-size 16 --theme monokai $OUT/migrate.trim.cast $OUT/migrate.gif
