#!/bin/sh
# Define the whole payload before invoking it, including when piped to sh.
git_on_install() {
set -eu
LC_ALL=C
export LC_ALL
fail() { printf 'git-on installer: %s\n' "$*" >&2; exit 1; }
version_ok() {
    printf '%s\n' "$1" | awk '
        NR!=1 {bad=1}
        !/^[0-9]+\.[0-9]+\.[0-9]+$/ {bad=1}
        /^[0-9]+\.[0-9]+\.[0-9]+$/ {
            n=split($0,a,"."); for(i=1;i<=n;i++) if(length(a[i])>1 && substr(a[i],1,1)=="0") exit 1
            ok=1
        }
        END { if(!ok || bad) exit 1 }'
}
version=
version_requested=0
bin_dir=${HOME:-}/.local/bin
use_gh=0
while [ "$#" -gt 0 ]; do
    case $1 in
        --version|--bin-dir)
            [ "$#" -ge 2 ] || fail "$1 needs a value"
            case $1 in --version) version=${2#v}; version_requested=1;; --bin-dir) bin_dir=$2;; esac
            shift 2;;
        --github-cli) use_gh=1; shift;;
        -h|--help)
            printf '%s\n' 'Usage: sh install.sh [--version VERSION] [--bin-dir /absolute/path] [--github-cli]' \
                'Install/update git-on for Linux x86_64, glibc 2.35+, without sudo.'
            exit 0;;
        *) fail "Unknown argument: $1";;
    esac
done
# Preserve relative/empty PATH entries when pinning the destination with cd.
initial_pwd=$(pwd -P)
remaining_path=${PATH:-}
absolute_path=
separator=
while :; do
    entry=${remaining_path%%:*}
    case $entry in /*) ;; *) entry=$initial_pwd/${entry:-.};; esac
    absolute_path=$absolute_path$separator$entry
    separator=:
    case $remaining_path in *:*) remaining_path=${remaining_path#*:};; *) break;; esac
done
PATH=$absolute_path
export PATH
for tool in awk cat chmod getconf gzip id mkdir mktemp mv rm sha256sum stat tar timeout uname; do
    command -v "$tool" >/dev/null 2>&1 || fail "Required tool is missing: $tool (GNU coreutils, tar and gzip are needed)"
done
if [ "$version_requested" -eq 1 ]; then
    version_ok "$version" || fail '--version must be a stable version such as 1.2.0 or v1.2.0'
fi
[ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ] || fail 'Prebuilt releases support Linux x86_64 only'
uid=$(id -u)
[ "$uid" != 0 ] || fail 'Run this installer as your normal user, without sudo'
libc=$(getconf GNU_LIBC_VERSION 2>/dev/null || :)
printf '%s\n' "$libc" | awk '$1=="glibc" {split($2,v,"."); if(v[1]>2 || (v[1]==2 && v[2]>=35)) ok=1} END {exit !ok}' ||
    fail 'This binary needs glibc 2.35+; musl/Alpine is not supported'
case $bin_dir in /*) ;; *) fail '--bin-dir must be an absolute path';; esac
case /${bin_dir#/}/ in */../*|*/./*) fail '--bin-dir must not contain . or .. components';; esac
case $bin_dir in /|*//*|*/) fail '--bin-dir must be a canonical absolute path without repeated/trailing slashes';; esac
# Reject symlink components before entering the directory. Once inside, use
# relative paths so renaming an ancestor cannot redirect staging or replacement.
rest=${bin_dir#/}
prefix=
while [ -n "$rest" ]; do
    part=${rest%%/*}
    case $rest in */*) rest=${rest#*/};; *) rest=;; esac
    [ -n "$part" ] || continue
    prefix=$prefix/$part
    [ ! -L "$prefix" ] || fail "Refusing a symlink in installation directory: $prefix"
    if [ -e "$prefix" ] && [ ! -d "$prefix" ]; then fail "Not a directory: $prefix"; fi
done
umask 077
mkdir -p -- "$bin_dir" || fail "Cannot create $bin_dir; choose an accessible --bin-dir"
cd -P -- "$bin_dir" || fail "Cannot enter $bin_dir"
[ "$(pwd -P)" = "$bin_dir" ] || fail 'Installation directory changed or became a symlink; existing installation is unchanged'
owner=$(stat -c %u .)
mode=$(stat -c %a .)
[ "$owner" = "$uid" ] && [ "$((0$mode & 022))" -eq 0 ] ||
    fail "Installation directory $bin_dir has mode $mode, uid $owner; use a directory you own without group/other write permission (--bin-dir)"
[ -w . ] && [ -x . ] || fail "Cannot stage in $bin_dir (mode $mode); owner-write and search permission are needed; choose --bin-dir"
check_target() {
    [ ! -L git-on ] && { [ ! -e git-on ] || [ -f git-on ]; } ||
        fail 'Refusing to replace a symlink or non-regular git-on destination'
}
check_target
stage=$(mktemp -d .git-on-install.XXXXXXXXXX) || fail 'Cannot create staging directory; existing installation is unchanged'
probe_pid=
cleanup() {
    if [ -n "$probe_pid" ]; then
        # The interrupted wait still owns this unreaped timeout child. Stop
        # its leader and private process group before removing probe output.
        kill -KILL "$probe_pid" 2>/dev/null || :
        kill -KILL "-$probe_pid" 2>/dev/null || :
        wait "$probe_pid" 2>/dev/null || :
    fi
    rm -rf -- "$stage"
}
trap cleanup 0
trap 'fail "Installation interrupted; removing temporary files"' HUP INT TERM
base=https://github.com/progbeat/git-on/releases
if [ "$use_gh" -eq 1 ]; then
    command -v gh >/dev/null 2>&1 || fail '--github-cli requires an already authenticated GitHub CLI (gh)'
    endpoint=latest
    [ -z "$version" ] || endpoint=tags/v$version
    # --jq is built into gh; no external jq executable is needed.
    tag=$(gh api --hostname github.com "repos/progbeat/git-on/releases/$endpoint" \
        --jq 'select(.draft == false and .prerelease == false) | .tag_name') || fail 'GitHub CLI release lookup failed'
else
    command -v curl >/dev/null 2>&1 || fail 'curl is required for public downloads'
    if [ -n "$version" ]; then
        tag=v$version
    else
        resolved=$(curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL \
            --connect-timeout 15 --max-time 180 --max-filesize 1048576 -o /dev/null -w '%{url_effective}' "$base/latest") ||
            fail 'Cannot resolve the latest release; check network access or use --github-cli'
        case $resolved in "$base/tag/"*) tag=${resolved#"$base/tag/"};; *) fail 'Unexpected latest release URL';; esac
    fi
fi
case $tag in v*) version_ok "${tag#v}" || fail 'Release must have a stable version tag';; *) fail 'Release must have a stable version tag';; esac
[ -z "$version" ] || [ "$tag" = "v$version" ] || fail 'Release does not match --version'
name=git-on-$tag-x86_64-unknown-linux-gnu
archive=$name.tar.gz
# Latest is never used below: both downloads are pinned to the same tag.
download() {
    if [ "$use_gh" -eq 1 ]; then
        gh release download "$tag" --repo https://github.com/progbeat/git-on --pattern "$1" --dir "$stage" ||
            fail 'GitHub CLI download failed; existing installation is unchanged'
    else
        curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL --connect-timeout 15 --max-time 180 \
            --max-filesize "$2" "$base/download/$tag/$1" -o "$stage/$1" ||
            fail 'Download failed; existing installation is unchanged'
    fi
    [ -f "$stage/$1" ] && [ ! -L "$stage/$1" ] || fail 'Downloaded asset is missing or is not a regular file'
    [ "$(stat -c %s "$stage/$1")" -le "$2" ] || fail 'Downloaded asset exceeds its size limit'
}
printf 'Installing git-on %s ...\n' "$tag"
download SHA256SUMS 1048576
download "$archive" 134217728
# Select only the fixed archive name; never pass downloaded filenames/options to
# sha256sum. release.json remains published for legacy installers/build provenance.
hash=$(awk -v name="$archive" '
    NF!=2 || length($1)!=64 || $1~/[^0-9a-f]/ {bad=1}
    $2==name {hash=$1; count++}
    END {if(bad || count!=1) exit 1; print hash}' "$stage/SHA256SUMS") || fail 'Invalid or incomplete SHA256SUMS'
actual=$(sha256sum "$stage/$archive")
[ "${actual%% *}" = "$hash" ] || fail 'Checksum verification failed; existing installation is unchanged'
# Extract bytes, never archive paths. GNU tar does not emit symlink/hardlink
# targets with -O. Reject duplicate binary members before tar can concatenate them.
# Bound both decompression work and output size; there is no custom tar parser.
(
    ulimit -f 262144 || exit 1
    timeout --foreground --kill-after=2 60 tar -tzf "$stage/$archive" -- "$name/git-on" > "$stage/members" || exit 1
    awk -v name="$name/git-on" 'NR!=1 || $0!=name {bad=1} END {exit (NR!=1 || bad)}' "$stage/members" || exit 1
    timeout --foreground --kill-after=2 60 tar -xzOf "$stage/$archive" -- "$name/git-on" > "$stage/git-on" || exit 1
) || fail 'Cannot read the release binary from the verified archive'
[ -s "$stage/git-on" ] || fail 'Archive binary is empty'
chmod 755 "$stage/git-on"
# Keep the monitored wrapper alive until it has terminated its own private
# timeout group, including children left behind by a successful probe.
timeout --signal=KILL 15 /bin/sh -c '
    probe_status=0
    "$1" --version > "$2/version" 2>/dev/null || probe_status=$?
    printf "%s\n" "$probe_status" > "$2/status" || :
    kill -KILL 0
' sh "$stage/git-on" "$stage" 2>/dev/null &
probe_pid=$!
wait "$probe_pid" 2>/dev/null || :
probe_pid=
[ -f "$stage/status" ] && [ "$(cat "$stage/status")" = 0 ] ||
    fail 'Downloaded executable failed its version check; glibc 2.35+, libz.so.1 and libgcc_s.so.1 are needed; existing installation is unchanged'
reported=$(cat "$stage/version")
[ "$reported" = "git-on ${tag#v}" ] || fail 'Downloaded executable failed its version check; existing installation is unchanged'
check_target
mv -fT -- "$stage/git-on" git-on || fail 'Cannot replace git-on; existing installation is unchanged'
printf 'Installed git-on %s to %s/git-on\n' "$tag" "$bin_dir"
selected=$(command -v git-on || :)
if [ -n "$selected" ] && [ "$selected" -ef ./git-on ]; then
    :
else
    [ -z "$selected" ] || printf 'PATH selects %s before this installation.\n' "$selected"
    printf 'Installed executable: %s/git-on\nAdd the installation directory to PATH if needed.\n' "$bin_dir"
fi
if ! command -v fusermount3 >/dev/null 2>&1 || [ ! -c /dev/fuse ] || [ ! -r /dev/fuse ] || [ ! -w /dev/fuse ]; then
    printf '%s\n' 'Before running git on: provide fuse3 (fusermount3), a usable /dev/fuse and permission to mount FUSE.'
fi
}
{ git_on_install "$@"; }
