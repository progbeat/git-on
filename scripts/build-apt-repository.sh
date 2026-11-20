#!/usr/bin/env bash
# Produce an unsigned flat APT repository from an immutable Git-on release.
set -euo pipefail

if [[ $# != 2 || ! $1 =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "Usage: $0 VERSION NEW_OUTPUT_DIRECTORY" >&2
    exit 2
fi
version=$1
output=$2
base=https://github.com/progbeat/git-on/releases/download/v${version}
asset=git-on-x86_64-unknown-linux-gnu.tar.gz
pkg=git-on_${version}_amd64.deb
origin=https://api.github.com/repos/progbeat/git-on/releases/tags/v${version}

work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
curl_https() { curl --proto '=https' -fsSL --retry 3 "$@"; }

# Authenticate the exact released archive before extracting or repackaging it.
release=$(curl_https "$origin")
digest=$(jq -er --arg asset "$asset" --arg base "$base" '
    select(.immutable == true and .draft == false and .prerelease == false)
    | [.assets[] | select(.name == $asset and .browser_download_url == ($base + "/" + $asset))]
    | select(length == 1)
    | .[0].digest | select(test("^sha256:[a-f0-9]{64}$")) | ltrimstr("sha256:")
' <<<"$release")
published_at=$(jq -er '.published_at | strings' <<<"$release")
export SOURCE_DATE_EPOCH
SOURCE_DATE_EPOCH=$(date -u -d "$published_at" +%s)
curl_https "$base/$asset" -o "$work/$asset"
(cd "$work"; printf '%s  %s\n' "$digest" "$asset" | sha256sum --check --strict)

# The packaged program needs system Git, fusermount3 and the ELF-linked
# libraries. Running a lazy snapshot still needs a usable /dev/fuse.
tar -xzf "$work/$asset" -C "$work" --strip-components=1 -- "${asset%.tar.gz}"/{git-on,LICENSE,THIRD_PARTY_NOTICES}
depends="git, fuse3, libc6 (>= 2.34), libgcc-s1, zlib1g"
# Do not publish an apparently installable package if a future release adds an
# unmapped shared library. Some distro releases renamed libcurl4 for time64.
LC_ALL=C readelf --dynamic "$work/git-on" > "$work/elf"
while IFS= read -r soname; do
    case "$soname" in
        ld-linux-x86-64.so.2|libc.so.6|libm.so.6|libgcc_s.so.1|libz.so.1) ;;
        libcurl.so.4) depends+=", libcurl4 | libcurl4t64" ;;
        *) echo "Unmapped Git-on runtime library: $soname" >&2; exit 1 ;;
    esac
done < <(sed -n 's/.*(NEEDED).*Shared library: \[\([^]]*\)\]/\1/p' "$work/elf")
root="$work/package"
install -D -m755 "$work/git-on" "$root/usr/bin/git-on"
install -D -m644 "$work/LICENSE" "$root/usr/share/doc/git-on/copyright"
install -D -m644 "$work/THIRD_PARTY_NOTICES" "$root/usr/share/doc/git-on/THIRD_PARTY_NOTICES"
mkdir -p "$root/DEBIAN"
cat > "$root/DEBIAN/control" <<CONTROL
Package: git-on
Version: $version
Architecture: amd64
Maintainer: Git-on maintainers <11060353+progbeat@users.noreply.github.com>
Section: vcs
Priority: optional
Depends: $depends
Homepage: https://github.com/progbeat/git-on
Description: Run commands on a specific Git revision
 Git-on exposes an immutable Git snapshot without changing the working tree
 or index. Writes to the temporary snapshot are discarded after the command.
CONTROL

mkdir -- "$output"
dpkg-deb --root-owner-group --build "$root" "$output/$pkg"
(
    cd "$output"
    dpkg-scanpackages --arch amd64 . > Packages
    gzip -n -k Packages
    # Generate into a sibling temporary file so Release never hashes itself.
    apt-ftparchive -o APT::FTPArchive::Release::Origin=Git-on \
        -o APT::FTPArchive::Release::Label=Git-on \
        -o APT::FTPArchive::Release::Architectures=amd64 release . > "$work/Release"
    mv -- "$work/Release" Release
)
