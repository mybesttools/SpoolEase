#!/usr/bin/env sh

# Initialize shell/toolchain environment for non-interactive script runs.
# Optional override: export SE_STARTUP_SCRIPT=/path/to/startup.sh
if [ -n "${SE_STARTUP_SCRIPT:-}" ] && [ -f "$SE_STARTUP_SCRIPT" ]; then
  # shellcheck disable=SC1090
  . "$SE_STARTUP_SCRIPT"
fi

# Common Rust/cargo initialization locations.
for f in "$HOME/.cargo/env" "$HOME/.zprofile" "$HOME/.zshrc"; do
  if [ -f "$f" ]; then
    # shellcheck disable=SC1090
    . "$f" >/dev/null 2>&1 || true
  fi
done

# Add known fallback cargo bin path used in this environment.
if [ -d "$HOME/Library/Caches/puccinialin/cargo/bin" ]; then
  PATH="$HOME/Library/Caches/puccinialin/cargo/bin:$PATH"
  export PATH
fi

if command -v cargo >/dev/null 2>&1; then
  CARGO_CMD="$(command -v cargo)"
  export CARGO_CMD
else
  echo "cargo not found after startup bootstrap" >&2
  exit 1
fi
