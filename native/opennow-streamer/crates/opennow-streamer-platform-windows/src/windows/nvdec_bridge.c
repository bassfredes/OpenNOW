/* Local opt-in experiment. NVDEC -> bounded CPU packing -> D3D11 upload.
 * No software fallback and no chroma reduction. FFmpeg owns the CUDA context.
 *
 * COBJMACROS must be defined before any Windows COM header: the Windows SDK
 * declares ID3D11*_AddRef/GetDesc as functions unless the C accessor macros
 * are enabled, and both the D3D11VA route and the optional GPU interop call
 * them from C.
 */
#ifndef COBJMACROS
#define COBJMACROS
#endif
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <libavcodec/avcodec.h>
#include <libavutil/opt.h>
#include <libavutil/pixdesc.h>
#include <libavutil/hwcontext.h>
#ifdef OPENNOW_GPU_INTEROP
#define COBJMACROS
#include <d3d11.h>
#include <cuda.h>
#include <cudaD3D11.h>
#include <libavutil/hwcontext_cuda.h>
typedef struct ONGpuSlot ONGpuSlot;
void on_nvdec_gpu_release(ONGpuSlot *slot);
#define ON_GPU_SLOTS 8
#endif

typedef struct {
    AVCodecContext *ctx;
    AVFrame *frame;
    AVFrame *hardware_frame;
    int low_latency;
    int width, height, depth;
#ifdef OPENNOW_GPU_INTEROP
    ID3D11Device *d3d_device;
    ONGpuSlot *gpu_slots[ON_GPU_SLOTS];
#endif
} ONNvdec;

typedef struct {
    int64_t pts, duration;
    int range, primaries, transfer, matrix;
    int pixel_format, width, height, source_depth, output_layout;
} ONFrameInfo;

const char *on_nvdec_pixel_format_name(int format) { return av_get_pix_fmt_name(format); }

/* Inspect a complete random-access unit without decoding or altering it.
 * Only an active SPS/PPS referenced by a parsed picture proves the layout.
 * The parser's pixel format IS the bitstream's own depth and chroma; no
 * requested depth may mask them, because the decoder must match what the
 * seat actually encoded (2026-09-24: a 10-bit request left an 8-bit 4:2:0
 * stream on a decoder that could never present it).
 * Return the bridge layout, or -1 when metadata is absent/incompatible. */
int on_hevc_keyframe_layout(const uint8_t *data, int size, int width, int height) {
    if (!data || size <= 0 || size > 32*1024*1024) return -1;
    AVCodecParserContext *parser = av_parser_init(AV_CODEC_ID_HEVC);
    AVCodecContext *ctx = avcodec_alloc_context3(NULL);
    AVPacket *packet = av_packet_alloc();
    int layout = -1;
    if (!parser || !ctx || !packet || av_new_packet(packet, size) < 0) goto done;
    memcpy(packet->data, data, size); /* av_new_packet supplies required zero padding */
    ctx->codec_id = AV_CODEC_ID_HEVC;
    parser->flags |= PARSER_FLAG_COMPLETE_FRAMES;
    uint8_t *output = NULL;
    int output_size = 0;
    int parsed = av_parser_parse2(parser, ctx, &output, &output_size,
        packet->data, size, 0, 0, 0);
    if (parsed < 0 || output_size != size || parser->key_frame != 1 ||
        parser->width != width || parser->height != height) goto done;
    if (parser->format == AV_PIX_FMT_YUV420P10LE) layout = 3;
    if (parser->format == AV_PIX_FMT_YUV444P10LE) layout = 1;
    if (parser->format == AV_PIX_FMT_YUV420P) layout = 2;
    if (parser->format == AV_PIX_FMT_YUV444P) layout = 0;
done:
    av_packet_free(&packet);
    avcodec_free_context(&ctx);
    if (parser) av_parser_close(parser);
    return layout;
}

void on_nvdec_close(ONNvdec *d) {
    if (!d) return;
#ifdef OPENNOW_GPU_INTEROP
    for (int i = 0; i < ON_GPU_SLOTS; ++i) on_nvdec_gpu_release(d->gpu_slots[i]);
    if (d->d3d_device) ID3D11Device_Release(d->d3d_device);
#endif
    av_frame_free(&d->frame);
    av_frame_free(&d->hardware_frame);
    avcodec_free_context(&d->ctx);
    free(d);
}

/* Refuse software output. FFmpeg's native HEVC parser submits complete access
 * units to NVDEC without CUVID's one-access-unit parser holdback. */
static enum AVPixelFormat cuda_only_format(AVCodecContext *ctx, const enum AVPixelFormat *formats) {
    (void)ctx;
    for (; *formats != AV_PIX_FMT_NONE; ++formats)
        if (*formats == AV_PIX_FMT_CUDA) return *formats;
    return AV_PIX_FMT_NONE;
}

ONNvdec *on_nvdec_open(int width, int height, int depth) {
    /* Match VideoFormat and application settings; codec/GPU support is still
     * checked by FFmpeg/NVDEC. Preserve exact-size validation on receive. */
    if (width < 48 || width > 7680 || height < 48 || height > 4320 ||
        (depth != 8 && depth != 10)) return NULL;
    const char *low_latency_flag = getenv("OPENNOW_NVDEC_LOW_LATENCY");
    int low_latency = low_latency_flag && strcmp(low_latency_flag, "1") == 0;
    const AVCodec *codec = avcodec_find_decoder_by_name(low_latency ? "hevc" : "hevc_cuvid");
    if (!codec) return NULL;
    ONNvdec *d = calloc(1, sizeof(*d));
    if (!d) return NULL;
    d->ctx = avcodec_alloc_context3(codec);
    d->frame = av_frame_alloc();
    d->hardware_frame = av_frame_alloc();
    d->low_latency = low_latency;
    d->width = width; d->height = height; d->depth = depth;
    if (!d->ctx || !d->frame || !d->hardware_frame) { on_nvdec_close(d); return NULL; }
    d->ctx->width = width; d->ctx->height = height;
    d->ctx->pkt_timebase = (AVRational){1, 10000000};
    d->ctx->flags |= AV_CODEC_FLAG_LOW_DELAY;
    d->ctx->thread_count = 1;
    AVDictionary *opts = NULL;
    if (low_latency) {
        d->ctx->get_format = cuda_only_format;
        if (av_hwdevice_ctx_create(&d->ctx->hw_device_ctx, AV_HWDEVICE_TYPE_CUDA,
                                   "0", NULL, 0) < 0) {
            on_nvdec_close(d);
            return NULL;
        }
    } else {
        av_dict_set(&opts, "gpu", "0", 0);
        av_dict_set(&opts, "surfaces", "8", 0);
    }
    int result = avcodec_open2(d->ctx, codec, &opts);
    av_dict_free(&opts);
    if (result < 0) { on_nvdec_close(d); return NULL; }
    return d;
}

int on_nvdec_send(ONNvdec *d, const uint8_t *data, int size,
                  int64_t pts, int64_t duration, int key) {
    if (!d || !data || size <= 0 || size > 32*1024*1024) return -1;
    AVPacket *p = av_packet_alloc();
    if (!p) return -1;
    int result = av_new_packet(p, size);
    if (result >= 0) {
        memcpy(p->data, data, size);
        p->pts = pts; p->dts = pts; p->duration = duration;
        if (key) p->flags |= AV_PKT_FLAG_KEY;
        result = avcodec_send_packet(d->ctx, p);
    }
    av_packet_free(&p);
    return result;
}

int on_nvdec_drain(ONNvdec *d) { return avcodec_send_packet(d->ctx, NULL); }

/* Reset reference/parser state while retaining FFmpeg's CUDA device context.
 * Reopening the entire decoder also recreates CUDA and can overflow the live
 * compressed queue before the recovery keyframe has even been submitted. */
void on_nvdec_reset(ONNvdec *d) {
    avcodec_flush_buffers(d->ctx);
    av_frame_unref(d->frame);
    av_frame_unref(d->hardware_frame);
}

/* Right shift that turns a decoded 4:4:4 word into the 10-bit Y410 code.
 * MSB (left)-aligned formats — the rebuilt FFmpeg emits
 * AV_PIX_FMT_YUV444P10MSB and, for 12-bit streams, YUV444P12MSB — store the
 * code in the high bits (code << (16 - depth)): a 10-bit code lives at <<6,
 * and a 12-bit code downconverts with the same >>6 (its top ten bits).
 * LSB-aligned yuv444p10le needs no shift; yuv444p16le carries
 * left-aligned data from cuvid and keeps the historical >>6. */
int on_nvdec_y410_shift(int av_pix_fmt) {
    return (av_pix_fmt == AV_PIX_FMT_YUV444P16LE ||
            av_pix_fmt == AV_PIX_FMT_YUV444P10MSB ||
            av_pix_fmt == AV_PIX_FMT_YUV444P12MSB) ? 6 : 0;
}

/* Returns 1 for one exact-format frame, 0 for no output, negative for failure. */
int on_nvdec_receive(ONNvdec *d, uint32_t *out, size_t pixels, ONFrameInfo *info) {
    if (!d || !out || !info || pixels != (size_t)d->width*d->height) return -1;
    av_frame_unref(d->frame);
    av_frame_unref(d->hardware_frame);
    int result = avcodec_receive_frame(d->ctx, d->low_latency ? d->hardware_frame : d->frame);
    if (result == AVERROR(EAGAIN) || result == AVERROR_EOF) return 0;
    if (result < 0) return result;
    if (d->low_latency) {
        if (d->hardware_frame->format != AV_PIX_FMT_CUDA) return -4;
        result = av_hwframe_transfer_data(d->frame, d->hardware_frame, 0);
        if (result < 0) return result;
        result = av_frame_copy_props(d->frame, d->hardware_frame);
        if (result < 0) return result;
    }
    AVFrame *f = d->frame;
    const AVPixFmtDescriptor *description = av_pix_fmt_desc_get(f->format);
    info->pixel_format = f->format; info->width = f->width; info->height = f->height;
    info->source_depth = description ? description->comp[0].depth : 0;
    info->pts = f->pts; info->duration = f->duration;
    info->range = f->color_range; info->primaries = f->color_primaries;
    info->transfer = f->color_trc; info->matrix = f->colorspace;
    if (f->width != d->width || f->height != d->height || f->decode_error_flags ||
        f->crop_left || f->crop_right || f->crop_top || f->crop_bottom ||
        f->pts == AV_NOPTS_VALUE) return -2;
    /* Preserve a server's real 4:2:0 output and report that layout to the
     * renderer. Never upsample and call it 4:4:4. The buffer is sized for
     * four bytes/pixel, which also bounds these native semi-planar layouts. */
    if ((d->depth == 10 && f->format == AV_PIX_FMT_P010LE) ||
        (d->depth == 8 && f->format == AV_PIX_FMT_NV12)) {
        if ((d->width & 1) || (d->height & 1)) return -2;
        size_t pitch = (size_t)d->width * (d->depth == 10 ? 2 : 1);
        uint8_t *bytes = (uint8_t *)out;
        for (int plane = 0; plane < 2; ++plane) {
            int rows = plane == 0 ? d->height : d->height / 2;
            if (!f->data[plane] || f->linesize[plane] < (int)pitch) return -2;
            uint8_t *target = bytes + (plane == 0 ? 0 : pitch*d->height);
            for (int y = 0; y < rows; ++y)
                memcpy(target + (size_t)y*pitch,
                       f->data[plane] + (ptrdiff_t)y*f->linesize[plane], pitch);
        }
        info->output_layout = d->depth == 10 ? 3 : 2;
        return 1;
    }
    if ((d->depth == 8 && f->format != AV_PIX_FMT_YUV444P) ||
        (d->depth == 10 && f->format != AV_PIX_FMT_YUV444P16LE &&
         f->format != AV_PIX_FMT_YUV444P10LE &&
         f->format != AV_PIX_FMT_YUV444P10MSB &&
         f->format != AV_PIX_FMT_YUV444P12MSB)) return -3;
    info->output_layout = d->depth == 10 ? 1 : 0;
    for (int y = 0; y < d->height; ++y) {
        const uint8_t *yp = f->data[0] + (ptrdiff_t)y*f->linesize[0];
        const uint8_t *up = f->data[1] + (ptrdiff_t)y*f->linesize[1];
        const uint8_t *vp = f->data[2] + (ptrdiff_t)y*f->linesize[2];
        uint32_t *row = out + (size_t)y*d->width;
        if (d->depth == 8) {
            for (int x = 0; x < d->width; ++x)
                row[x] = 0xff000000u | ((uint32_t)yp[x]<<16) | ((uint32_t)up[x]<<8) | vp[x];
        } else {
            int shift = on_nvdec_y410_shift(f->format);
            for (int x = 0; x < d->width; ++x) {
                uint32_t Y = ((const uint16_t *)yp)[x] >> shift;
                uint32_t U = ((const uint16_t *)up)[x] >> shift;
                uint32_t V = ((const uint16_t *)vp)[x] >> shift;
                row[x] = 0xc0000000u | (V<<20) | (Y<<10) | U;
            }
        }
    }
    info->pts = f->pts; info->duration = f->duration;
    info->range = f->color_range; info->primaries = f->color_primaries;
    info->transfer = f->color_trc; info->matrix = f->colorspace;
    return 1;
}

#ifdef OPENNOW_GPU_INTEROP
#include "nvdec_gpu_bridge.h"
#endif

/* -------------------------------------------------------------------------
 * D3D11VA (DXVA) zero-copy route for real HEVC 4:4:4.
 *
 * FFmpeg's stock dxva_modes table only ever advertised HEVC Main/Main10
 * (4:2:0); with the repository patch
 * vendor/patches/ffmpeg-d3d11va-hevc-444.patch the driver's Range Extensions
 * 4:4:4 profiles are selected instead, decoding straight into the array
 * texture of a pool created on OUR ID3D11Device (AV_PIX_FMT_D3D11) — no CPU
 * copy, unlike the CUDA interop that caused device loss and is banned.
 * ------------------------------------------------------------------------- */
#include <libavutil/hwcontext_d3d11va.h>

typedef struct OND3d11va {
    AVCodecContext *ctx;
    AVFrame *frame;
    int width, height, depth;
} OND3d11va;

/* Only the hardware surface in our device is acceptable: a software frame
 * here would mean the hwaccel silently disengaged (stock FFmpeg behaviour
 * for Range Extensions streams), and continuing would burn CPU at 5K. */
static enum AVPixelFormat on_d3d11va_get_format(AVCodecContext *ctx,
                                                const enum AVPixelFormat *fmts) {
    (void)ctx;
    for (const enum AVPixelFormat *f = fmts; *f != AV_PIX_FMT_NONE; ++f)
        if (*f == AV_PIX_FMT_D3D11)
            return AV_PIX_FMT_D3D11;
    return AV_PIX_FMT_NONE;
}

static void on_d3d11va_error(char *errbuf, int errbuf_len,
                             const char *what, int code) {
    if (!errbuf || errbuf_len <= 0)
        return;
    char detail[AV_ERROR_MAX_STRING_SIZE];
    detail[0] = 0;
    if (code)
        av_strerror(code, detail, sizeof(detail));
    snprintf(errbuf, (size_t)errbuf_len, "%s: %s", what, detail);
}

/* Shared recursive lock bridging FFmpeg's D3D11VA device callbacks and the
 * render-side Y410 conversion. AVD3D11VADeviceContext requires a RECURSIVE
 * lock; a CRITICAL_SECTION provides it, and the single process-wide instance
 * lets both sides serialize immediate-context work (FFmpeg's default lock
 * would only serialize FFmpeg's own calls, not Qt's render thread). */
static INIT_ONCE g_opennow_d3d11_lock_init = INIT_ONCE_STATIC_INIT;
static CRITICAL_SECTION g_opennow_d3d11_lock;

static BOOL CALLBACK on_d3d11va_init_lock(PINIT_ONCE once, PVOID param, PVOID *context) {
    (void)once; (void)param; (void)context;
    InitializeCriticalSection(&g_opennow_d3d11_lock);
    return TRUE;
}

void on_d3d11va_lock(void *ctx) {
    EnterCriticalSection((CRITICAL_SECTION *)ctx);
}

void on_d3d11va_unlock(void *ctx) {
    LeaveCriticalSection((CRITICAL_SECTION *)ctx);
}

void *on_d3d11va_shared_lock_ctx(void) {
    InitOnceExecuteOnce(&g_opennow_d3d11_lock_init, on_d3d11va_init_lock, NULL, NULL);
    return &g_opennow_d3d11_lock;
}

/* Shares the caller's ID3D11Device (AddRef'd; released with the hw device
 * context). Returns NULL with errbuf describing the failure so the Rust
 * side can log it and fall back to the NVDEC path. */
OND3d11va *on_d3d11va_open(void *device, int width, int height, int depth,
                           char *errbuf, int errbuf_len) {
    if (!device) {
        on_d3d11va_error(errbuf, errbuf_len, "null D3D11 device", 0);
        return NULL;
    }
    if (width < 48 || width > 7680 || height < 48 || height > 4320) {
        on_d3d11va_error(errbuf, errbuf_len, "unsupported decode extent", 0);
        return NULL;
    }
    if (depth != 8 && depth != 10) {
        on_d3d11va_error(errbuf, errbuf_len, "unsupported bit depth", 0);
        return NULL;
    }
    const AVCodec *codec = avcodec_find_decoder(AV_CODEC_ID_HEVC);
    if (!codec) {
        on_d3d11va_error(errbuf, errbuf_len, "FFmpeg HEVC decoder missing", 0);
        return NULL;
    }
    AVBufferRef *hwref = av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_D3D11VA);
    if (!hwref) {
        on_d3d11va_error(errbuf, errbuf_len, "av_hwdevice_ctx_alloc", AVERROR(ENOMEM));
        return NULL;
    }
    AVHWDeviceContext *hwc = (AVHWDeviceContext *)hwref->data;
    AVD3D11VADeviceContext *d3d = (AVD3D11VADeviceContext *)hwc->hwctx;
    /* The only mandatory field; device_context/video_device/video_context are
     * derived from it on init, so decode happens on the same (multithread-
     * protected) immediate context Qt presents from. */
    ID3D11Device_AddRef((ID3D11Device *)device);
    d3d->device = (ID3D11Device *)device;
    /* Serialize every immediate-context use with the render side: the same
     * recursive lock is taken around the Qt Y410 conversion copy/draw/execute
     * (see on_d3d11va_shared_lock_ctx), so decoder submissions on the worker
     * thread and the render thread can never interleave on the shared
     * context — the live-only block-smear hypothesis. */
    d3d->lock = on_d3d11va_lock;
    d3d->unlock = on_d3d11va_unlock;
    d3d->lock_ctx = on_d3d11va_shared_lock_ctx();
    int result = av_hwdevice_ctx_init(hwref);
    if (result < 0) {
        av_buffer_unref(&hwref);
        on_d3d11va_error(errbuf, errbuf_len, "av_hwdevice_ctx_init", result);
        return NULL;
    }
    OND3d11va *d = (OND3d11va *)calloc(1, sizeof(*d));
    if (!d) {
        av_buffer_unref(&hwref);
        on_d3d11va_error(errbuf, errbuf_len, "decoder allocation", AVERROR(ENOMEM));
        return NULL;
    }
    d->ctx = avcodec_alloc_context3(codec);
    d->frame = av_frame_alloc();
    if (!d->ctx || !d->frame) {
        av_buffer_unref(&hwref);
        avcodec_free_context(&d->ctx);
        av_frame_free(&d->frame);
        free(d);
        on_d3d11va_error(errbuf, errbuf_len, "decoder allocation", AVERROR(ENOMEM));
        return NULL;
    }
    d->ctx->hw_device_ctx = av_buffer_ref(hwref);
    av_buffer_unref(&hwref);
    d->ctx->get_format = on_d3d11va_get_format;
    d->ctx->thread_count = 1;
    d->ctx->flags |= AV_CODEC_FLAG_LOW_DELAY;
    d->ctx->pkt_timebase = (AVRational){1, 1000000};
    /* The default pool is 1 base + 16 reference surfaces for HEVC, which
     * only covers the DPB. Every frame we hand to the presenter holds its
     * pool slot (the lease), so the pool must also cover the presenter
     * queue and the reorder tail: 16 extra surfaces keep DPB + leases from
     * ever exhausting, which would drop reference frames and smear blocks. */
    d->ctx->extra_hw_frames = 16;
    result = avcodec_open2(d->ctx, codec, NULL);
    if (result < 0) {
        on_d3d11va_error(errbuf, errbuf_len, "avcodec_open2", result);
        avcodec_free_context(&d->ctx);
        av_frame_free(&d->frame);
        free(d);
        return NULL;
    }
    d->width = width;
    d->height = height;
    d->depth = depth;
    if (errbuf && errbuf_len > 0)
        errbuf[0] = 0;
    return d;
}

int on_d3d11va_send(OND3d11va *d, const uint8_t *data, int size,
                    int64_t pts, int64_t duration, int key) {
    if (!d || !data || size <= 0 || size > 32 * 1024 * 1024)
        return -1;
    AVPacket *p = av_packet_alloc();
    if (!p)
        return -1;
    int result = av_new_packet(p, size);
    if (result >= 0) {
        memcpy(p->data, data, size);
        p->pts = pts;
        p->dts = pts;
        p->duration = duration;
        if (key)
            p->flags |= AV_PKT_FLAG_KEY;
        result = avcodec_send_packet(d->ctx, p);
    }
    av_packet_free(&p);
    return result;
}

int on_d3d11va_drain(OND3d11va *d) {
    return d ? avcodec_send_packet(d->ctx, NULL) : -1;
}

/* In-place flush for recovery keyframes: keeps the device and decoder
 * instances, so recovery never restarts the whole decoder. */
void on_d3d11va_flush(OND3d11va *d) {
    if (!d)
        return;
    avcodec_flush_buffers(d->ctx);
    av_frame_unref(d->frame);
}

void on_d3d11va_close(OND3d11va *d) {
    if (!d)
        return;
    avcodec_free_context(&d->ctx);
    av_frame_free(&d->frame);
    free(d);
}

/* Returns 1 with a frame: *texture is an AddRef'd view of the pool's array
 * texture (valid independently of *lease), *subresource addresses the array
 * element, and *lease is an AVFrame clone that pins the pool slot until
 * released. 0 = need more input, negative = validation failure (-4 not a
 * hardware frame, -5 pool format mismatch). */
int on_d3d11va_receive(OND3d11va *d, ID3D11Texture2D **texture,
                       unsigned *subresource, void **lease, ONFrameInfo *info) {
    if (!d || !texture || !subresource || !lease || !info)
        return -1;
    av_frame_unref(d->frame);
    int result = avcodec_receive_frame(d->ctx, d->frame);
    if (result == AVERROR(EAGAIN) || result == AVERROR_EOF)
        return 0;
    if (result < 0)
        return result;
    if (d->frame->format != AV_PIX_FMT_D3D11 || !d->frame->data[0])
        return -4;
    ID3D11Texture2D *tex = (ID3D11Texture2D *)d->frame->data[0];
    D3D11_TEXTURE2D_DESC desc;
    ID3D11Texture2D_GetDesc(tex, &desc);
    int want = d->depth == 10 ? DXGI_FORMAT_Y410 : DXGI_FORMAT_AYUV;
    if ((int)desc.Format != want)
        return -5;
    AVFrame *held = av_frame_clone(d->frame);
    if (!held)
        return AVERROR(ENOMEM);
    intptr_t index = (intptr_t)d->frame->data[1];
    info->pixel_format = 0; /* Rust maps the DXGI format via output_layout */
    info->width = d->width;
    info->height = d->height;
    info->source_depth = d->depth;
    info->output_layout = d->depth == 10 ? 1 : 0; /* 1=Y410, 0=AYUV (NVDEC order) */
    info->pts = d->frame->pts;
    info->duration = d->frame->duration;
    info->range = d->frame->color_range;
    info->primaries = d->frame->color_primaries;
    info->transfer = d->frame->color_trc;
    info->matrix = d->frame->colorspace;
    ID3D11Texture2D_AddRef(tex);
    *lease = held;
    *texture = tex;
    *subresource = (unsigned)index * (desc.MipLevels ? desc.MipLevels : 1u);
    return 1;
}

void on_d3d11va_release_lease(void *lease) {
    AVFrame *frame = (AVFrame *)lease;
    av_frame_free(&frame);
}
