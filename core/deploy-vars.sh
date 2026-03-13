proj_dir=$(pwd)

xtask_dir=""

# 1. Walk up looking for a deps/esp-hal-app checkout (submodule style)
path=$(pwd)
while [ "$path" != "/" ]; do
    if [ -d "$path/deps/esp-hal-app" ]; then
        xtask_dir="$path/deps/esp-hal-app"
        break
    fi
    path=$(dirname "$path")
done

# 2. Fall back to the cargo git cache (populated when building with git = "..." dep)
if [ -z "$xtask_dir" ]; then
    cache_base="$HOME/.cargo/git/checkouts"
    if [ -d "$cache_base" ]; then
        xtask_dir=$(find "$cache_base" -maxdepth 3 -name "xtask" -type d 2>/dev/null \
            | grep "esp-hal-app" | head -1 | xargs dirname 2>/dev/null)
    fi
fi

if [ -z "$xtask_dir" ]; then
    echo "esp-hal-app not found (checked deps/ and ~/.cargo/git/checkouts)" >&2
    exit 1
fi

base_target_dir="$proj_dir/../build"

