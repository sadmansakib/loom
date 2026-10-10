#!/usr/bin/env bash
set -eu

SRC=dist
DEST=/srv/app

mkdir -p "$DEST"
rsync -a "$SRC/" "$DEST/"
echo "deployed to $DEST"
