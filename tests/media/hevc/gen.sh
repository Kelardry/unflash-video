#!/usr/bin/env bash
# Test streams for the built-in HEVC decoder: small x265 encodes covering
# the tools the decoder supports, each with ffmpeg's per-frame MD5 of the
# decoded pictures (presentation order, cropped) as the oracle: yuv420p for
# 8-bit clips, the 16-bit planes (yuv420p10le) for 10-bit ones and the luma
# plane (gray) for the 4:0:0 one. Needs ffmpeg with libx265.
set -euo pipefail
cd "$(dirname "$0")"
gen() {
  # gen NAME "INPUT FILTER" WxH FRAMES PIX_FMT "X265 PARAMS"
  local name=$1 src=$2 size=$3 n=$4 fmt=$5 params=$6
  ffmpeg -y -v error -f lavfi -i "$src" -frames:v "$n" -s "$size" -pix_fmt "$fmt" -c:v libx265 -tag:v hvc1 \
    -x265-params "log-level=error:$params" -movflags +faststart "$name.mp4"
  ffmpeg -v error -i "$name.mp4" -pix_fmt "$fmt" -f framemd5 - | grep -v '^#' > "$name.framemd5"
  printf '%-22s %7d bytes  %s\n' "$name" "$(stat -c %s "$name.mp4")" "$(ffprobe -v error -select_streams v:0 -show_entries stream=profile,pix_fmt -of csv=p=0 "$name.mp4")"
}
T2="testsrc2=size=176x144:rate=30"
NOISE="mandelbrot=size=176x144:rate=30"
FADE="testsrc2=size=176x144:rate=30,fade=in:0:15"
# intra only: all the intra modes, the 4x4 DST, transforms up to 32x32, no loop filters
gen intra            "$T2"    64x48   3 yuv420p "keyint=1:no-sao=1:no-deblock=1"
# intra with deblocking and SAO, strong intra smoothing, 16x16 coding tree blocks
gen intra_filters    "$NOISE" 96x64   3 yuv420p "keyint=1:ctu=16:strong-intra-smoothing=1"
# P pictures only, one reference, 32x32 coding tree blocks, deeper transform trees
gen p_frames         "$T2"    96x64  20 yuv420p "bframes=0:ref=1:ctu=32:tu-intra-depth=3:tu-inter-depth=3"
# B pictures in a pyramid, several references, all merge candidates
gen b_pyramid        "$T2"    128x96 30 yuv420p "bframes=4:b-pyramid=1:ref=4:max-merge=5:keyint=24"
# explicit weighted prediction on a fade (P and B)
gen weighted         "$FADE"  96x64  30 yuv420p "bframes=2:weightp=1:weightb=1"
# asymmetric and rectangular partitions, small coding units
gen amp_rect         "$NOISE" 128x96 16 yuv420p "bframes=2:amp=1:rect=1:ctu=32:min-cu-size=8:limit-modes=0"
# transform skip, the default scaling lists, sign hiding off
gen tskip_scaling    "$NOISE" 96x64  12 yuv420p "tskip=1:scaling-list=default:no-signhide=1"
# several slices per picture, deblocking offsets
gen slices           "$T2"    160x96 12 yuv420p "slices=3:deblock=-2,2:bframes=1"
# wavefronts (entropy coding sync) with 16x16 coding tree blocks
gen wpp              "$T2"    176x144 8 yuv420p "wpp=1:ctu=16:bframes=1"
# lossless: every coding unit bypasses transform and quantisation
gen lossless         "$NOISE" 64x48   5 yuv420p "lossless=1"
# lossless coding units chosen per unit, adaptive quantisation (cu_qp_delta)
gen cu_lossless      "$NOISE" 96x64   8 yuv420p "cu-lossless=1:aq-mode=2:qg-size=16"
# constrained intra prediction with intra blocks in inter pictures
gen cip              "$NOISE" 96x64  12 yuv420p "constrained-intra=1:bframes=1"
# open GOPs: CRA pictures with leading pictures
gen open_gop         "$T2"    96x64  40 yuv420p "open-gop=1:keyint=12:min-keyint=12:bframes=3:scenecut=0"
# an odd size that needs a conformance window, chroma QP offsets
gen odd_size         "$T2"    100x60 12 yuv420p "cbqpoffs=3:crqpoffs=-2:bframes=2"
# 10-bit: B pictures, SAO, weighted prediction
gen main10           "$T2"    96x64  20 yuv420p10le "bframes=3:ref=3"
gen main10_wp_lossless "$FADE" 96x64 12 yuv420p10le "weightp=1:weightb=1:bframes=2:cu-lossless=1"
# 4:0:0: a range extensions profile, but none of its coding tools
gen mono             "$T2"    96x64  10 gray "bframes=2"
