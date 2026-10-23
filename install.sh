#!/bin/sh
# Thin dispatcher for cargo-dist release installers (git-on 1.3.0+).
git_on_install() (
    set -u
    umask 077
    fail() { printf 'git-on: %s\n' "$*" >&2; exit 1; }
    version=''
    version_selected=0
    bin_dir=${HOME:?HOME must be set}/.local/bin
    while [ "$#" -gt 0 ]; do
        case $1 in
            --version|--bin-dir)
                option=$1; shift
                [ "$#" -gt 0 ] || fail "$option requires a value"
                case $option in --version) version=$1; version_selected=1;; --bin-dir) bin_dir=$1;; esac;;
            --version=*) version=${1#*=}; version_selected=1;;
            --bin-dir=*) bin_dir=${1#*=};;
            --github-cli) fail '--github-cli was removed in 1.3.0; use public HTTPS release downloads, or download and inspect the release installer with your authenticated gh account, then run it with GIT_ON_INSTALL_DIR';;
            -h|--help) printf '%s\n' 'Usage: sh install.sh [--version VERSION] [--bin-dir /absolute/path]' 'Supports git-on 1.3.0 and later. Default: latest stable release.'; exit 0;;
            *) fail "unknown option: $1";;
        esac
        shift
    done
    case $bin_dir in /*) ;; *) fail '--bin-dir must be an absolute path';; esac
    case $bin_dir/ in *//*|*/./*|*/../*) fail '--bin-dir must be a canonical absolute path';; esac
    supported_version() {
        # Stable semver only, optional v; no URLs, shell syntax or prereleases.
        printf '%s\n' "${1#v}" | awk '
            !/^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/ {exit 1}
            {split($0,p,"."); if(p[1]<1 || (p[1]==1 && p[2]<3)) exit 2}'
    }
    check_version() {
        supported_version "$1"
        code=$?
        case $code in 0) ;; 2) fail 'only git-on 1.3.0 and later are supported; no supported release is available at the selected version';; *) fail "invalid stable version: $1";; esac
    }
    if [ "$version_selected" = 1 ]; then
        check_version "$version"
    else
        latest=$(curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -o /dev/null -w '%{url_effective}' https://github.com/progbeat/git-on/releases/latest) || exit "$?"
        case $latest in
            https://github.com/progbeat/git-on/releases/tag/*) version=${latest##*/};;
            *) fail 'GitHub did not resolve a stable release';;
        esac
        check_version "$version"
    fi
    tag=v${version#v}
    temporary=$(mktemp -d "${TMPDIR:-/tmp}/git-on-installer.XXXXXXXX") || exit "$?"
    trap 'rm -rf "$temporary"' 0
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM
    # Download completion is a prerequisite for execution. HTTPS/GitHub is the
    # trust boundary; the release installer verifies its selected archive.
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL \
        "https://github.com/progbeat/git-on/releases/download/$tag/git-on-installer.sh" \
        -o "$temporary/installer.sh" || exit "$?"
    GIT_ON_INSTALL_DIR="$bin_dir" GIT_ON_NO_MODIFY_PATH=1 GIT_ON_DISABLE_UPDATE=1 \
        sh "$temporary/installer.sh"
)
# A truncated bootstrap must not invoke its payload.
git_on_install "$@"
