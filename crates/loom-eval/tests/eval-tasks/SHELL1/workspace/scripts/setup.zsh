#!/usr/bin/env zsh
set -e

for f in *.tar.gz; do
  print "unpacking $f"
  tar -xzf "$f"
done
