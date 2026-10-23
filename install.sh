#!/bin/sh
# Download a release installer completely, then run it locally.
git_on_install() (
    set -eu
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
            -h|--help) printf '%s\n' 'Usage: sh install.sh [--version VERSION] [--bin-dir /absolute/path]'; exit 0;;
            *) fail "unknown option: $1";;
        esac
        shift
    done
    case $bin_dir in /*) ;; *) fail '--bin-dir must be an absolute path';; esac
    case $bin_dir/ in *//*|*/./*|*/../*) fail '--bin-dir must be a canonical absolute path';; esac
    if [ "$version_selected" = 0 ]; then
        latest=$(curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -o /dev/null -w '%{url_effective}' https://github.com/progbeat/git-on/releases/latest)
        case $latest in
            https://github.com/progbeat/git-on/releases/tag/*) version=${latest##*/};;
            *) fail 'GitHub did not resolve a stable release';;
        esac
    fi
    version=${version#v}
    printf '%s\n' "$version" | awk '
        NR != 1 || !/^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/ {exit 1}
        {split($0,p,"."); if(p[1]<1 || (p[1]==1 && p[2]<3)) exit 1}
    ' || fail 'only stable git-on 1.3.0 and later are supported; no supported release at the selected version'
    temporary=$(mktemp -d "${TMPDIR:-/tmp}/git-on-installer.XXXXXXXX")
    trap 'rm -rf "$temporary"' 0
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL \
        "https://github.com/progbeat/git-on/releases/download/v$version/git-on-installer.sh" \
        -o "$temporary/installer.sh"
    GIT_ON_INSTALL_DIR="$bin_dir" GIT_ON_NO_MODIFY_PATH=1 GIT_ON_DISABLE_UPDATE=1 \
        sh "$temporary/installer.sh"
)
# A truncated bootstrap must not invoke its payload.
{ git_on_install "$@"; }
