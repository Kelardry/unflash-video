#!/usr/bin/env bash
# Test streams for the built-in AV1 decoder (crates/unflash-av1): small
# encodes from the three encoders covering what the decoder has to get right,
# each with ffmpeg's per-frame MD5 of its libdav1d decoding as the oracle
# (output order, film grain applied, every picture at its own size). Needs
# ffmpeg with libaom, libsvtav1, librav1e and libdav1d.
set -euo pipefail
cd "$(dirname "$0")"
T2="testsrc2=size=176x144:rate=30"
NOISE="mandelbrot=size=176x144:rate=30"
enc() {
  # enc NAME.EXT "INPUT FILTER" WxH FRAMES PIX_FMT ENCODER OPTIONS...
  local file=$1 src=$2 size=$3 n=$4 pix=$5; shift 5
  # (bitexact: no random Matroska segment UID, so the files come out the
  # same every time; SVT-AV1 talks on stderr whatever the log level)
  ffmpeg -y -v error -f lavfi -i "$src" -frames:v "$n" -s "$size" -pix_fmt "$pix" -an "$@" -fflags +bitexact "$file" \
    2> >(grep -v -e '^Svt\[' -e 'scaling denominators' -e 'retains a significant amount' >&2)
}
oracle() {
  # oracle NAME.EXT PIX_FMT: ffmpeg's libdav1d decode, frame by frame
  local file=$1 pix=$2
  ffmpeg -v error -c:v libdav1d -i "$file" -autoscale 0 -fps_mode passthrough -f framemd5 -pix_fmt "$pix" - | grep -v '^#' > "${file%.*}.framemd5"
  printf '%-22s %6d bytes  %3d pictures  %s\n' "$file" "$(stat -c %s "$file")" "$(wc -l < "${file%.*}.framemd5")" \
    "$(ffprobe -v error -select_streams v:0 -show_entries stream=profile,width,height,pix_fmt -of csv=p=0 "$file")"
}
# libaom, 8-bit, odd size (101x75: chroma 51x38), a key frame every 10
# frames, hidden alt-ref frames shown later with show_existing_frame;
# tagged BT.709 full range
enc aom_odd.mp4 "$T2" 101x75 20 yuv420p -c:v libaom-av1 -cpu-used 8 -b:v 100k -g 10 -keyint_min 10 \
  -colorspace bt709 -color_primaries bt709 -color_trc bt709 -color_range pc
oracle aom_odd.mp4 yuv420p
# libaom on detail: alt-refs and show_existing_frame, 2x2 tiles, a key frame
# every 15 frames (for decoding from a key frame in the middle)
enc aom_altref_tiles.mp4 "$NOISE" 128x96 30 yuv420p -c:v libaom-av1 -cpu-used 6 -b:v 150k -g 15 -keyint_min 15 -tile-columns 1 -tile-rows 1
oracle aom_altref_tiles.mp4 yuv420p
# SVT-AV1 with film grain synthesis (applied by the decoder)
enc svt_grain.mp4 "$T2" 128x96 24 yuv420p -c:v libsvtav1 -preset 8 -crf 40 -svtav1-params film-grain=8
oracle svt_grain.mp4 yuv420p
# SVT-AV1, 10-bit with film grain, in Matroska (the av1C is CodecPrivate);
# tagged BT.2020
enc svt_10bit.mkv "$NOISE" 128x96 24 yuv420p10le -c:v libsvtav1 -preset 8 -crf 40 -svtav1-params film-grain=6 \
  -colorspace bt2020nc -color_primaries bt2020 -color_trc smpte2084
oracle svt_10bit.mkv yuv420p10le
# SVT-AV1's random resize: the frame size changes from frame to frame
# (64x48 to 128x96, odd sizes among them) with scaled references, no key frames
enc svt_resize.mp4 "$T2" 128x96 24 yuv420p -c:v libsvtav1 -preset 8 -crf 40 -svtav1-params resize-mode=2
oracle svt_resize.mp4 yuv420p
# rav1e
enc rav1e.mp4 "$NOISE" 96x64 20 yuv420p -c:v librav1e -speed 10 -qp 100
oracle rav1e.mp4 yuv420p
# monochrome (grey chroma in the decoder's pictures)
enc aom_gray.mp4 "$T2" 64x48 8 gray -c:v libaom-av1 -cpu-used 8 -b:v 50k
oracle aom_gray.mp4 gray
# formats the decoder turns down: 4:4:4, 4:2:2, 12-bit
enc aom_444.mp4 "$T2" 64x48 2 yuv444p -c:v libaom-av1 -cpu-used 8 -b:v 50k
enc aom_422.mp4 "$T2" 64x48 2 yuv422p -c:v libaom-av1 -cpu-used 8 -b:v 50k
enc aom_12bit.mp4 "$T2" 64x48 2 yuv420p12le -c:v libaom-av1 -cpu-used 8 -b:v 50k
du -cb ./*.mp4 ./*.mkv ./*.framemd5 | tail -1
