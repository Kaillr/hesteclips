// libavcodec's H.264 decoder for playback (`decode.rs`): the few calls it
// needs, behind a small interface of its own. Built against libavcodec's
// headers, so its structs are read as the library lays them out; the
// library itself is opened at run time (from lib/ beside the app, where the
// release ships it, else the system's), so the app starts without it and
// playback falls back to ffmpeg processes.
//
// Given a CUDA context (`cuda.c`), it decodes on NVIDIA's card (NVDEC), and
// frames stay there: `hcav_frame_cuda` says where, for copying them on.
// Given a VA-API device (`hcav_vaapi_open`: AMD's and Intel's cards), the
// same through VA-API: `hcav_frame_dmabuf` hands each frame's planes over
// as DMA-BUFs, for Vulkan to copy from (`dmabuf.rs`).
//
// Built without the headers (HCAV_NO_LIBAV), loading always fails.

// RTLD_DEFAULT, RTLD_NOLOAD.
#define _GNU_SOURCE
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#define HCAV_FRAME 1
#define HCAV_AGAIN 0
#define HCAV_END (-1)
#define HCAV_ERROR (-2)
#define HCAV_SENT 2

// Where frames are decoded (`hcav_open`).
#define HCAV_CPU 0
#define HCAV_CUDA 1
#define HCAV_VAAPI 2

// A plane of a frame on the card, as a DMA-BUF (`hcav_frame_dmabuf`): the
// buffer (`fd`, `size` bytes, laid out as `modifier` says), and where the
// plane starts in it and its rows' pitch.
typedef struct {
    int fd;
    uint64_t size;
    uint64_t modifier;
    uint64_t offset;
    uint64_t pitch;
} HcPlane;

#ifdef HCAV_NO_LIBAV

int hcav_load(const char *dir, char *err, int errlen) {
    (void)dir;
    snprintf(err, errlen, "built without libavcodec's headers");
    return -1;
}
void *hcav_open(const uint8_t *avcc, int avcc_len, int threads, int kind, void *hw, char *err, int errlen) {
    (void)avcc; (void)avcc_len; (void)threads; (void)kind; (void)hw;
    snprintf(err, errlen, "built without libavcodec's headers");
    return NULL;
}
void *hcav_vaapi_open(const char *node, char *err, int errlen) {
    (void)node;
    snprintf(err, errlen, "built without libavcodec's headers");
    return NULL;
}
void hcav_vaapi_close(void *device) { (void)device; }
void hcav_vaapi_vendor(void *device, char *out, int len) { (void)device; if (len > 0) out[0] = 0; }
int hcav_frame_cuda(void *d, uint64_t *y, int *y_pitch, uint64_t *uv, int *uv_pitch) { (void)d; (void)y; (void)y_pitch; (void)uv; (void)uv_pitch; return -1; }
int hcav_frame_dmabuf(void *d, HcPlane *planes) { (void)d; (void)planes; return -1; }
void hcav_unmap(void *d) { (void)d; }
int hcav_download(void *d) { (void)d; return -1; }
int hcav_send(void *d, const uint8_t *data, int size, int64_t pts, int key, int *code) { (void)d; (void)data; (void)size; (void)pts; (void)key; *code = 0; return HCAV_ERROR; }
int hcav_receive(void *d, int64_t *pts, int *code) { (void)d; (void)pts; *code = 0; return HCAV_ERROR; }
int hcav_take_nv12(void *d, uint8_t *out, int width, int height) { (void)d; (void)out; (void)width; (void)height; return -1; }
void hcav_flush(void *d) { (void)d; }
void hcav_close(void *d) { (void)d; }
void hcav_error_text(int code, char *out, int len) { (void)code; snprintf(out, len, "no libavcodec"); }

#else

#include <dlfcn.h>
#include <libavcodec/avcodec.h>
#include <libavutil/avutil.h>
#include <libavutil/hwcontext.h>
#include <libavutil/log.h>
#include <libswresample/version_major.h>
// CUDA's types as NVIDIA's headers have them (as ffmpeg is built with), for
// its CUDA device context.
#include "ffnvcodec/dynlink_cuda.h"
#include <libavutil/hwcontext_cuda.h>
// DMA-BUFs (VA-API's frames handed over); needs nothing of libdrm's.
#include <libavutil/hwcontext_drm.h>

#define STR2(x) #x
#define STR(x) STR2(x)

static struct {
    int loaded;
    const AVCodec *(*find_decoder)(enum AVCodecID);
    AVCodecContext *(*alloc_context3)(const AVCodec *);
    int (*open2)(AVCodecContext *, const AVCodec *, AVDictionary **);
    void (*free_context)(AVCodecContext **);
    int (*send_packet)(AVCodecContext *, const AVPacket *);
    int (*receive_frame)(AVCodecContext *, AVFrame *);
    void (*flush_buffers)(AVCodecContext *);
    AVPacket *(*packet_alloc)(void);
    void (*packet_free)(AVPacket **);
    AVFrame *(*frame_alloc)(void);
    void (*frame_free)(AVFrame **);
    void (*frame_unref)(AVFrame *);
    void *(*mallocz)(size_t);
    void (*free_)(void *);
    int (*strerror_)(int, char *, size_t);
    void (*log_set_level)(int);
    // Decoding on the card: where these are there (`hw`).
    int hw;
    AVBufferRef *(*hwdevice_ctx_alloc)(enum AVHWDeviceType);
    int (*hwdevice_ctx_init)(AVBufferRef *);
    AVBufferRef *(*buffer_ref)(const AVBufferRef *);
    void (*buffer_unref)(AVBufferRef **);
    int (*hwframe_transfer_data)(AVFrame *, const AVFrame *, int);
    enum AVPixelFormat (*default_get_format)(struct AVCodecContext *, const enum AVPixelFormat *);
    // Through VA-API: where these are there too (`vaapi`).
    int vaapi;
    int (*hwdevice_ctx_create)(AVBufferRef **, enum AVHWDeviceType, const char *, AVDictionary *, int);
    int (*hwframe_map)(AVFrame *, const AVFrame *, int);
} av;

// `soname` from `dir` if it's there, else wherever the system has it.
static void *open_lib(const char *dir, const char *soname) {
    void *h = NULL;
    if (dir && *dir) {
        char path[4096];
        snprintf(path, sizeof path, "%s/%s", dir, soname);
        h = dlopen(path, RTLD_NOW | RTLD_GLOBAL);
    }
    if (!h) {
        h = dlopen(soname, RTLD_NOW | RTLD_GLOBAL);
    }
    return h;
}

// Open the libraries (once). 0 when they're there.
int hcav_load(const char *dir, char *err, int errlen) {
    if (av.loaded) {
        return 0;
    }
    // libavcodec's own libraries first, from the same place: then it finds
    // them already open, the versions it was built with.
    void *util = open_lib(dir, "libavutil.so." STR(LIBAVUTIL_VERSION_MAJOR));
    open_lib(dir, "libswresample.so." STR(LIBSWRESAMPLE_VERSION_MAJOR));
    void *codec = open_lib(dir, "libavcodec.so." STR(LIBAVCODEC_VERSION_MAJOR));
    if (!util || !codec) {
        snprintf(err, errlen, "libavcodec %d isn't installed (%s)", LIBAVCODEC_VERSION_MAJOR, dlerror());
        return -1;
    }
#define SYM(lib, field, name)                                                  \
    do {                                                                       \
        *(void **)&av.field = dlsym(lib, name);                                \
        if (!av.field) {                                                       \
            snprintf(err, errlen, "libavcodec has no %s", name);               \
            return -1;                                                         \
        }                                                                      \
    } while (0)
    SYM(codec, find_decoder, "avcodec_find_decoder");
    SYM(codec, alloc_context3, "avcodec_alloc_context3");
    SYM(codec, open2, "avcodec_open2");
    SYM(codec, free_context, "avcodec_free_context");
    SYM(codec, send_packet, "avcodec_send_packet");
    SYM(codec, receive_frame, "avcodec_receive_frame");
    SYM(codec, flush_buffers, "avcodec_flush_buffers");
    SYM(codec, packet_alloc, "av_packet_alloc");
    SYM(codec, packet_free, "av_packet_free");
    SYM(util, frame_alloc, "av_frame_alloc");
    SYM(util, frame_free, "av_frame_free");
    SYM(util, frame_unref, "av_frame_unref");
    SYM(util, mallocz, "av_mallocz");
    SYM(util, free_, "av_free");
    SYM(util, strerror_, "av_strerror");
    SYM(util, log_set_level, "av_log_set_level");
#undef SYM
    // Optional: without them, frames are decoded on the CPU.
    *(void **)&av.hwdevice_ctx_alloc = dlsym(util, "av_hwdevice_ctx_alloc");
    *(void **)&av.hwdevice_ctx_init = dlsym(util, "av_hwdevice_ctx_init");
    *(void **)&av.buffer_ref = dlsym(util, "av_buffer_ref");
    *(void **)&av.buffer_unref = dlsym(util, "av_buffer_unref");
    *(void **)&av.hwframe_transfer_data = dlsym(util, "av_hwframe_transfer_data");
    *(void **)&av.default_get_format = dlsym(codec, "avcodec_default_get_format");
    av.hw = av.hwdevice_ctx_alloc && av.hwdevice_ctx_init && av.buffer_ref && av.buffer_unref && av.hwframe_transfer_data && av.default_get_format;
    *(void **)&av.hwdevice_ctx_create = dlsym(util, "av_hwdevice_ctx_create");
    *(void **)&av.hwframe_map = dlsym(util, "av_hwframe_map");
    av.vaapi = av.hw && av.hwdevice_ctx_create && av.hwframe_map;
    // A damaged frame is skipped, not reported line by line.
    av.log_set_level(AV_LOG_FATAL);
    av.loaded = 1;
    return 0;
}

void hcav_close(void *p);

typedef struct {
    AVCodecContext *ctx;
    AVPacket *pkt;
    AVFrame *frame;
    // A frame on the card copied down (`hcav_download`).
    AVFrame *down;
    // A frame decoded through VA-API, handed over as DMA-BUFs
    // (`hcav_frame_dmabuf`) until `hcav_unmap`.
    AVFrame *mapped;
    // A frame came out and hasn't been let go of.
    int holding;
    // The packet's bytes, with the zeroed padding the decoder reads past the end.
    uint8_t *buf;
    size_t cap;
} Dec;

void hcav_error_text(int code, char *out, int len) {
    if (!av.loaded || av.strerror_(code, out, (size_t)len) < 0) {
        snprintf(out, len, "error %d", code);
    }
}

// The card's decoder (the format in `opaque`: CUDA's or VA-API's) where
// libavcodec offers it for the stream, else the CPU.
static enum AVPixelFormat pick_hw(AVCodecContext *ctx, const enum AVPixelFormat *formats) {
    enum AVPixelFormat want = (enum AVPixelFormat)(intptr_t)ctx->opaque;
    for (const enum AVPixelFormat *f = formats; *f != AV_PIX_FMT_NONE; f++) {
        if (*f == want) return *f;
    }
    return av.default_get_format(ctx, formats);
}

// libavutil's VA-API device on the render node `node` (/dev/dri/renderD128,
// say), for `hcav_open`: NULL, with why in `err`, where there's no VA-API
// driver for the card. Shared by the decoders on the card; let go of with
// `hcav_vaapi_close` (it lasts until the last decoder on it closes).
void *hcav_vaapi_open(const char *node, char *err, int errlen) {
    if (!av.loaded || !av.vaapi) {
        snprintf(err, errlen, "this libavcodec can't decode through VA-API");
        return NULL;
    }
    AVBufferRef *device = NULL;
    int r = av.hwdevice_ctx_create(&device, AV_HWDEVICE_TYPE_VAAPI, node, NULL, 0);
    if (r < 0) {
        char text[128];
        hcav_error_text(r, text, sizeof text);
        snprintf(err, errlen, "no VA-API driver on %s (%s)", node, text);
        return NULL;
    }
    return device;
}

void hcav_vaapi_close(void *device) {
    AVBufferRef *ref = device;
    if (ref) av.buffer_unref(&ref);
}

// The VA-API driver's name for itself ("Mesa Gallium driver … for AMD
// Radeon …", "Intel iHD driver …"): empty if libva won't say.
void hcav_vaapi_vendor(void *device, char *out, int len) {
    if (len <= 0) return;
    out[0] = 0;
    if (!device) return;
    AVHWDeviceContext *ctx = (AVHWDeviceContext *)((AVBufferRef *)device)->data;
    // AVVAAPIDeviceContext starts with its VADisplay (its header needs
    // libva's; libavutil has libva open).
    void *display = ctx->hwctx ? *(void **)ctx->hwctx : NULL;
    const char *(*query)(void *) = NULL;
    *(void **)&query = dlsym(RTLD_DEFAULT, "vaQueryVendorString");
    void *va = NULL;
    if (!query && (va = dlopen("libva.so.2", RTLD_NOW | RTLD_NOLOAD))) {
        *(void **)&query = dlsym(va, "vaQueryVendorString");
    }
    const char *vendor = display && query ? query(display) : NULL;
    snprintf(out, (size_t)len, "%s", vendor ? vendor : "");
    if (va) dlclose(va);
}

// An H.264 decoder for a stream described by `avcc` (the MP4's avcC box),
// on `threads` threads (0: as many as it likes); on the card instead where
// libavcodec can: `kind` HCAV_CUDA with `hw` a CUDA context (`cuda.c`),
// HCAV_VAAPI with `hw` a device from `hcav_vaapi_open`.
void *hcav_open(const uint8_t *avcc, int avcc_len, int threads, int kind, void *hw, char *err, int errlen) {
    if (!av.loaded) {
        snprintf(err, errlen, "libavcodec isn't loaded");
        return NULL;
    }
    const AVCodec *codec = av.find_decoder(AV_CODEC_ID_H264);
    if (!codec) {
        snprintf(err, errlen, "libavcodec has no H.264 decoder");
        return NULL;
    }
    Dec *d = av.mallocz(sizeof(Dec));
    if (!d) {
        snprintf(err, errlen, "out of memory");
        return NULL;
    }
    d->ctx = av.alloc_context3(codec);
    d->pkt = av.packet_alloc();
    d->frame = av.frame_alloc();
    d->down = av.frame_alloc();
    d->mapped = av.frame_alloc();
    if (!d->ctx || !d->pkt || !d->frame || !d->down || !d->mapped) {
        snprintf(err, errlen, "out of memory");
        hcav_close(d);
        return NULL;
    }
    d->ctx->extradata = av.mallocz((size_t)avcc_len + AV_INPUT_BUFFER_PADDING_SIZE);
    if (!d->ctx->extradata) {
        snprintf(err, errlen, "out of memory");
        hcav_close(d);
        return NULL;
    }
    memcpy(d->ctx->extradata, avcc, (size_t)avcc_len);
    d->ctx->extradata_size = avcc_len;
    d->ctx->thread_count = threads;
    d->ctx->thread_type = FF_THREAD_FRAME | FF_THREAD_SLICE;
    if (kind != HCAV_CPU && (!av.hw || (kind == HCAV_VAAPI && !av.vaapi))) {
        snprintf(err, errlen, "this libavcodec can't decode on the graphics card");
        hcav_close(d);
        return NULL;
    }
    if (kind == HCAV_CUDA) {
        // Our context: libavcodec decodes in it and doesn't destroy it.
        AVBufferRef *device = av.hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_CUDA);
        if (device) {
            AVCUDADeviceContext *cu = ((AVHWDeviceContext *)device->data)->hwctx;
            cu->cuda_ctx = hw;
            cu->stream = NULL;
        }
        if (!device || av.hwdevice_ctx_init(device) < 0) {
            av.buffer_unref(&device);
            snprintf(err, errlen, "libavcodec can't use CUDA");
            hcav_close(d);
            return NULL;
        }
        d->ctx->hw_device_ctx = av.buffer_ref(device);
        av.buffer_unref(&device);
        d->ctx->opaque = (void *)(intptr_t)AV_PIX_FMT_CUDA;
        d->ctx->get_format = pick_hw;
    } else if (kind == HCAV_VAAPI) {
        d->ctx->hw_device_ctx = av.buffer_ref(hw);
        d->ctx->opaque = (void *)(intptr_t)AV_PIX_FMT_VAAPI;
        d->ctx->get_format = pick_hw;
    }
    if (kind != HCAV_CPU && !d->ctx->hw_device_ctx) {
        snprintf(err, errlen, "out of memory");
        hcav_close(d);
        return NULL;
    }
    int r = av.open2(d->ctx, codec, NULL);
    if (r < 0) {
        char text[128];
        hcav_error_text(r, text, sizeof text);
        snprintf(err, errlen, "the H.264 decoder didn't open: %s", text);
        hcav_close(d);
        return NULL;
    }
    return d;
}

// Hand the decoder one frame's bytes (NULL: the end, for the last frames to
// come out): HCAV_SENT; HCAV_AGAIN when it has frames to take out first;
// HCAV_ERROR with the library's code (a damaged frame, say).
int hcav_send(void *p, const uint8_t *data, int size, int64_t pts, int key, int *code) {
    Dec *d = p;
    *code = 0;
    int r;
    if (!data) {
        r = av.send_packet(d->ctx, NULL);
    } else {
        size_t need = (size_t)size + AV_INPUT_BUFFER_PADDING_SIZE;
        if (need > d->cap) {
            uint8_t *bigger = av.mallocz(need * 2);
            if (!bigger) {
                *code = AVERROR(ENOMEM);
                return HCAV_ERROR;
            }
            av.free_(d->buf);
            d->buf = bigger;
            d->cap = need * 2;
        }
        memcpy(d->buf, data, (size_t)size);
        memset(d->buf + size, 0, AV_INPUT_BUFFER_PADDING_SIZE);
        d->pkt->data = d->buf;
        d->pkt->size = size;
        d->pkt->pts = pts;
        d->pkt->dts = AV_NOPTS_VALUE;
        d->pkt->flags = key ? AV_PKT_FLAG_KEY : 0;
        r = av.send_packet(d->ctx, d->pkt);
        d->pkt->data = NULL;
        d->pkt->size = 0;
    }
    if (r == AVERROR(EAGAIN)) {
        return HCAV_AGAIN;
    }
    if (r < 0) {
        *code = r;
        return HCAV_ERROR;
    }
    return HCAV_SENT;
}

// The next decoded frame, if one is ready: HCAV_FRAME with its time in
// `pts` (held until the next call: `hcav_take_nv12` copies it), HCAV_AGAIN
// when it needs more input, HCAV_END once everything sent has come out.
int hcav_receive(void *p, int64_t *pts, int *code) {
    Dec *d = p;
    *code = 0;
    if (d->holding) {
        av.frame_unref(d->mapped);
        av.frame_unref(d->frame);
        av.frame_unref(d->down);
        d->holding = 0;
    }
    int r = av.receive_frame(d->ctx, d->frame);
    if (r == AVERROR(EAGAIN)) {
        return HCAV_AGAIN;
    }
    if (r == AVERROR_EOF) {
        return HCAV_END;
    }
    if (r < 0) {
        *code = r;
        return HCAV_ERROR;
    }
    d->holding = 1;
    *pts = d->frame->pts != AV_NOPTS_VALUE ? d->frame->pts : d->frame->best_effort_timestamp;
    return HCAV_FRAME;
}

// Studio range from full: 0-255 to 16-235 (luma) or 16-240 (colour).
static inline uint8_t studio(uint8_t v, int luma) {
    return (uint8_t)(16 + (v * (luma ? 219 : 224) + 127) / 255);
}

// Where the frame just received is on the card: its luma (rows `y_pitch`
// bytes apart) and its U, V pairs. 0, or -1 if it isn't on the card as
// 8-bit studio-range NV12 (then `hcav_download` and `hcav_take_nv12`).
int hcav_frame_cuda(void *p, uint64_t *y, int *y_pitch, uint64_t *uv, int *uv_pitch) {
    Dec *d = p;
    AVFrame *f = d->frame;
    if (!d->holding || f->format != AV_PIX_FMT_CUDA || !f->hw_frames_ctx) {
        return -1;
    }
    AVHWFramesContext *frames = (AVHWFramesContext *)f->hw_frames_ctx->data;
    if (frames->sw_format != AV_PIX_FMT_NV12 || f->color_range == AVCOL_RANGE_JPEG) {
        return -1;
    }
    *y = (uint64_t)(uintptr_t)f->data[0];
    *y_pitch = f->linesize[0];
    *uv = (uint64_t)(uintptr_t)f->data[1];
    *uv_pitch = f->linesize[1];
    return 0;
}

#define FOURCC(a, b, c, d) ((uint32_t)(a) | ((uint32_t)(b) << 8) | ((uint32_t)(c) << 16) | ((uint32_t)(d) << 24))

// Where the frame just received is on the card, decoded through VA-API:
// its luma's plane (R8) and its U, V pairs' plane (two bytes a pair), each
// a DMA-BUF. The fds stay ours until `hcav_unmap` (or the next frame):
// duplicate one to keep it. 0, or -1 if it isn't on the card as 8-bit
// studio-range NV12 in those two planes (then `hcav_download` and
// `hcav_take_nv12`). Waits for the card to finish decoding it.
int hcav_frame_dmabuf(void *p, HcPlane *planes) {
    Dec *d = p;
    AVFrame *f = d->frame;
    if (!d->holding || f->format != AV_PIX_FMT_VAAPI || !f->hw_frames_ctx) {
        return -1;
    }
    AVHWFramesContext *frames = (AVHWFramesContext *)f->hw_frames_ctx->data;
    if (frames->sw_format != AV_PIX_FMT_NV12 || f->color_range == AVCOL_RANGE_JPEG) {
        return -1;
    }
    av.frame_unref(d->mapped);
    d->mapped->format = AV_PIX_FMT_DRM_PRIME;
    if (av.hwframe_map(d->mapped, f, AV_HWFRAME_MAP_READ) < 0) {
        av.frame_unref(d->mapped);
        return -1;
    }
    const AVDRMFrameDescriptor *desc = (const AVDRMFrameDescriptor *)d->mapped->data[0];
    // A layer a plane (R8, then GR88: VA-API's export), or both in one
    // layer (NV12: older drivers' derived images).
    const AVDRMPlaneDescriptor *at[2] = {NULL, NULL};
    uint32_t l0 = desc->nb_layers > 0 ? desc->layers[0].format : 0;
    uint32_t l1 = desc->nb_layers > 1 ? desc->layers[1].format : 0;
    if (desc->nb_layers == 2 && desc->layers[0].nb_planes == 1 && desc->layers[1].nb_planes == 1 && l0 == FOURCC('R', '8', ' ', ' ') &&
        (l1 == FOURCC('G', 'R', '8', '8') || l1 == FOURCC('R', 'G', '8', '8'))) {
        at[0] = &desc->layers[0].planes[0];
        at[1] = &desc->layers[1].planes[0];
    } else if (desc->nb_layers == 1 && desc->layers[0].nb_planes == 2 && l0 == FOURCC('N', 'V', '1', '2')) {
        at[0] = &desc->layers[0].planes[0];
        at[1] = &desc->layers[0].planes[1];
    } else {
        av.frame_unref(d->mapped);
        return -1;
    }
    for (int i = 0; i < 2; i++) {
        int o = at[i]->object_index;
        if (o < 0 || o >= desc->nb_objects || at[i]->offset < 0 || at[i]->pitch <= 0) {
            av.frame_unref(d->mapped);
            return -1;
        }
        planes[i].fd = desc->objects[o].fd;
        planes[i].size = desc->objects[o].size;
        planes[i].modifier = desc->objects[o].format_modifier;
        planes[i].offset = (uint64_t)at[i]->offset;
        planes[i].pitch = (uint64_t)at[i]->pitch;
    }
    return 0;
}

// Let go of the DMA-BUFs `hcav_frame_dmabuf` handed over.
void hcav_unmap(void *p) {
    Dec *d = p;
    av.frame_unref(d->mapped);
}

// The frame just received copied down from the card, for `hcav_take_nv12`
// (one that isn't on the card needs nothing). 0, else -1.
int hcav_download(void *p) {
    Dec *d = p;
    if (!d->holding) {
        return -1;
    }
    if (!d->frame->hw_frames_ctx) {
        return 0;
    }
    av.frame_unref(d->down);
    if (av.hwframe_transfer_data(d->down, d->frame, 0) < 0) {
        return -1;
    }
    d->down->color_range = d->frame->color_range;
    return 0;
}

// The frame just received (copied down, if it was on the card), as tight
// NV12 (`width`×`height`, studio range): the luma, then Cb, Cr pairs at half
// size. 0, or -1 if it isn't that size or 8-bit 4:2:0.
int hcav_take_nv12(void *p, uint8_t *out, int width, int height) {
    Dec *d = p;
    AVFrame *f = d->frame->hw_frames_ctx ? d->down : d->frame;
    if (!d->holding || f->width != width || f->height != height) {
        return -1;
    }
    int full = f->color_range == AVCOL_RANGE_JPEG || f->format == AV_PIX_FMT_YUVJ420P;
    int planar = f->format == AV_PIX_FMT_YUV420P || f->format == AV_PIX_FMT_YUVJ420P;
    if (!planar && f->format != AV_PIX_FMT_NV12) {
        return -1;
    }
    int cw = (width + 1) / 2, ch = (height + 1) / 2;
    uint8_t *y = out, *uv = out + (size_t)width * height;
    for (int r = 0; r < height; r++) {
        const uint8_t *src = f->data[0] + (size_t)r * f->linesize[0];
        uint8_t *dst = y + (size_t)r * width;
        if (full) {
            for (int x = 0; x < width; x++) dst[x] = studio(src[x], 1);
        } else {
            memcpy(dst, src, (size_t)width);
        }
    }
    for (int r = 0; r < ch; r++) {
        uint8_t *dst = uv + (size_t)r * cw * 2;
        if (planar) {
            const uint8_t *u = f->data[1] + (size_t)r * f->linesize[1];
            const uint8_t *v = f->data[2] + (size_t)r * f->linesize[2];
            if (full) {
                for (int x = 0; x < cw; x++) {
                    dst[2 * x] = studio(u[x], 0);
                    dst[2 * x + 1] = studio(v[x], 0);
                }
            } else {
                for (int x = 0; x < cw; x++) {
                    dst[2 * x] = u[x];
                    dst[2 * x + 1] = v[x];
                }
            }
        } else {
            const uint8_t *src = f->data[1] + (size_t)r * f->linesize[1];
            if (full) {
                for (int x = 0; x < cw * 2; x++) dst[x] = studio(src[x], 0);
            } else {
                memcpy(dst, src, (size_t)cw * 2);
            }
        }
    }
    return 0;
}

// Throw away everything queued and decoded, to start again at a keyframe.
void hcav_flush(void *p) {
    Dec *d = p;
    if (d->holding) {
        av.frame_unref(d->mapped);
        av.frame_unref(d->frame);
        av.frame_unref(d->down);
        d->holding = 0;
    }
    av.flush_buffers(d->ctx);
}

void hcav_close(void *p) {
    Dec *d = p;
    if (!d) {
        return;
    }
    if (d->frame) {
        av.frame_free(&d->frame);
    }
    if (d->down) {
        av.frame_free(&d->down);
    }
    if (d->mapped) {
        av.frame_free(&d->mapped);
    }
    if (d->pkt) {
        av.packet_free(&d->pkt);
    }
    if (d->ctx) {
        av.free_context(&d->ctx);
    }
    av.free_(d->buf);
    av.free_(d);
}

#endif
