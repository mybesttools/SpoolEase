base_target="spoolease-bin"
path_in_base_target="/bins/0.6"
product="console"

source ./deploy-vars.sh
source ./deploy-shell-init.sh

mkdir -p "$base_target_dir${path_in_base_target}/${product}/ota"
mkdir -p "$base_target_dir${path_in_base_target}/${product}/web-install"

pushd "${xtask_dir}"
"${CARGO_CMD}" xtask ota build --input "$proj_dir" --output "$base_target_dir${path_in_base_target}/${product}/ota"
"${CARGO_CMD}" xtask web-install build --input "$proj_dir" --output "$base_target_dir${path_in_base_target}/${product}/web-install"
popd
