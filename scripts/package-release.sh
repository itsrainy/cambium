#!/bin/sh
# Build cambi for one target and package it for a release.
#
#   scripts/package-release.sh [--target TRIPLE] [--out DIR] [--tag vX.Y.Z]
#
# Writes, under DIR (default: dist/):
#
#   cambi-<version>-<target>.tar.gz          cambi, LICENSE and README.md,
#                                            under cambi-<version>-<target>/
#   cambi-<version>-<target>.tar.gz.sha256   one SHA256SUMS line for it
#   SHA256SUMS                               every .sha256 line in DIR
#
# A release's SHA256SUMS is the .sha256 lines concatenated, which is all
# .github/workflows/release.yml does with them; scripts/install.sh checks a
# download against that file, so `scripts/install.sh --from-dir DIR` installs
# from this script's output as it would from a release.
#
# The target defaults to the host. It must already be installed
# (`rustup target add TRIPLE`): this script builds, it does not fetch
# toolchains. With --tag, the tag must name the workspace version (v0.1.0 for
# 0.1.0), so a release cannot ship a binary that reports another version.
#
# The binary is built with the `dist` profile (Cargo.toml): the release
# profile, stripped and without debug info. When the target is the host, the
# packaged binary's `--version` is checked before anything is written.
set -eu

die() {
    echo "package-release: $*" >&2
    exit 1
}

target=""
out="dist"
tag=""
while [ $# -gt 0 ]; do
    case "$1" in
    --target) target="${2:?--target needs a triple}"; shift 2 ;;
    --out) out="${2:?--out needs a directory}"; shift 2 ;;
    --tag) tag="${2:?--tag needs a tag}"; shift 2 ;;
    -h | --help) sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

host=$(rustc -vV | sed -n 's/^host: //p')
[ -n "$target" ] || target="$host"

if command -v rustup >/dev/null 2>&1; then
    rustup target list --installed | grep -qx "$target" ||
        die "target $target is not installed; run: rustup target add $target"
fi

# `path+file:///…/crates/cli#cambium-cli@0.1.0` (or `…#0.1.0` from an older
# cargo): the version is whatever follows the last '@' or '#'.
pkgid=$(cargo pkgid -p cambium-cli)
version=${pkgid##*[@#]}
case "$version" in
[0-9]*.[0-9]*.[0-9]*) ;;
*) die "could not read the version from: $pkgid" ;;
esac
if [ -n "$tag" ] && [ "$tag" != "v$version" ]; then
    die "tag $tag does not name the workspace version $version (expected v$version)"
fi

cargo build --locked --profile dist -p cambium-cli --target "$target"

target_dir=${CARGO_TARGET_DIR:-$root/target}
bin="$target_dir/$target/dist/cambi"
[ -x "$bin" ] || die "no binary at $bin"

if [ "$target" = "$host" ]; then
    reported=$("$bin" --version)
    case "$reported" in
    "cambi $version ("*")") echo "package-release: $reported" ;;
    *) die "built binary reports '$reported', expected 'cambi $version (<sha>)'" ;;
    esac
fi

name="cambi-$version-$target"
mkdir -p "$out"
out=$(cd "$out" && pwd)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT INT TERM
mkdir "$stage/$name"
cp "$bin" "$stage/$name/cambi"
cp LICENSE README.md "$stage/$name/"
chmod 755 "$stage/$name/cambi"

# Sorted names and no owner or group, so the archive's listing does not
# depend on the machine that built it.
if tar --version 2>/dev/null | grep -q 'GNU tar'; then
    tar -C "$stage" --sort=name --owner=0 --group=0 --numeric-owner -cf - "$name" |
        gzip -9n >"$out/$name.tar.gz"
else
    tar -C "$stage" --uid 0 --gid 0 -cf - "$name" | gzip -9n >"$out/$name.tar.gz"
fi

if command -v sha256sum >/dev/null 2>&1; then
    sum=$(sha256sum "$out/$name.tar.gz" | cut -d' ' -f1)
else
    sum=$(shasum -a 256 "$out/$name.tar.gz" | cut -d' ' -f1)
fi
printf '%s  %s\n' "$sum" "$name.tar.gz" >"$out/$name.tar.gz.sha256"
cat "$out"/cambi-*.tar.gz.sha256 >"$out/SHA256SUMS"

size=$(wc -c <"$bin" | tr -d ' ')
echo "package-release: $out/$name.tar.gz (cambi $size bytes)"
cat "$out/$name.tar.gz.sha256"
