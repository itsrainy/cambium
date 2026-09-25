#!/bin/sh
# Install cambi from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/itsrainy/cambium/main/scripts/install.sh | sh
#   curl -fsSL …/install.sh | sh -s -- --version v0.1.0 --prefix /usr/local
#
# Options:
#   --version vX.Y.Z   install that release (default: the latest one)
#   --prefix DIR       install to DIR/bin (default: $HOME/.local, so ~/.local/bin)
#   --target TRIPLE    install that build instead of the host's
#   --from-dir DIR     take the tarball and SHA256SUMS from DIR, not the network
#
# The download is checked against the release's SHA256SUMS before anything is
# installed. Builds exist for Linux (x86_64, aarch64; glibc) and macOS
# (x86_64, arm64). Windows is not supported.
set -eu

repo="itsrainy/cambium"

die() {
    echo "install: $*" >&2
    exit 1
}
say() {
    echo "install: $*" >&2
}

version=""
prefix="${HOME:?HOME is not set}/.local"
target=""
from_dir=""
while [ $# -gt 0 ]; do
    case "$1" in
    --version) version="${2:?--version needs a version}"; shift 2 ;;
    --prefix) prefix="${2:?--prefix needs a directory}"; shift 2 ;;
    --target) target="${2:?--target needs a triple}"; shift 2 ;;
    --from-dir) from_dir="${2:?--from-dir needs a directory}"; shift 2 ;;
    -h | --help)
        echo "usage: install.sh [--version vX.Y.Z] [--prefix DIR] [--target TRIPLE] [--from-dir DIR]"
        exit 0
        ;;
    *) die "unknown argument: $1" ;;
    esac
done

host_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$arch" in
    x86_64 | amd64) arch=x86_64 ;;
    arm64 | aarch64) arch=aarch64 ;;
    *) die "no cambi build for the $arch architecture" ;;
    esac
    case "$os" in
    Linux) echo "$arch-unknown-linux-gnu" ;;
    Darwin)
        # A shell under Rosetta reports x86_64 on Apple silicon; the native
        # build is the one to install there.
        if [ "$arch" = x86_64 ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = 1 ]; then
            arch=aarch64
        fi
        echo "$arch-apple-darwin"
        ;;
    *) die "no cambi build for $os (Linux and macOS only)" ;;
    esac
}

fetch() { # URL OUTPUT
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        die "need curl or wget to download"
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        die "need sha256sum or shasum to check the download"
    fi
}

[ -n "$target" ] || target=$(host_target)

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

if [ -n "$from_dir" ]; then
    [ -d "$from_dir" ] || die "no directory $from_dir"
    if [ -z "$version" ]; then
        # The one tarball for this target in the directory names the version.
        found=$(cd "$from_dir" && ls cambi-*-"$target".tar.gz 2>/dev/null || true)
        [ -n "$found" ] || die "no cambi-*-$target.tar.gz in $from_dir"
        [ "$(echo "$found" | wc -l | tr -d ' ')" = 1 ] ||
            die "several versions for $target in $from_dir; pick one with --version"
        version=${found#cambi-}
        version=${version%-"$target".tar.gz}
    fi
fi

if [ -z "$version" ]; then
    fetch "https://api.github.com/repos/$repo/releases/latest" "$tmp/latest.json" ||
        die "could not look up the latest release of $repo"
    version=$(sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' "$tmp/latest.json" | head -n 1)
    [ -n "$version" ] || die "the latest release of $repo names no tag"
fi
version=${version#v}
asset="cambi-$version-$target.tar.gz"

if [ -n "$from_dir" ]; then
    [ -f "$from_dir/$asset" ] || die "no $asset in $from_dir"
    [ -f "$from_dir/SHA256SUMS" ] || die "no SHA256SUMS in $from_dir"
    cp "$from_dir/$asset" "$from_dir/SHA256SUMS" "$tmp/"
else
    base="https://github.com/$repo/releases/download/v$version"
    say "downloading $asset from $base"
    fetch "$base/$asset" "$tmp/$asset" || die "could not download $base/$asset"
    fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "could not download $base/SHA256SUMS"
fi

want=$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS" | head -n 1)
[ -n "$want" ] || die "SHA256SUMS has no line for $asset"
got=$(sha256 "$tmp/$asset")
[ "$got" = "$want" ] || die "checksum mismatch for $asset: expected $want, got $got"
say "checksum ok ($got)"

mkdir "$tmp/x"
tar -xzf "$tmp/$asset" -C "$tmp/x"
bin="$tmp/x/cambi-$version-$target/cambi"
[ -f "$bin" ] || die "$asset holds no cambi-$version-$target/cambi"

bindir="$prefix/bin"
mkdir -p "$bindir"
# Copy then rename, so a running cambi is never overwritten in place.
cp "$bin" "$bindir/.cambi.tmp.$$"
chmod 755 "$bindir/.cambi.tmp.$$"
mv -f "$bindir/.cambi.tmp.$$" "$bindir/cambi"
say "installed $bindir/cambi"

if "$bindir/cambi" --version >/dev/null 2>&1; then
    "$bindir/cambi" --version
else
    say "$bindir/cambi does not run on this machine (was --target right?)"
fi
case ":${PATH:-}:" in
*":$bindir:"*) ;;
*) say "$bindir is not on PATH; add it to run cambi by name" ;;
esac
