#!/usr/bin/env bash
# An existing archive version is a lower bound for publication.
set -euo pipefail
version=$1
mapfile -t versions < <(sed -n 's/^Version:[[:blank:]]*//p' "$2")
pattern='^[0-9]+\.[0-9]+\.[0-9]+$'
if [[ ${#versions[@]} != 1 || ! $version =~ $pattern || ! ${versions[0]} =~ $pattern ]]; then
    echo "Could not determine a single valid APT archive version." >&2
    exit 1
fi
published=${versions[0]}
if dpkg --compare-versions "$version" lt "$published"; then
    echo "Refusing to downgrade the APT archive from $published to $version." >&2
    exit 1
fi
