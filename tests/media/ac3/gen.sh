#!/usr/bin/env bash
# Test streams for the AC-3 / E-AC-3 decoder (crates/unflash-sound): short
# ffmpeg encodes of synthetic signals in every channel layout ffmpeg's
# encoders take, at 32, 44.1 and 48 kHz and several bit rates. Each
# channel has its own content (a tone at its own frequency, a sweep, noise
# bursts out of silence at its own times), so that a channel mix-up shows,
# and the low bit rates leave many mantissas without bits (dither) and
# couple from low frequencies. The tests compare the decoder with ffmpeg's
# decode at test time; nothing else is stored.
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
  # gen NAME LAYOUT CHANNELS(tone frequencies, or "lfe") RATE CODEC BITRATE SECONDS [encoder options...]
  local name=$1 layout=$2 chans=$3 rate=$4 codec=$5 bitrate=$6 secs=$7
  shift 7
  local exprs="" i=0
  for c in $chans; do
    if [ "$c" = lfe ]; then e=$(lfe); else e=$(chan $i $c 0.$((i * 13 % 10))); fi
    exprs="${exprs:+$exprs|}$e"
    i=$((i + 1))
  done
  local ext=$codec
  ffmpeg -y -v error -f lavfi -i "aevalsrc=exprs=$exprs:s=$rate:d=$secs:c=$layout" -c:a "$codec" -b:a "$bitrate" "$@" "$name.$ext"
  printf '%-28s %7d bytes  %s\n' "$name.$ext" "$(stat -c %s "$name.$ext")" "$(ffprobe -v error -show_entries stream=sample_rate,channel_layout,bit_rate -of csv=p=0 "$name.$ext")"
}

# AC-3: every coding mode ffmpeg encodes (all but 1+1), with and without LFE
gen ac3_mono_48k            mono                      "440"                       48000 ac3 96k  1
gen ac3_stereo_44k          stereo                    "440 660"                   44100 ac3 128k 1.5
gen ac3_stereo_32k_48kbps   stereo                    "330 550"                   32000 ac3 48k  1.5
gen ac3_2f1r_48k            "3.0(back)"               "440 660 880"               48000 ac3 192k 1
gen ac3_3f_44k              "3.0"                     "440 660 550"               44100 ac3 160k 1
gen ac3_3f1r_32k            "4.0"                     "440 660 550 990"           32000 ac3 192k 1
gen ac3_2f2r_48k            "quad(side)"              "440 660 880 1100"          48000 ac3 256k 1
gen ac3_3f2r_lfe_48k        "5.1(side)"               "440 660 550 lfe 880 1100"  48000 ac3 448k 1
gen ac3_3f2r_lfe_44k_192k   "5.1(side)"               "440 660 550 lfe 880 1100"  44100 ac3 192k 1
gen ac3_1f_lfe_32k          "FC+LFE"                  "440 lfe"                   32000 ac3 96k  1
gen ac3_2f_lfe_48k          "2.1"                     "440 660 lfe"               48000 ac3 192k 1
gen ac3_2f1r_lfe_44k        "FL+FR+LFE+BC"            "440 660 lfe 880"           44100 ac3 192k 1
gen ac3_3f1r_lfe_48k        "4.1"                     "440 660 550 lfe 990"       48000 ac3 256k 1
# Annex D (bsid 6): Lo/Ro mix levels in the extended bit stream information
gen ac3_3f2r_annex_d_48k    "5.0(side)"               "440 660 550 880 1100"      48000 ac3 320k 1 -dmix_mode 2 -loro_cmixlev 1.0 -loro_surmixlev 0.5
ffmpeg -y -v error -f lavfi -i "anullsrc=r=48000:cl=stereo:d=0.5" -c:a ac3 -b:a 192k ac3_silence_48k.ac3
printf '%-28s %7d bytes\n' ac3_silence_48k.ac3 "$(stat -c %s ac3_silence_48k.ac3)"

# E-AC-3 (ffmpeg's encoder: six blocks per frame, coupling, rematrixing)
gen eac3_mono_48k           mono                      "440"                       48000 eac3 64k  1
gen eac3_stereo_48k         stereo                    "440 660"                   48000 eac3 96k  1.5
gen eac3_stereo_44k_48kbps  stereo                    "330 550"                   44100 eac3 48k  1.5
gen eac3_3f2r_lfe_48k       "5.1(side)"               "440 660 550 lfe 880 1100"  48000 eac3 384k 1
# mixing metadata: Lo/Ro levels
gen eac3_3f2r_lfe_32k_mix   "5.1(side)"               "440 660 550 lfe 880 1100"  32000 eac3 192k 1 -dmix_mode 2 -loro_cmixlev 0.5 -loro_surmixlev 0.707
