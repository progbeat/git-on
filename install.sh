#!/bin/sh
git_on_install() {
    release=latest/download
    case ${GIT_ON_VERSION:-latest} in
        latest) ;;
        *) release=download/v${GIT_ON_VERSION#v};;
    esac
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL \
        "https://github.com/progbeat/git-on/releases/$release/git-on-installer.sh" | sh -s -- "$@"
}
{ git_on_install "$@"; }
