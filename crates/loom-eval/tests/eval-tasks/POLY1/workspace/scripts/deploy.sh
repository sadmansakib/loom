#!/usr/bin/env bash
set -euo pipefail

SRC=dist
DEST=/srv/app

release="$1"

rm -rf $DEST/old
mkdir -p $DEST
cp -r $SRC $DEST/new-$release
echo "deployed $release to $DEST/new-$release"
