proj_dir=$(pwd)

path=$(pwd)
xtask_dir=""
while [ "$path" != "/" ]; do
    if [ -d "$path/deps/esp-hal-app" ]; then
        xtask_dir="$path/deps/esp-hal-app"
        break
    fi
    path=$(dirname "$path")
done
if [ -z "$xtask_dir" ]; then
    echo "esp-hal-app not found" >&2
    exit 1
fi

base_target_dir="$proj_dir/../build"

