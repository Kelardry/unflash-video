#!/usr/bin/env bash
# Test streams for the DTS core decoder (crates/unflash-sound): short
# ffmpeg encodes (its dca encoder) of synthetic signals in every channel
# layout the encoder takes (mono, stereo, 2/2, 3/2 with and without LFE),
# at 22.05, 32, 44.1 and 48 kHz and several bit rates, with and without
# ADPCM. Each channel has its own content (a tone at its own frequency, a
# sweep, noise bursts out of silence at its own times), so that a channel
# mix-up shows. The tests compare the decoder with ffmpeg's decode at
# test time; nothing else is stored.
set -euo pipefail
cd "$(dirname "$0")"

# channel i's signal: tone, sweep (200 Hz to 18 kHz, half the time),
# 30 ms noise bursts every 250 ms, a low noise floor; silent at first
chan() {
  local i=$1 f=$2 p=$3
  echo "gt(t\,0.06)*(0.25*sin(2*PI*$f*t)+0.2*sin(2*PI*(200*t+8900*t*t))*lt(mod(t+$p\,1)\,0.5)+0.5*(2*random($i)-1)*between(mod(t+$p\,0.25)\,0.2\,0.23)+0.02*(2*random($((i+5)))-1))"
}
lfe() {
  echo "0.3*sin(2*PI*45*t)+0.2*sin(2*PI*80*t)*lt(mod(t\,0.5)\,0.25)"
}

gen() {
  # gen NAME LAYOUT CHANNELS(tone frequencies, or "lfe") RATE BITRATE SECONDS [encoder options...]
  local name=$1 layout=$2 chans=$3 rate=$4 bitrate=$5 secs=$6
  shift 6
  local exprs="" i=0
  for c in $chans; do
    if [ "$c" = lfe ]; then e=$(lfe); else e=$(chan $i $c 0.$((i * 13 % 10))); fi
    exprs="${exprs:+$exprs|}$e"
    i=$((i + 1))
  done
  ffmpeg -y -v error -f lavfi -i "aevalsrc=exprs=$exprs:s=$rate:d=$secs:c=$layout" -c:a dca -strict -2 -b:a "$bitrate" "$@" "$name.dts"
  printf '%-30s %7d bytes  %s\n' "$name.dts" "$(stat -c %s "$name.dts")" "$(ffprobe -v error -show_entries stream=sample_rate,channel_layout,bit_rate -of csv=p=0 "$name.dts")"
}

gen dts_mono_48k_320k        mono         "440"                       48000 320k  0.5
gen dts_stereo_44k_384k      stereo       "440 660"                   44100 384k  0.5
gen dts_stereo_48k_1411k     stereo       "440 660"                   48000 1411.2k 0.25
gen dts_stereo_32k_448k      stereo       "330 550"                   32000 448k  0.5
gen dts_mono_22k_320k        mono         "330"                       22050 320k  0.5
gen dts_quad_48k_640k        "quad(side)" "440 660 880 1100"          48000 640k  0.4
gen dts_5.0_44k_768k         "5.0(side)"  "440 660 550 880 1100"      44100 768k  0.4
gen dts_5.1_48k_768k         "5.1(side)"  "440 660 550 lfe 880 1100"  48000 768k  0.5
gen dts_5.1_44k_1536k        "5.1(side)"  "440 660 550 lfe 880 1100"  44100 1536k 0.25
# ADPCM (the encoder's option)
gen dts_stereo_48k_adpcm     stereo       "440 660"                   48000 448k  0.5 -dca_adpcm 1
gen dts_5.1_48k_adpcm        "5.1(side)"  "440 660 550 lfe 880 1100"  48000 768k  0.4 -dca_adpcm 1
ffmpeg -y -v error -f lavfi -i "anullsrc=r=48000:cl=stereo:d=0.25" -c:a dca -strict -2 -b:a 384k dts_silence_48k.dts
printf '%-30s %7d bytes\n' dts_silence_48k.dts "$(stat -c %s dts_silence_48k.dts)"
