#!/data/data/com.termux/files/usr/bin/bash
# OOPBoT's Termux setup for the Google Play build of Termux. OOPBoT fills in the @…@ values and serves this
# from its setup command, with OOPBOT_URL/PIPKIN_URL and OOPBOT_TOKEN/PIPKIN_TOKEN in the environment. Every step checks before it
# acts, so running the command again after a failure finishes the setup.
set -u

say() { printf 'OOPBoT: %s\n' "$*"; }
fail() {
  say "$*" >&2
  exit 1
}

# 1. Environment.
[ -n "${PREFIX:-}" ] && [ -n "${HOME:-}" ] || fail "Run this inside Termux."
OOPBOT_URL=${OOPBOT_URL:-${PIPKIN_URL:-}}
OOPBOT_TOKEN=${OOPBOT_TOKEN:-${PIPKIN_TOKEN:-}}
[ -n "$OOPBOT_URL" ] && [ -n "$OOPBOT_TOKEN" ] || fail "Copy the setup command in OOPBoT and paste it here."
export OOPBOT_URL OOPBOT_TOKEN
export PIPKIN_URL="$OOPBOT_URL" PIPKIN_TOKEN="$OOPBOT_TOKEN"
# oopbot for the release, oopbot-dev for the debug build, which keeps its own helper beside the release's.
name='@PIPKIN_NAME@'
dir="$HOME/.$name/helper"
package='@PIPKIN_PACKAGE@'
expected_ver=${OOPBOT_VERSION:-@OOPBOT_VERSION@}
case $expected_ver in
  "" | *@*) expected_ver="" ;;
esac

# Termux sessions run the shell that ~/.termux/shell links to (chsh sets it), or bash without a usable one; this picks
# it the way $PREFIX/bin/login does. OOPBoT starts its helper by opening a session, so that shell must run one of the
# hooks written in step 3.
login_shell=bash
if [ -L "$HOME/.termux/shell" ]; then
  target=$(readlink "$HOME/.termux/shell")
  if [ -f "$target" ] && [ -x "$target" ] && [ ! "$target" -ef "$PREFIX/bin/login" ]; then
    login_shell=$(basename "$(readlink -f "$target")")
  fi
fi
case $login_shell in
  bash | zsh | fish) ;;
  *) fail "OOPBoT needs bash, zsh or fish as Termux's shell. Run chsh -s bash, then paste the command again." ;;
esac


alive() { # $1 = pid file, $2 = text the process's command line must contain
  local pid
  pid=$(cat "$1" 2>/dev/null) && [ -n "$pid" ] && grep -qaF "$2" "/proc/$pid/cmdline" 2>/dev/null
}

# Replaces $1 with stdin through a temp file, unless it already has that content.
write_file() {
  local tmp="$1.$$.tmp"
  cat >"$tmp" || {
    rm -f "$tmp"
    return 1
  }
  if cmp -s "$tmp" "$1"; then
    rm -f "$tmp"
  else
    mv -f "$tmp" "$1"
  fi
}

# 2. Termux prerequisites & OOPBoT Native Engine.
dpkg --configure -a || true
missing_pkgs=()
command -v curl >/dev/null 2>&1 || missing_pkgs+=(curl)
command -v node >/dev/null 2>&1 || missing_pkgs+=(nodejs)
if [ "${#missing_pkgs[@]}" -gt 0 ]; then
  say "Installing Termux packages (${missing_pkgs[*]})..."
  { yes | pkg upgrade -y && pkg install -y "${missing_pkgs[@]}"; } ||
    fail "Couldn't install Termux packages (${missing_pkgs[*]}). Check your internet connection, then paste the command again."
fi
command -v curl >/dev/null 2>&1 || fail "curl is missing after installation."
command -v node >/dev/null 2>&1 || fail "Node.js is missing after installation."

installed_ver=""
if [ -x "$PREFIX/bin/oopbot" ]; then
  installed_ver=$("$PREFIX/bin/oopbot" --version 2>/dev/null | awk 'NR==1 && $1=="oopbot" {print $2}')
fi
need_oopbot=false
if [ -z "$installed_ver" ]; then
  need_oopbot=true
elif [ -n "$expected_ver" ] && [ "$installed_ver" != "$expected_ver" ]; then
  need_oopbot=true
elif [ -z "$expected_ver" ]; then
  case "$installed_ver" in
    0.0.* | 0.1.* | 0.2.* | 0.3.* | 0.4.* | 0.5.[0-3] | 0.5.[0-3][-+]*) need_oopbot=true ;;
  esac
fi

if [ "$need_oopbot" = true ]; then
  say "Installing OOPBoT Rust engine${expected_ver:+ v$expected_ver}..."
  mkdir -p "$PREFIX/bin" || fail "Couldn't create $PREFIX/bin."
  tmp_bin="$PREFIX/bin/oopbot.$$.tmp"
  rm -f "$tmp_bin"
  downloaded=false
  if [ -n "$expected_ver" ]; then
    curl -fsSL -m 120 "https://github.com/Ahmadfauzi34/OOPBoT/releases/download/v${expected_ver}/oopbot-aarch64-linux-android" \
      -o "$tmp_bin" && downloaded=true || rm -f "$tmp_bin"
  fi
  if [ "$downloaded" != true ]; then
    curl -fsSL -m 120 "https://github.com/Ahmadfauzi34/OOPBoT/releases/latest/download/oopbot-aarch64-linux-android" \
      -o "$tmp_bin" && downloaded=true || rm -f "$tmp_bin"
  fi
  [ "$downloaded" = true ] || fail "Couldn't download OOPBoT engine. Check your internet connection, then paste the command again."
  chmod 755 "$tmp_bin" || { rm -f "$tmp_bin"; fail "Couldn't mark oopbot executable."; }
  bin_ver=$("$tmp_bin" --version 2>/dev/null | awk 'NR==1 && $1=="oopbot" {print $2}')
  [ -n "$bin_ver" ] || { rm -f "$tmp_bin"; fail "Downloaded OOPBoT binary couldn't run."; }
  if [ -n "$expected_ver" ] && [ "$bin_ver" != "$expected_ver" ]; then
    rm -f "$tmp_bin"
    fail "Downloaded oopbot $bin_ver, but OOPBoT needs $expected_ver. Paste the command again once release v$expected_ver is available."
  fi
  mv -f "$tmp_bin" "$PREFIX/bin/oopbot" || { rm -f "$tmp_bin"; fail "Couldn't install $PREFIX/bin/oopbot."; }
  installed_ver=$bin_ver
fi

# Refresh or drop any stale secondary copy under OOPBOT_ROOT/bin so helper/host paths never pick an older binary.
root_bin_dir="$HOME/.local/share/$name/bin"
if [ -e "$root_bin_dir/oopbot" ]; then
  cp -f "$PREFIX/bin/oopbot" "$root_bin_dir/oopbot.$$.tmp" &&
    chmod 755 "$root_bin_dir/oopbot.$$.tmp" &&
    mv -f "$root_bin_dir/oopbot.$$.tmp" "$root_bin_dir/oopbot" ||
    rm -f "$root_bin_dir/oopbot.$$.tmp"
fi
if [ -f "$root_bin_dir/oopbot.version" ] && [ -n "$installed_ver" ]; then
  printf '%s\n' "$installed_ver" | write_file "$root_bin_dir/oopbot.version" || true
fi

# 3. The helper's files and ports, the boot script, the shell hooks and <name>-start.
{ mkdir -p "$dir" && chmod 700 "$dir"; } || fail "Couldn't create $dir."
fetch() { # $1 = name, $2 = sha256, $3 = mode
  local tmp="$dir/$1.$$.tmp"
  [ "$(sha256sum "$dir/$1" 2>/dev/null | cut -d' ' -f1)" = "$2" ] && chmod "$3" "$dir/$1" && return 0
  curl -fsS -m 60 "$OOPBOT_URL/$1" -o "$tmp" || {
    rm -f "$tmp"
    fail "Couldn't download $1 from OOPBoT. Keep OOPBoT open and paste the command again."
  }
  if [ "$(sha256sum "$tmp" | cut -d' ' -f1)" != "$2" ]; then
    rm -f "$tmp"
    fail "$1 from OOPBoT was damaged. Paste the command again."
  fi
  { chmod "$3" "$tmp" && mv -f "$tmp" "$dir/$1"; } || fail "Couldn't install $1."
}

fetch helper.mjs '@HELPER_SHA256@' 600
fetch run.sh '@RUN_SHA256@' 700
fetch start.sh '@START_SHA256@' 700
# Each build's helper has its own ports, so neither build reaches the other's helper.
printf '%s\n' '@HELPER_PORTS@' | write_file "$dir/ports" || fail "Couldn't write $dir/ports."

boot="$HOME/.termux/boot/$name-helper"
mkdir -p "${boot%/*}" || fail "Couldn't create ~/.termux/boot."
# shellcheck disable=SC2016 # $HOME expands when the boot script runs.
printf '#!%s/bin/sh\nexec "$HOME/.%s/helper/run.sh"\n' "$PREFIX" "$name" | write_file "$boot" ||
  fail "Couldn't write the boot script."
chmod 700 "$boot" || fail "Couldn't write the boot script."

# Puts $2 in the file $1 between OOPBoT's markers: in place of the first complete marked block, dropping any later
# ones, or at the end when there is none. A start marker without an end marker is left alone.
put_block() {
  local rc=$1
  [ -L "$rc" ] && rc=$(readlink -f "$rc")
  [ -e "$rc" ] || : >"$rc" || return 1
  OOPBOT_BLOCK=$2 awk '
    function flush() { for (i = 0; i < n; i++) print held[i]; n = 0 }
    $0 == "# >>> oopbot >>>" || $0 == "# >>> pipkin >>>" { flush(); inside = 1; held[n++] = $0; next }
    inside && ($0 == "# <<< oopbot <<<" || $0 == "# <<< pipkin <<<") { n = 0; inside = 0; if (!done) print ENVIRON["OOPBOT_BLOCK"]; done = 1; next }
    inside { held[n++] = $0; next }
    { print }
    END { flush(); if (!done) print ENVIRON["OOPBOT_BLOCK"] }
  ' "$rc" | write_file "$rc"
}

# The session OOPBoT opens to start the helper runs these hooks; start.sh does the rest.
# shellcheck disable=SC2016 # $HOME expands when the shell runs the hook.
sh_hook='# >>> oopbot >>>
oopbot_close=
for oopbot_start in "$HOME/.oopbot/helper/start.sh" "$HOME/.oopbot-dev/helper/start.sh"; do
  if [ -x "$oopbot_start" ]; then
    "$oopbot_start" --shell
    # 3: this session started the helper.
    if [ $? = 3 ]; then oopbot_close=1; fi
  fi
done
unset oopbot_start
# A session that only started helpers closes, which keeps Termux under its limit of 8 sessions.
if [ -n "$oopbot_close" ]; then exit 0; fi
unset oopbot_close
# <<< oopbot <<<'
put_block "$HOME/.bashrc" "$sh_hook" || fail "Couldn't update ~/.bashrc."
if [ -x "$PREFIX/bin/zsh" ] || [ "$login_shell" = zsh ]; then
  put_block "${ZDOTDIR:-$HOME}/.zshrc" "$sh_hook" || fail "Couldn't update ~/.zshrc."
fi
if [ -x "$PREFIX/bin/fish" ] || [ "$login_shell" = fish ]; then
  # A file of its own, since fish sources conf.d in every fish process, scripts and fish -c included. exit there would
  # only end the file, so the shell gives way to a command that exits 0 instead.
  fish_hook="${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/oopbot.fish"
  mkdir -p "${fish_hook%/*}" || fail "Couldn't create ${fish_hook%/*}."
  write_file "$fish_hook" <<'EOF' || fail "Couldn't write $fish_hook."
# Written by OOPBoT's Termux setup, which replaces this file each time it runs.
if status is-interactive
    set -l oopbot_close false
    for oopbot_start in "$HOME/.oopbot/helper/start.sh" "$HOME/.oopbot-dev/helper/start.sh"
        if test -x $oopbot_start
            $oopbot_start --shell
            # 3: this session started the helper.
            if test $status = 3
                set oopbot_close true
            end
        end
    end
    # A session that only started helpers closes, which keeps Termux under its limit of 8 sessions.
    if test $oopbot_close = true
        exec true
    end
end
EOF
fi

if [ "$(readlink "$PREFIX/bin/$name-start" 2>/dev/null)" != "$dir/start.sh" ]; then
  ln -sfn "$dir/start.sh" "$PREFIX/bin/$name-start" || fail "Couldn't create $name-start."
fi



# 4. Pairing.
node "$dir/helper.mjs" pair
case $? in
  0) ;;
  10)
    grep -qxE '[0-9a-f]{64}' "$dir/key" 2>/dev/null ||
      fail "This setup command has expired. In OOPBoT, tap Copy setup command again."
    ;;
  *) exit 1 ;;
esac

# 5. Start the helper, or restart it with the new files and key.
old=
if alive "$dir/helper.pid" helper.mjs; then
  old=$(cat "$dir/helper.pid")
  kill -USR1 "$old"
else
  rm -f "$dir/helper.port"
  "$dir/start.sh" || exit 1
fi
ready=false
for ((i = 1; i <= 40; i++)); do
  if [ -s "$dir/helper.port" ] && alive "$dir/helper.pid" helper.mjs && [ "$(cat "$dir/helper.pid")" != "$old" ]; then
    ready=true
    break
  fi
  # A helper that ran without run.sh doesn't come back after USR1.
  if [ "$i" = 12 ] && [ -n "$old" ] && ! alive "$dir/helper.pid" helper.mjs && ! alive "$dir/run.pid" run.sh; then
    "$dir/start.sh" || exit 1
  fi
  sleep 0.25
done
$ready || fail "The helper didn't start. Paste the command again."

# 6. Done; back to OOPBoT. Android blocks the switch when Termux isn't on screen, so say how to get there too.
echo "OOPBoT setup done. Switch back to OOPBoT if it doesn't open on its own."
am start --user 0 -a android.intent.action.MAIN -c android.intent.category.LAUNCHER \
  -n "$package/io.jeph.pipkin.MainActivity" >/dev/null 2>&1 || true

