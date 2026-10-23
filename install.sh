#!/bin/sh
git_on_install() {
    set -eu
    version=latest
    # Rotate unconsumed arguments to preserve their order and exact bytes.
    remaining=$#
    while [ "$remaining" -gt 0 ]; do
        argument=$1; shift
        remaining=$((remaining - 1))
        case $argument in
            --version|--bin-dir)
                [ "$remaining" -gt 0 ] || { printf '%s requires a value\n' "$argument" >&2; exit 2; }
                case $argument in
                    --version) version=$1;;
                    --bin-dir) export GIT_ON_INSTALL_DIR="$1";;
                esac
                shift; remaining=$((remaining - 1));;
            --version=*) version=${argument#--version=};;
            --bin-dir=*) export GIT_ON_INSTALL_DIR="${argument#--bin-dir=}";;
            *) set -- "$@" "$argument";;
        esac
    done
    case $version in
        latest) release=latest/download;;
        *) release=download/v${version#v};;
    esac
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL \
        "https://github.com/progbeat/git-on/releases/$release/git-on-installer.sh" | sh -s -- "$@"
}
{ git_on_install "$@"; }
