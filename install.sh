#!/bin/sh
set -eu

usage() {
    printf '%s\n' \
        'Usage: ./install.sh [--user] [--build] [--no-init]' \
        '' \
        '  --user     Install to ~/.local/bin instead of /usr/local/bin.' \
        '  --build    Compile the locked Rust source instead of using bin/dotlab.' \
        '  --no-init  Install only; do not create/switch to the base profile.'
}

install_scope=system
build_from_source=0
initialize=1

while [ "$#" -gt 0 ]; do
    case "$1" in
        --user) install_scope=user ;;
        --build) build_from_source=1 ;;
        --no-init) initialize=0 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'install.sh: unknown option: %s\n' "$1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then
    printf '%s\n' 'install.sh: this release supports x86_64 Linux only' >&2
    exit 1
fi

if [ ! -e /etc/arch-release ] && [ "${DOTLAB_INSTALL_TEST_MODE:-0}" != 1 ]; then
    printf '%s\n' 'install.sh: Dotlab package management is supported only on Arch Linux' >&2
    exit 1
fi

if [ "$(id -u)" -eq 0 ] && [ "${DOTLAB_INSTALL_TEST_MODE:-0}" != 1 ]; then
    printf '%s\n' 'install.sh: run this installer as your normal user, not root' >&2
    exit 1
fi

for command in git install pacman sha256sum sudo; do
    if ! command -v "$command" >/dev/null 2>&1; then
        printf 'install.sh: required command not found: %s\n' "$command" >&2
        exit 1
    fi
done

if [ "$build_from_source" -eq 1 ]; then
    if ! command -v cargo >/dev/null 2>&1; then
        printf '%s\n' 'install.sh: --build requires cargo/rustc (install the rust package)' >&2
        exit 1
    fi
    (
        cd "$script_dir"
        cargo build --locked --release
    )
    source_binary=$script_dir/target/release/dotlab
else
    source_binary=$script_dir/bin/dotlab
    if [ ! -f "$source_binary" ]; then
        printf '%s\n' 'install.sh: bin/dotlab is missing; use --build in a source checkout' >&2
        exit 1
    fi
    (
        cd "$script_dir"
        sha256sum --check --strict SHA256SUMS
    )
fi

if [ "$install_scope" = user ]; then
    destination=$HOME/.local/bin/dotlab
    install -Dm755 -- "$source_binary" "$destination"
    metal_invocation="sudo $destination"
else
    destination=/usr/local/bin/dotlab
    temporary=/usr/local/bin/.dotlab-install-$$
    sudo install -Dm755 -- "$source_binary" "$temporary"
    sudo mv -f -- "$temporary" "$destination"
    metal_invocation='sudo dotlab'
fi

printf 'Installed %s to %s\n' "$("$destination" version)" "$destination"
"$destination" doctor

if [ "$initialize" -eq 1 ]; then
    "$destination" init
fi

printf '%s\n' \
    '' \
    'Installation is complete.' \
    'Add a profile:' \
    '  dotlab profile add NAME GIT_URL [--source DIR] [--map SRC=DEST] [--package PKG]' \
    'Preview and switch:' \
    '  dotlab switch NAME --dry-run' \
    '  dotlab switch NAME'
printf 'Check bare-metal slot support with:\n  %s metal preflight\n' "$metal_invocation"
