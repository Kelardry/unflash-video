#!/usr/bin/env bash
# Test streams for the built-in VP9 decoder: small libvpx encodes covering
# the tools the decoder supports, each with ffmpeg's per-frame MD5 of the
# decoded pictures (8-bit clips as yuv420p, deeper ones as yuv420p10le /
# yuv420p12le, every frame at its own size) as the oracle. Needs ffmpeg
# with libvpx-vp9; the resize clip also a C compiler and libvpx's headers
# (libvpx-dev), for resize_enc.c.
set -euo pipefail
cd "$(dirname "$0")"
oracle() {
  # oracle FILE PIX_FMT
  ffmpeg -v error -i "$1" -autoscale 0 -pix_fmt "$2" -f framemd5 - | grep -v '^#' > "${1%.*}.framemd5"
  printf '%-18s %7d bytes %4d frames\n' "$1" "$(stat -c %s "$1")" "$(wc -l < "${1%.*}.framemd5")"
}
gen() {
  # gen NAME "INPUT FILTER" WxH FRAMES PIX_FMT ENCODER OPTIONS...
  local name=$1 src=$2 size=$3 n=$4 fmt=$5; shift 5
  ffmpeg -y -v error -f lavfi -i "$src" -frames:v "$n" -s "$size" -pix_fmt "$fmt" -c:v libvpx-vp9 "$@" "$name.ivf"
  oracle "$name.ivf" "$fmt"
}
T2="testsrc2=size=176x144:rate=30"
MB="mandelbrot=size=176x144:rate=30"
# hidden alt-ref frames in superframes, compound prediction, switchable filters
gen altref       "$T2" 176x144 40 yuv420p -b:v 300k -auto-alt-ref 1 -lag-in-frames 16 -g 60
gen altref_mb    "$MB" 176x144 30 yuv420p -b:v 400k -auto-alt-ref 1 -lag-in-frames 25 -arnr-maxframes 7
# the real-time encoder's choices (no alt-ref, fast modes)
gen realtime     "$MB" 176x144 20 yuv420p -b:v 200k -deadline realtime -cpu-used 8
# profile 2: 10 and 12 bits
gen p2_10bit     "$T2" 176x144 20 yuv420p10le -profile:v 2 -b:v 300k -auto-alt-ref 1 -lag-in-frames 10
gen p2_12bit     "$MB" 96x64   15 yuv420p12le -profile:v 2 -b:v 300k
# frames decodable without earlier probabilities; no backward adaptation
gen errres       "$T2" 176x144 20 yuv420p -b:v 200k -error-resilient 1
gen parallel     "$T2" 176x144 20 yuv420p -b:v 200k -frame-parallel 1
# 2x4 tiles (a tile column is at least 256 samples wide)
gen tiles        "$T2" 512x96   8 yuv420p -b:v 400k -tile-columns 1 -tile-rows 2 -row-mt 1
# lossless: the Walsh-Hadamard transform
gen lossless     "$T2" 64x48   10 yuv420p -lossless 1
gen lossless_10  "$T2" 64x48    8 yuv420p10le -profile:v 2 -lossless 1
# segmentation: variance, complexity and cyclic-refresh adaptive quantisation
gen aq_variance  "$MB" 176x144 20 yuv420p -b:v 300k -aq-mode 1
gen aq_cyclic    "$MB" 176x144 20 yuv420p -b:v 300k -aq-mode 3
# loop filter sharpness; very low and very high quantisers
gen sharpness    "$T2" 176x144 15 yuv420p -b:v 150k -sharpness 7
gen q_fine       "$MB" 96x64   10 yuv420p -crf 4 -b:v 0
gen q_coarse     "$MB" 176x144 20 yuv420p -crf 63 -b:v 0
# odd sizes: blocks and chroma past the picture edges
gen odd          "$T2" 99x61   20 yuv420p -b:v 150k -auto-alt-ref 1 -lag-in-frames 8
gen odd_small    "$MB" 33x17   15 yuv420p -b:v 100k
# the same stream in WebM, read through the Matroska demuxer
ffmpeg -y -v error -i altref.ivf -c copy altref.webm
oracle altref.webm yuv420p
# frame size changes predicted from scaled references (down and up), built
# with libvpx's encoder API
cc -O2 -o resize_enc resize_enc.c -lvpx
src() { ffmpeg -v error -f lavfi -i "mandelbrot=size=$1:rate=30" -frames:v "$2" -f rawvideo -pix_fmt yuv420p -; }
{ src 208x160 6; src 128x96 6; src 176x144 6; src 104x80 6; } | ./resize_enc resize.ivf 300 208 160 6 128 96 6 176 144 6 104 80 6
oracle resize.ivf yuv420p
{ src 66x50 5; src 40x30 5; src 65x49 5; } | ./resize_enc resize_odd.ivf 150 66 50 5 40 30 5 65 49 5
oracle resize_odd.ivf yuv420p
rm -f resize_enc
du -ch ./*.ivf ./*.webm | tail -1
