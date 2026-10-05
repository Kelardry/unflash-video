// Encode raw I420 frames from stdin with libvpx's VP9 encoder, changing the
// frame size between segments of the input without key frames, so that the
// encoder predicts from scaled references (ffmpeg's libvpx wrapper keeps
// one size for a whole stream). Writes an IVF file.
//
//   cc -o resize_enc resize_enc.c -lvpx
//   resize_enc out.ivf KBPS W1 H1 N1 [W2 H2 N2 ...] < frames.yuv
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "vpx/vp8cx.h"
#include "vpx/vpx_encoder.h"

static void put16(FILE *f, unsigned v) {
  fputc(v & 255, f);
  fputc((v >> 8) & 255, f);
}

static void put32(FILE *f, unsigned v) {
  put16(f, v & 0xffff);
  put16(f, v >> 16);
}

static int write_packets(vpx_codec_ctx_t *codec, FILE *out) {
  int n = 0;
  vpx_codec_iter_t iter = NULL;
  const vpx_codec_cx_pkt_t *pkt;
  while ((pkt = vpx_codec_get_cx_data(codec, &iter)) != NULL) {
    if (pkt->kind != VPX_CODEC_CX_FRAME_PKT) continue;
    put32(out, (unsigned)pkt->data.frame.sz);
    put32(out, (unsigned)pkt->data.frame.pts);
    put32(out, 0);
    fwrite(pkt->data.frame.buf, 1, pkt->data.frame.sz, out);
    n++;
  }
  return n;
}

static int fail(vpx_codec_ctx_t *codec, const char *what) {
  fprintf(stderr, "%s: %s\n", what, vpx_codec_error(codec));
  return 1;
}

int main(int argc, char **argv) {
  if (argc < 6 || (argc - 3) % 3 != 0) {
    fprintf(stderr, "usage: %s out.ivf KBPS W1 H1 N1 [W2 H2 N2 ...] < frames.yuv\n", argv[0]);
    return 2;
  }
  FILE *out = fopen(argv[1], "wb");
  if (!out) return 2;
  int segments = (argc - 3) / 3;
  vpx_codec_enc_cfg_t cfg;
  vpx_codec_ctx_t codec;
  if (vpx_codec_enc_config_default(vpx_codec_vp9_cx(), &cfg, 0)) return 2;
  cfg.g_w = atoi(argv[3]);
  cfg.g_h = atoi(argv[4]);
  cfg.g_timebase.num = 1;
  cfg.g_timebase.den = 30;
  cfg.rc_target_bitrate = atoi(argv[2]);
  cfg.g_lag_in_frames = 0;
  cfg.kf_mode = VPX_KF_DISABLED;
  cfg.g_threads = 1;
  if (vpx_codec_enc_init(&codec, vpx_codec_vp9_cx(), &cfg, 0)) return fail(&codec, "init");
  vpx_codec_control(&codec, VP8E_SET_CPUUSED, 4);

  // the IVF header; the frame count is filled in at the end
  fwrite("DKIF", 1, 4, out);
  put16(out, 0);
  put16(out, 32);
  fwrite("VP90", 1, 4, out);
  put16(out, cfg.g_w);
  put16(out, cfg.g_h);
  put32(out, 30);
  put32(out, 1);
  put32(out, 0);
  put32(out, 0);

  int frames = 0, pts = 0;
  for (int s = 0; s < segments; s++) {
    int w = atoi(argv[3 + 3 * s]), h = atoi(argv[4 + 3 * s]), n = atoi(argv[5 + 3 * s]);
    if (s > 0) {
      cfg.g_w = w;
      cfg.g_h = h;
      if (vpx_codec_enc_config_set(&codec, &cfg)) return fail(&codec, "resize");
    }
    vpx_image_t img;
    vpx_img_alloc(&img, VPX_IMG_FMT_I420, w, h, 1);
    int cw = (w + 1) / 2, ch = (h + 1) / 2;
    size_t size = (size_t)w * h + 2 * (size_t)cw * ch;
    unsigned char *buf = malloc(size);
    for (int i = 0; i < n; i++) {
      if (fread(buf, 1, size, stdin) != size) {
        fprintf(stderr, "short input\n");
        return 2;
      }
      unsigned char *p = buf;
      for (int y = 0; y < h; y++, p += w) memcpy(img.planes[0] + y * img.stride[0], p, w);
      for (int plane = 1; plane < 3; plane++)
        for (int y = 0; y < ch; y++, p += cw) memcpy(img.planes[plane] + y * img.stride[plane], p, cw);
      if (vpx_codec_encode(&codec, &img, pts++, 1, 0, VPX_DL_GOOD_QUALITY)) return fail(&codec, "encode");
      frames += write_packets(&codec, out);
    }
    free(buf);
    vpx_img_free(&img);
  }
  if (vpx_codec_encode(&codec, NULL, -1, 1, 0, VPX_DL_GOOD_QUALITY)) return fail(&codec, "flush");
  frames += write_packets(&codec, out);
  fseek(out, 24, SEEK_SET);
  put32(out, frames);
  fclose(out);
  vpx_codec_destroy(&codec);
  return 0;
}
