#!/bin/sh

TOP=$(dirname "$0")/..

TAG="$1"

if [ -z "$TAG"]; then
    echo "You need to give a git revision as an argument."
    exit 1
fi

for crate in $(cd "${TOP}/crates/" && ls); do
    if git diff --quiet "$TAG..HEAD" "${TOP}/crates/${crate}"; then
	echo "$crate: ...:"
    else
	echo "$crate: CHANGED"
    fi
done
