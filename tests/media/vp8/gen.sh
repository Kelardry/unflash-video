#!/usr/bin/env bash
# Test streams for the built-in VP8 decoder: small libvpx encodes covering
# the coding tools, each with the per-frame MD5 of the pictures ffmpeg's own
# VP8 decoder (not libvpx) makes of it as the oracle. Needs ffmpeg with
# libvpx. The inspect example lists the tools a stream uses:
#   cargo run --release -p unflash-vp8 --example inspect -- smooth.ivf
set -euo pipefail
cd "$(dirname "$0")"
oracle() {
  # every decoded picture as it is: no frame rate conversion, and no scaling
  # of the pictures after a size change to the first size
  ffmpeg -v error -c:v vp8 -i "$1" -fps_mode passthrough -autoscale 0 -f framemd5 - | grep -v '^#' > "${1%.*}.framemd5"
  printf '%-18s %7d bytes  %3d pictures\n' "$1" "$(stat -c %s "$1")" "$(wc -l < "${1%.*}.framemd5")"
}
# src NAME WxH FRAMES: a lavfi source (trimmed by a filter, which two-pass
# encoding needs: with -frames:v the first pass leaves its statistics
# unfinished)
src() { echo "$1=size=$2:rate=30,trim=end_frame=$3"; }
gen() {
  # gen FILE "LAVFI GRAPH" ENCODER OPTIONS...
  local out=$1 graph=$2; shift 2
  ffmpeg -y -v error -f lavfi -i "$graph" -pix_fmt yuv420p -c:v libvpx "$@" "$out"
  oracle "$out"
}
gen2() {
  # two-pass encoding, which libvpx needs for alt-ref frames
  local out=$1 graph=$2; shift 2
  ffmpeg -y -v error -f lavfi -i "$graph" -pix_fmt yuv420p -c:v libvpx "$@" -pass 1 -passlogfile "$out" -f "${out##*.}" /dev/null
  ffmpeg -y -v error -f lavfi -i "$graph" -pix_fmt yuv420p -c:v libvpx "$@" -pass 2 -passlogfile "$out" "$out"
  rm -f "$out"-*.log
  oracle "$out"
}
# smooth moving content: six-tap prediction, split vectors, golden and alt-ref
# references, the normal loop filter with mode and reference deltas
gen smooth.ivf "$(src testsrc2 176x144 30)" -b:v 300k
# detail with a key frame every 5 frames: subblock intra modes and their
# key-frame contexts
gen intra.ivf "$(src mandelbrot 128x96 20)" -b:v 400k -g 5
# alt-ref frames: invisible frames (in WebM, blocks of their own), sign bias
gen2 altref.webm "$(src testsrc2 128x96 40)" -b:v 250k -auto-alt-ref 1 -lag-in-frames 16 -arnr-maxframes 5 -arnr-strength 3
# real time with error resilience: cyclic-refresh segmentation (quantisers,
# a map every frame) and probabilities that last one frame
gen resilient.webm "$(src testsrc2 176x144 30)" -b:v 200k -deadline realtime -cpu-used 8 -error-resilient default
# a region of interest on the first frames only: segment quantisers, the
# map updated and then kept
gen roi.ivf "testsrc2=size=176x144:rate=30,trim=end_frame=24,split[a][b];[a]trim=end_frame=4,addroi=x=0:y=0:w=iw/2:h=ih/2:qoffset=-0.6[a1];[b]trim=start_frame=4,setpts=PTS-STARTPTS[b1];[a1][b1]concat=n=2:v=1[out0]" -b:v 200k
# eight token partitions
gen partitions.ivf "$(src testsrc2 176x144 20)" -b:v 400k -slices 8
# loop filter sharpness on detail
gen sharpness.ivf "$(src mandelbrot 128x96 20)" -b:v 300k -sharpness 6
# a very low quantiser: large coefficients (the longest token categories)
gen lowq.ivf "$(src mandelbrot 64x48 8)" -qmin 0 -qmax 2 -b:v 5M
# a very high quantiser: strong loop filtering
gen highq.ivf "$(src testsrc2 176x144 20)" -qmin 56 -qmax 63 -b:v 20k
# sizes that are not whole macroblocks, odd chroma, a picture smaller than one
# macroblock
gen odd.ivf "$(src testsrc2 97x61 20)" -b:v 200k
gen tiny.ivf "$(src testsrc2 6x4 10)" -b:v 50k
# versions 1 to 3: bilinear prediction, the simple loop filter, whole-sample
# chroma vectors
gen version1.ivf "$(src testsrc2 80x48 15)" -b:v 150k -profile:v 1
gen version2.ivf "$(src testsrc2 80x48 15)" -b:v 150k -profile:v 2
gen version3.ivf "$(src testsrc2 80x48 15)" -b:v 150k -profile:v 3
# temporal layers: frames that refresh the golden or alt-ref frame and not the
# last one
gen layers.ivf "$(src testsrc2 128x96 30)" -b:v 300k -ts-parameters ts_number_layers=3:ts_target_bitrate=100,200,300:ts_rate_decimator=4,2,1:ts_periodicity=4:ts_layer_id=0,2,1,2:ts_layering_mode=3
# a fast pan: long vectors reaching beyond the picture
gen pan.ivf "testsrc2=size=352x288:rate=30,trim=end_frame=20,crop=96:64:x='mod(n*13,256)':y='mod(n*9,224)'" -b:v 300k
# a key frame changing the picture size part way (two encodes joined)
ffmpeg -y -v error -f lavfi -i "$(src testsrc2 96x64 10)" -pix_fmt yuv420p -c:v libvpx -b:v 200k resize-a.ivf
ffmpeg -y -v error -f lavfi -i "$(src mandelbrot 72x40 10)" -pix_fmt yuv420p -c:v libvpx -b:v 200k resize-b.ivf
printf "file '%s'\n" resize-a.ivf resize-b.ivf > resize.txt
ffmpeg -y -v error -f concat -safe 0 -i resize.txt -c copy resize.ivf
rm resize-a.ivf resize-b.ivf resize.txt
oracle resize.ivf
du -ch ./*.ivf ./*.webm ./*.framemd5 | tail -1
