#!/bin/sh
set -eu

archive=${1:?Usage: tests/archive-smoke.sh PATH_TO_DOTLAB_TAR_GZ}
temporary=$(mktemp -d)
cleanup() {
    rm -rf -- "$temporary"
}
trap cleanup EXIT HUP INT TERM

tar -xzf "$archive" -C "$temporary"
project=$(find "$temporary" -mindepth 1 -maxdepth 1 -type d -name 'dotlab-*' -print -quit)
[ -n "$project" ]

"$project/bin/dotlab" version
"$project/bin/dotlab" --help >/dev/null
"$project/bin/dotlab" metal promote --help >/dev/null

mkdir -p "$temporary/home" "$temporary/fakebin"
cat > "$temporary/fakebin/pacman" <<'EOF'
#!/bin/sh
exit 0
EOF
cat > "$temporary/fakebin/sudo" <<'EOF'
#!/bin/sh
exec "$@"
EOF
chmod 755 "$temporary/fakebin/pacman" "$temporary/fakebin/sudo"

HOME=$temporary/home \
PATH=$temporary/fakebin:$PATH \
DOTLAB_INSTALL_TEST_MODE=1 \
"$project/install.sh" --user --no-init

"$temporary/home/.local/bin/dotlab" version
"$temporary/home/.local/bin/dotlab" metal promote --help >/dev/null
