#!/usr/bin/env bash
# Side-by-side (stacked) crop of golden vs candidate: sbs.sh <golden-dir> <candidate-dir> <shot> <w>x<h>+<x>+<y> <out.png>
set -euo pipefail
nix shell nixpkgs#imagemagick -c magick \( "$1/$3.png" -crop "$4" +repage \) \( "$2/$3.png" -crop "$4" +repage \) -background red -splice 0x4 -append "$5"
