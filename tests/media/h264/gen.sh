#!/usr/bin/env bash
# Test streams for the built-in H.264 decoder: small x264 encodes covering
# the profiles and tools the decoder supports, each with ffmpeg's per-frame
# MD5 of the decoded yuv420p pictures (presentation order, cropped) as the
# oracle. Needs ffmpeg with libx264.
set -euo pipefail
cd "$(dirname "$0")"
gen() {
  # gen NAME "INPUT FILTER" WxH FRAMES "ENCODER OPTIONS"
  local name=$1 src=$2 size=$3 n=$4; shift 4
  ffmpeg -y -v error -f lavfi -i "$src" -frames:v "$n" -s "$size" -pix_fmt yuv420p -c:v libx264 "$@" -movflags +faststart "$name.mp4"
  ffmpeg -v error -i "$name.mp4" -f framemd5 - | grep -v '^#' > "$name.framemd5"
  printf '%-22s %7d bytes  %s\n' "$name" "$(stat -c %s "$name.mp4")" "$(ffprobe -v error -select_streams v:0 -show_entries stream=codec_tag_string,profile,level -of csv=p=0 "$name.mp4")"
}
T2="testsrc2=size=176x144:rate=30"
NOISE="mandelbrot=size=176x144:rate=30"
# constrained baseline: CAVLC, P only, one reference (avc1.42C01F-style files)
gen cb_cavlc          "$T2" 64x48   30 -profile:v baseline -level 3.1 -refs 1 -g 12
# baseline with cropping (100x60 -> 112x64 coded), several references, two slices
gen cb_crop_slices    "$T2" 100x60  30 -profile:v baseline -refs 3 -x264-params "slices=2:partitions=all"
# main: CABAC, B-frames, pyramid, temporal direct
gen main_cabac_b      "$T2" 96x64   40 -profile:v main -bf 3 -refs 3 -x264-params "b-pyramid=normal:direct=temporal:partitions=all:subme=7"
# main: CAVLC with B-frames and weighted prediction on a fade
gen main_cavlc_wp     "$T2,fade=in:0:20" 96x64 40 -profile:v main -bf 2 -x264-params "cabac=0:weightp=2:weightb=1:b-adapt=0:partitions=all"
# high: 8x8 transform, spatial direct, qp changes per macroblock, deblock offsets, four slices
gen high_8x8_aq       "$NOISE" 112x80 40 -profile:v high -bf 3 -refs 4 -x264-params "8x8dct=1:direct=spatial:aq-mode=1:aq-strength=1.5:deblock=2,-1:slices=4:partitions=all:subme=7:b-pyramid=normal"
# high: custom scaling matrices, temporal direct, explicit weighted P/B
gen high_cqm_wp       "$T2,fade=in:0:15" 96x64 40 -profile:v high -bf 2 -refs 2 -x264-params "8x8dct=1:cqm=jvt:direct=temporal:weightp=2:weightb=1:partitions=all"
# high: constrained intra prediction, no deblocking, intra refresh (I MBs in P pictures)
gen high_cip_nodeblock "$T2" 96x64 30 -profile:v high -x264-params "8x8dct=1:constrained-intra=1:no-deblock=1:intra-refresh=1:keyint=15"
# high: short GOPs with open GOP (non-IDR I pictures), many references, 4x4 sub-partitions
gen high_opengop_refs "$NOISE" 96x64 40 -profile:v high -bf 2 -refs 8 -x264-params "8x8dct=1:open-gop=1:keyint=10:min-keyint=5:partitions=all:p4x4=1:subme=9:me=umh"
# high: the odd-size crop with CABAC and B-frames
gen high_crop_cabac   "$T2" 100x60 30 -profile:v high -bf 2 -x264-params "8x8dct=1:partitions=all"
# very low QP on noise: x264 codes macroblocks as I_PCM (CABAC and CAVLC)
gen pcm_cabac         "$NOISE" 96x64 12 -profile:v high -x264-params "qp=1"
gen pcm_cavlc         "$NOISE" 96x64 12 -profile:v high -x264-params "qp=1:cabac=0"
# interlaced (x264 codes MBAFF frames): decoded like the rest
gen high_interlaced   "$T2" 96x64 20 -profile:v high -x264-params "interlaced=1"
# the same pictures three ways with an IDR every 10 frames, for splicing GOPs of
# one into another (the smart-cut export): High CABAC with B-frames, Main CAVLC
# without, Main CABAC with B-frames
gen splice_a          "$T2" 96x64 40 -profile:v high -bf 2 -x264-params "keyint=10:min-keyint=10:scenecut=0:8x8dct=1:partitions=all"
gen splice_b          "$T2" 96x64 40 -profile:v main -bf 0 -x264-params "keyint=10:min-keyint=10:scenecut=0:cabac=0"
gen splice_c          "$T2" 96x64 40 -profile:v main -bf 2 -x264-params "keyint=10:min-keyint=10:scenecut=0:b-pyramid=none"
ls -la *.mp4 *.framemd5 | awk '{print $5, $9}'
