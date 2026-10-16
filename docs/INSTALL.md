# Installing cambi

## Prebuilt binaries

```console
$ curl -fsSL https://raw.githubusercontent.com/itsrainy/cambium/main/scripts/install.sh | sh
$ curl -fsSL https://raw.githubusercontent.com/itsrainy/cambium/main/scripts/install.sh | sh -s -- --version v0.1.0 --prefix /usr/local
```

No release exists yet, so today these commands fail; build from a clone
(see [From source](#from-source)) instead. When `curl` fails, `curl ... | sh` still exits 0, because
`sh` runs the empty script it was given: in a bash script or CI step, run
`set -o pipefail` first so a failed download fails the step, as the
templates in [docs/ci/](ci/) do. Older POSIX shells (dash before
0.5.12) reject `pipefail`; there, download the script to a file with
`curl -fsSL -o install.sh ...` and run `sh install.sh` only if that
succeeded.

The script picks the build for the machine, checks the download against the
release's `SHA256SUMS`, and installs `cambi` to `~/.local/bin` (or
`<prefix>/bin`); it exits non-zero when there is no tarball to download.
The latest release is the default; `--version` pins one, which is what a
CI job should do. The tarballs and `SHA256SUMS` are on the release page for
a download by hand.

## From source

With Rust 1.98 or newer (the workspace `rust-version`) and a C
compiler (`blake3`, `ring` and `zstd` build C code):

```console
$ cargo install --git https://github.com/itsrainy/cambium cambium-cli --locked
$ cargo install --path crates/cli --locked    # from a clone
$ cambi --version
cambi 0.1.0 (0a4a001)
```

`cargo install --git` works only once the repository is public; until then
it cannot fetch it, and the from-a-clone line is the route that works.

The crate is `cambium-cli`, the binary `cambi`. The crate named `cambi` on
crates.io is an unrelated project. `--version` names the commit the binary
was built from, or `unknown` for a source tree with neither git metadata nor
cargo's packaged-crate metadata. The benchmark harness is a second binary,
`cambium-bench`, from `cambium-bench-cli`; nothing in it is part of the
[JSON contract](CONTRACT.md).

No `protoc` needed: the vendored `build.proto` is compiled by `protox`, a
pure-Rust protobuf compiler.

Platforms: Linux on x86_64 and aarch64 (glibc), macOS on x86_64 and arm64.
Windows is not supported.

## At run time

On `PATH`:

- `bazelisk` or `bazel`, needed to run and not to build. cambi prefers
  `bazelisk` when both are there, and `bazel.binary` in `cambium.toml`
  names another;
- `git`, which cambi runs for the diff and the revisions;
- for a `command` store only, whatever its templates run (typically `aws` or
  `gsutil`, see [docs/STORE.md](STORE.md#s3-and-gcs)).
