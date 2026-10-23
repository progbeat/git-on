#!/usr/bin/env python3
"""Reviewed correction of pinned cargo-dist 0.33.0 shell output; never publish.

Global generation must have both locally built archives available. Recompute
embedded SHA-256 from those verified inputs and fail on a changed template.
"""
import argparse
import hashlib
from pathlib import Path
import re

ARCHIVES = ('git-on-aarch64-apple-darwin.tar.gz', 'git-on-x86_64-unknown-linux-gnu.tar.gz')
VERIFY = r'''verify_checksum() {
    local _file="$1"
    local _checksum_style="$2"
    local _checksum_value="$3"
    local _calculated_checksum
    [ "$_checksum_style" = sha256 ] || err "required SHA-256 metadata is missing"
    [ "${#_checksum_value}" = 64 ] || err "malformed SHA-256 metadata"
    case $_checksum_value in *[!0-9a-f]*) err "malformed SHA-256 metadata";; esac
    if check_cmd sha256sum; then
        _calculated_checksum="$(sha256sum -b "$_file")" || err "SHA-256 verification failed"
    elif check_cmd shasum; then
        _calculated_checksum="$(shasum -a 256 -b "$_file")" || err "SHA-256 verification failed"
    else
        err "SHA-256 verification requires sha256sum or shasum -a 256"
    fi
    _calculated_checksum=${_calculated_checksum%% *}
    [ "$_calculated_checksum" = "$_checksum_value" ] || err "SHA-256 checksum mismatch"
}
'''


def harden(directory):
    directory = Path(directory)
    path = directory / 'git-on-installer.sh'
    source = path.read_text()
    if '"version":"0.33.0"' not in source or '# git-on: fail-closed SHA-256' in source:
        raise ValueError('expected unmodified pinned cargo-dist 0.33.0 installer')
    for name in ARCHIVES:
        digest = hashlib.sha256((directory / name).read_bytes()).hexdigest()
        # Upstream may already embed hashes when local manifests are available.
        # This inserts the verified archive hash immediately before selection ends.
        pattern = r'("' + re.escape(name) + r'"\)\n)(.*?)(            ;;)'
        def selection(match):
            body = re.sub(r'^            _checksum_(?:style|value)=.*\n', '', match[2], flags=re.M)
            return match[1] + body + f'            _checksum_style="sha256"\n            _checksum_value="{digest}"\n' + match[3]
        source, count = re.subn(pattern, selection, source, flags=re.S)
        if count != 1:
            raise ValueError('cargo-dist archive selection template changed: ' + name)
    start = source.index('verify_checksum() {\n')
    end = source.index('\ndownload_binary_and_run_installer "$@" || exit 1', start)
    source = source[:start] + '# git-on: fail-closed SHA-256 (see scripts/harden_dist_installer.py)\n' + VERIFY + source[end:]
    old = '''        if [ -n "${_checksum_style:-}" ]; then
            verify_checksum "$_file" "$_checksum_style" "$_checksum_value"
        else
            say "no checksums to verify" 1>&2
        fi'''
    if source.count(old) != 1:
        raise ValueError('cargo-dist verification call template changed')
    source = source.replace(old, '        verify_checksum "$_file" "${_checksum_style:-}" "${_checksum_value:-}"')
    # Upstream only cleans download directories on success. Ordinary failures
    # and interruption must also clean up destination staging. Reset these
    # globals before registering the trap: inherited values are not ours.
    old = '        _dir="$(ensure mktemp -d)" || return 1'
    if source.count(old) != 1:
        raise ValueError('cargo-dist temporary directory template changed')
    source = source.replace(old, old + '''
        GIT_ON_CLEANUP_DIR=$_dir
        _install_temp=
        _lib_install_temp=
        trap 'rm -rf "$GIT_ON_CLEANUP_DIR" "$_install_temp" "$_lib_install_temp"' 0
        trap 'exit 129' HUP
        trap 'exit 130' INT
        trap 'exit 143' TERM''')
    path.write_text(source)
    # Correct generation metadata after applying the documented correction.
    for name in (*ARCHIVES, 'git-on-installer.sh'):
        file = directory / name
        (directory / (name + '.sha256')).write_text(hashlib.sha256(file.read_bytes()).hexdigest() + ' *' + name + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path)
    harden(parser.parse_args().directory)
