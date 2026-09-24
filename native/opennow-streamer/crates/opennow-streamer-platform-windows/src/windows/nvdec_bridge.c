/* Local opt-in experiment. NVDEC -> bounded CPU packing -> D3D11 upload.
 * No software fallback and no chroma reduction. FFmpeg owns the CUDA context.
 */
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
 * Return the bridge layout, or -1 when metadata is absent/incompatible. */
int on_hevc_keyframe_layout(const uint8_t *data, int size, int width, int height, int depth) {
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
    if (depth == 10 && parser->format == AV_PIX_FMT_YUV420P10LE) layout = 3;
    if (depth == 10 && parser->format == AV_PIX_FMT_YUV444P10LE) layout = 1;
    if (depth == 8 && parser->format == AV_PIX_FMT_YUV420P) layout = 2;
    if (depth == 8 && parser->format == AV_PIX_FMT_YUV444P) layout = 0;
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
         f->format != AV_PIX_FMT_YUV444P10LE)) return -3;
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
            int shift = f->format == AV_PIX_FMT_YUV444P16LE ? 6 : 0;
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
