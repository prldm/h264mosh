#!/usr/bin/env bash
# Produce a fixed set of outputs with a given binary, for bit-for-bit comparison
# between two builds:
#   scripts/ref_outputs.sh old.exe out/a && scripts/ref_outputs.sh target/release/h264mosh out/b
#   diff -r out/a out/b
# Needs the streams written by `prep` and scripts/roundtrip_matrix.sh.
set -eu
BIN=$1
OUT=$2
S720=${S720:-out/clips_ff.264}
S1080=${S1080:-out/v2_1080_cavlc.264}
M=target/matrix
mkdir -p "$OUT"
run() { # name, args...
    local name=$1
    shift
    "$BIN" "$@" >/dev/null
}
run frames720 mosh "$S720" -o "$OUT/frames720.264" --drop-keyframes
run fx720 mosh "$S720" -o "$OUT/fx720.264" --drop-keyframes 696 --mv-scale 1.5 --kill-intra --mv-add 1:-0.5
run fx1080 mosh "$S1080" -o "$OUT/fx1080.264" --drop-keyframes --mv-scale 1.5 --kill-intra --mv-add 1:-0.5
run zero mosh "$S720" -o "$OUT/zero.264" --drop-keyframes 696 --zero-residual --mvd-scale 2 --fx-frames 696-900
run slices mosh "$M/x264_cavlc_slices.264" -o "$OUT/slices.264" --mv-scale 2 --kill-intra
run ref2 mosh "$M/x264_cavlc_ref2.264" -o "$OUT/ref2.264" --mv-add 3:1 --explicit-skips
run i444 mosh "$M/x264_cavlc_i444.264" -o "$OUT/i444.264" --mv-scale 0.5 --zero-residual
run i422 mosh "$M/x264_cavlc_i422.264" -o "$OUT/i422.264" --split-8x8 --kill-intra
run bit10 mosh "$M/x264_cavlc_bit10.264" -o "$OUT/bit10.264" --mv-scale -1 --zero-residual
run coeffs mosh "$S1080" -o "$OUT/coeffs.264" --coeff-scale 0:0.25
run coeffs_cabac mosh "$M/x264_default.264" -o "$OUT/coeffs_cabac.264" --coeff-scale 0:0.25 --qp-add 6
"$BIN" info "$S1080" --mbs >"$OUT/info_mbs.txt"
"$BIN" roundtrip "$S720" --mb >"$OUT/roundtrip.txt"
