#!/usr/bin/env bash
# Time the CPU-heavy commands (best of 3) and keep their outputs for comparison.
#
# usage: scripts/bench.sh <720p cavlc .264> <1080p cavlc .264> <output dir> [threads]
# threads sets RAYON_NUM_THREADS (ignored by builds without rayon).
set -eu
S720=$1
S1080=$2
OUT=$3
BIN=${BIN:-target/release/h264mosh}
mkdir -p "$OUT"
[ $# -ge 4 ] && export RAYON_NUM_THREADS=$4

best() { # name, command...
    local name=$1 t best=999999
    shift
    for _ in 1 2 3; do
        local s e
        s=$(date +%s%N)
        "$@" >"$OUT/$name.txt" 2>&1
        e=$(date +%s%N)
        t=$(((e - s) / 1000000))
        [ $t -lt $best ] && best=$t
    done
    printf '  %-34s %6d ms\n' "$name" "$best"
}

FX="--mv-scale 1.5 --kill-intra --mv-add 1:-0.5"
best "mosh frame ops 720p" "$BIN" mosh "$S720" -o "$OUT/frames720.264" --drop-keyframes
best "mosh effects 720p (1208 fr)" "$BIN" mosh "$S720" -o "$OUT/fx720.264" --drop-keyframes 696 $FX
best "mosh effects 1080p (512 fr)" "$BIN" mosh "$S1080" -o "$OUT/fx1080.264" --drop-keyframes $FX
best "info --mbs 1080p" "$BIN" info "$S1080" --mbs
best "roundtrip --mb 720p" "$BIN" roundtrip "$S720" --mb
