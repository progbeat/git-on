#!/bin/sh
# Download the reviewed cargo-dist tool into a caller-owned output directory.
set -eu
case $(uname -s):$(uname -m) in
    Darwin:arm64) target=aarch64-apple-darwin; expected=7b3cbe25511de01d74c0f5fcb7909edabd379bea9cfa284d93af5a3cdfa3247c;;
    Linux:x86_64) target=x86_64-unknown-linux-gnu; expected=4b3f0a5f0ebbdb798f6db649d01b32ba1518376b6f7a0502b7d92b75cc2c8293;;
    *) echo 'unsupported cargo-dist build host' >&2; exit 1;;
esac
out=$1
mkdir -p "$out"
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL \
    "https://github.com/axodotdev/cargo-dist/releases/download/v0.33.0/cargo-dist-$target.tar.xz" -o "$out/tool.tar.xz"
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$out/tool.tar.xz")
else
    actual=$(shasum -a 256 "$out/tool.tar.xz")
fi
[ "${actual%% *}" = "$expected" ] || { echo 'cargo-dist SHA-256 mismatch' >&2; exit 1; }
tar -xJf "$out/tool.tar.xz" -C "$out"
cp "$out/cargo-dist-$target/dist" "$out/dist"
"$out/dist" --version
