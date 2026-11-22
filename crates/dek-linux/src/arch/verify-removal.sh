#!/usr/bin/sh
set -eu
export LC_ALL=C

fail() {
    printf 'dek: %s\n' "$*" >&2
    exit 1
}

{
    IFS= read -r package
    IFS= read -r version
} < "$1" || fail 'cannot read the reviewed removal'
IFS= read -r target || fail 'pacman provided no removal target'
if [ "$target" != "$package" ] || IFS= read -r extra; then
    fail "pacman removal targets differ from reviewed package $package"
fi
installed=$(/usr/bin/pacman --query --color never -- "$package") ||
    fail "cannot verify installed version of $package inside the transaction"
[ "$installed" = "$package $version" ] ||
    fail "reviewed removal expected $package $version, found $installed"
