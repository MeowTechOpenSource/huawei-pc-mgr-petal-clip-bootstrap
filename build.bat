if not exist dist mkdir dist

cargo +nightly-x86_64-pc-windows-msvc build --release -p version --target=x86_64-pc-windows-msvc
cargo +nightly-x86_64-pc-windows-msvc build --release -p huawei-pc-manager-bootstrap-core -p huawei-pc-manager-bootstrap

copy target\i686-pc-windows-msvc\release\huawei_pc_manager_bootstrap_core.dll dist
copy target\i686-pc-windows-msvc\release\huawei-pc-manager-bootstrap.exe dist
