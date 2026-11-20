#!/usr/bin/env bash
# Write a deb822 source with the supplied ASCII-armored public key.
set -euo pipefail
if [[ $# != 1 || $1 == *[[:space:]]* || ( $1 != https://* && $1 != file:/* ) ]]; then
    echo "Usage: $0 ARCHIVE_URI < PUBLIC_KEY.asc" >&2
    exit 2
fi
key=$(cat)
if [[ $key != '-----BEGIN PGP PUBLIC KEY BLOCK-----'$'\n'* ]]; then
    echo "Expected an ASCII-armored OpenPGP public key" >&2
    exit 2
fi
printf 'Types: deb\nURIs: %s/\nSuites: ./\nArchitectures: amd64\nSigned-By:\n' "${1%/}"
sed -e 's/^$/./' -e 's/^/  /' <<<"$key"
