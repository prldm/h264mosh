#!/usr/bin/env bash
# Encode a short clip with many encoder feature combinations and check that
# `h264mosh roundtrip --mb` reproduces every stream bit for bit.
#
# usage: scripts/roundtrip_matrix.sh [source video] [work dir]
set -u
SRC=${1:-vidi2.mp4}
WORK=${2:-target/matrix}
FFMPEG=${H264MOSH_FFMPEG:-ffmpeg}
BIN=target/release/h264mosh
mkdir -p "$WORK"

Y4M="$WORK/src.y4m"
if [ ! -f "$Y4M" ]; then
    "$FFMPEG" -v error -y -i "$SRC" -t 2 -vf "scale=640:360,format=yuv420p" -f yuv4mpegpipe "$Y4M" || exit 1
fi

pass=0
fail=0
check() { # name file
    if out=$("$BIN" roundtrip --mb "$2" 2>&1); then
        pass=$((pass + 1))
        printf '  ok    %s\n' "$1"
    else
        fail=$((fail + 1))
        printf '  FAIL  %s\n' "$1"
        printf '%s\n' "$out" | sed 's/^/        /'
    fi
}

enc() { # name, ffmpeg output options...
    local name=$1
    shift
    local f="$WORK/$name.264"
    if "$FFMPEG" -v error -y -i "$Y4M" "$@" -f h264 "$f" 2>"$WORK/$name.log"; then
        check "$name" "$f"
    else
        rm -f "$f"
        printf '  skip  %s (%s)\n' "$name" "$(head -1 "$WORK/$name.log")"
    fi
}

x264() { # name, x264-params, extra ffmpeg options...
    local name=$1 params=$2
    shift 2
    enc "x264_$name" -c:v libx264 -x264-params "${params:-log=-1}" "$@"
}

echo "libx264 feature matrix:"
x264 default ""
x264 cavlc cabac=0
x264 baseline "" -profile:v baseline
x264 slices4 slices=4
x264 slicemax slice-max-size=1500
x264 sliced_threads sliced-threads=1
x264 interlaced_tff tff=1
x264 interlaced_bff_cavlc bff=1:cabac=0
x264 fake_interlaced fake-interlaced=1:pic-struct=1
x264 cqm_jvt cqm=jvt
x264 cqm_flat_8x8off cqm=flat:8x8dct=0
x264 i444 "" -pix_fmt yuv444p
x264 i422 "" -pix_fmt yuv422p
x264 i400 "" -pix_fmt gray
x264 bit10 "" -pix_fmt yuv420p10le
x264 lossless qp=0
x264 hrd nal-hrd=vbr:vbv-maxrate=2000:vbv-bufsize=2000:bitrate=1500
x264 hrd_cbr nal-hrd=cbr:vbv-maxrate=1000:vbv-bufsize=1000:bitrate=1000
x264 open_gop open-gop=1:keyint=20
x264 intra_refresh intra-refresh=1:keyint=30
x264 all_intra keyint=1
x264 refs16 ref=16:bframes=8:b-pyramid=strict
x264 pyramid_normal bframes=5:b-pyramid=normal:weightb=1
x264 direct_temporal direct=temporal:bframes=3
x264 weightp2 weightp=2:ref=4
x264 aud aud=1:keyint=15
x264 stitchable stitchable=1
x264 no_deblock deblock=0
x264 deblock_offsets deblock=-3,2
x264 crop "" -vf crop=624:344:8:8
x264 colour colorprim=bt709:transfer=bt709:colormatrix=bt709:range=pc:chromaloc=2:overscan=show:videoformat=pal -vf setsar=4/3
x264 mosh_prep bframes=0:ref=1:weightp=0
x264 mosh_prep_cavlc bframes=0:ref=1:weightp=0:cabac=0

echo "libx264 CAVLC (macroblock layer):"
x264 cavlc_i422 cabac=0 -pix_fmt yuv422p
x264 cavlc_i444 cabac=0 -pix_fmt yuv444p
x264 cavlc_i400 cabac=0 -pix_fmt gray
x264 cavlc_bit10 cabac=0 -pix_fmt yuv420p10le
x264 cavlc_bit10_i422 cabac=0 -pix_fmt yuv422p10le
x264 cavlc_lossless cabac=0:qp=0
x264 cavlc_lossless_i444 cabac=0:qp=0 -pix_fmt yuv444p
x264 cavlc_qp1 cabac=0:qp=1
x264 cavlc_qp51 cabac=0:qp=51
x264 cavlc_no8x8 cabac=0:8x8dct=0
x264 cavlc_cqm cabac=0:cqm=jvt
x264 cavlc_ref2 cabac=0:ref=2:bframes=0
x264 cavlc_ref16 cabac=0:ref=16:bframes=3:b-pyramid=normal
x264 cavlc_slices cabac=0:slices=7
x264 cavlc_weightp cabac=0:weightp=2:ref=3:bframes=0
x264 cavlc_direct_temporal cabac=0:direct=temporal:bframes=3:weightb=1
x264 cavlc_direct_spatial_i444 cabac=0:bframes=3 -pix_fmt yuv444p
x264 cavlc_all_intra cabac=0:keyint=1
x264 cavlc_intra_refresh cabac=0:intra-refresh=1:bframes=0
x264 cavlc_psy_off cabac=0:subme=9:partitions=all -tune psnr

echo "other encoders (skipped when not available on this machine):"
enc ff_mediafoundation -c:v h264_mf
enc ff_mediafoundation_hq -c:v h264_mf -b:v 20M
enc ff_nvenc -c:v h264_nvenc
enc ff_nvenc_bframes -c:v h264_nvenc -bf 3 -b_ref_mode middle
enc ff_amf -c:v h264_amf
enc ff_qsv -c:v h264_qsv
enc ff_videotoolbox -c:v h264_videotoolbox
enc ff_openh264 -c:v libopenh264

echo "passed: $pass, failed: $fail"
[ "$fail" -eq 0 ]
