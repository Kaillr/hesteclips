// NVIDIA's video encoder (NVENC) for recording (`nvenc.rs`), driven
// directly rather than through an ffmpeg process: a frame goes in where the
// graphics card made it (Vulkan memory shared with CUDA) and only the
// encoded frame comes out, so no picture passes through the CPU. Several
// frames are in flight at once: one thread hands them in, another takes
// each one's bitstream when it's done, in the same order, as NVIDIA allows
// in synchronous mode (the only one there is on Linux).
//
// libcuda and libnvidia-encode come with NVIDIA's driver and are opened at
// run time, so the app starts without them. NVIDIA's API headers, as
// nv-codec-headers 11.1 packages them (ffnvcodec/), take drivers from 470.57
// on: GTX 10-series cards' last driver (580) too.

#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

typedef struct {
    char *buf;
    int len;
} Why;

// The loader's complaints ("Cannot load libcuda.so.1") go into the error
// text handed back, not to stderr.
#define FFNV_LOG_FUNC(logctx, msg, ...)                                     \
    do {                                                                     \
        Why *why_ = (Why *)(logctx);                                         \
        if (why_ && why_->len > 0) snprintf(why_->buf, why_->len, (msg), __VA_ARGS__); \
    } while (0)
#define FFNV_DEBUG_LOG_FUNC(logctx, msg, ...)
#include "ffnvcodec/dynlink_loader.h"

#ifndef CUDA_EXTERNAL_MEMORY_DEDICATED
#define CUDA_EXTERNAL_MEMORY_DEDICATED 0x1
#endif

// What `hcnv_open` returns in `code` besides a session.
#define HCNV_NO_DRIVER 1 // no NVIDIA driver here (its libraries aren't there)
#define HCNV_FAILED 2    // there is one, and it didn't work

#define MAX_SLOTS 64

// One input frame's memory and the bitstream it's encoded into.
typedef struct {
    CUdeviceptr ptr;
    // The Vulkan memory it's in, or NULL for CUDA's own (frames from the CPU).
    CUexternalMemory shared;
    uint32_t pitch;
    NV_ENC_REGISTERED_PTR registered;
    NV_ENC_INPUT_PTR mapped;
    NV_ENC_OUTPUT_PTR bitstream;
} Slot;

typedef struct {
    CudaFunctions *cu;
    NvencFunctions *nv;
    NV_ENCODE_API_FUNCTION_LIST api;
    CUcontext ctx;
    void *session;
    uint32_t width, height;
    Slot slots[MAX_SLOTS];
    int count;
} Enc;

static void say(char *err, int errlen, const char *fmt, ...) {
    if (!err || errlen <= 0) return;
    va_list args;
    va_start(args, fmt);
    vsnprintf(err, errlen, fmt, args);
    va_end(args);
}

static const char *status_name(NVENCSTATUS s) {
    switch (s) {
    case NV_ENC_ERR_NO_ENCODE_DEVICE: return "no encoder on this card";
    case NV_ENC_ERR_UNSUPPORTED_DEVICE: return "this card has no encoder that's supported";
    case NV_ENC_ERR_INVALID_ENCODERDEVICE: return "invalid device";
    case NV_ENC_ERR_DEVICE_NOT_EXIST: return "the card went away";
    case NV_ENC_ERR_INVALID_PTR: return "invalid pointer";
    case NV_ENC_ERR_INVALID_PARAM: return "invalid parameter";
    case NV_ENC_ERR_UNSUPPORTED_PARAM: return "unsupported parameter";
    case NV_ENC_ERR_OUT_OF_MEMORY: return "out of memory";
    case NV_ENC_ERR_INVALID_VERSION: return "the driver doesn't take this API version";
    case NV_ENC_ERR_ENCODER_BUSY: return "the encoder is busy";
    case NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY: return "the card allows no more encoding sessions";
    case NV_ENC_ERR_RESOURCE_REGISTER_FAILED: return "couldn't register the frame's memory";
    case NV_ENC_ERR_MAP_FAILED: return "couldn't map the frame's memory";
    case NV_ENC_ERR_GENERIC: return "error";
    default: return "error";
    }
}

static int nv_ok(Enc *e, NVENCSTATUS s, const char *what, char *err, int errlen) {
    if (s == NV_ENC_SUCCESS) return 1;
    const char *detail = e->session && e->api.nvEncGetLastErrorString ? e->api.nvEncGetLastErrorString(e->session) : NULL;
    if (detail && *detail)
        say(err, errlen, "%s: %s (%d): %s", what, status_name(s), (int)s, detail);
    else
        say(err, errlen, "%s: %s (%d)", what, status_name(s), (int)s);
    return 0;
}

static int cu_ok(Enc *e, CUresult r, const char *what, char *err, int errlen) {
    if (r == CUDA_SUCCESS) return 1;
    const char *name = NULL;
    if (e->cu && e->cu->cuGetErrorName) e->cu->cuGetErrorName(r, &name);
    say(err, errlen, "%s: %s", what, name ? name : "CUDA error");
    return 0;
}

static int push(Enc *e, char *err, int errlen) {
    return cu_ok(e, e->cu->cuCtxPushCurrent(e->ctx), "CUDA context", err, errlen);
}

static void pop(Enc *e) {
    CUcontext dummy;
    e->cu->cuCtxPopCurrent(&dummy);
}

void hcnv_close(void *p);

// An encoder for `width`×`height` NV12 at `fps`: H.264 High, no B-frames
// (each frame comes out as it goes in), VBR at `kbps` (at most `max_kbps`
// over a `buf_kbits` buffer), a keyframe every `gop` frames, NVIDIA's speed
// preset `preset` (1 fastest, 7 best) tuned for low latency. On the CUDA
// device whose UUID is `uuid` (the Vulkan device's), or the first if NULL.
void *hcnv_open(const uint8_t *uuid, int width, int height, int fps, int kbps, int max_kbps, int buf_kbits, int gop,
                int preset, int *code, char *err, int errlen) {
    *code = HCNV_FAILED;
    Enc *e = calloc(1, sizeof *e);
    if (!e) {
        say(err, errlen, "out of memory");
        return NULL;
    }
    e->width = width;
    e->height = height;
    Why why = {err, errlen};
    if (cuda_load_functions(&e->cu, &why) < 0 || nvenc_load_functions(&e->nv, &why) < 0) {
        *code = HCNV_NO_DRIVER;
        goto fail;
    }
    CudaFunctions *cu = e->cu;
    if (!cu_ok(e, cu->cuInit(0), "CUDA won't start", err, errlen)) goto fail;

    int count = 0;
    if (!cu_ok(e, cu->cuDeviceGetCount(&count), "CUDA's devices", err, errlen)) goto fail;
    CUdevice dev = -1;
    for (int i = 0; i < count && dev < 0; i++) {
        CUdevice d;
        if (cu->cuDeviceGet(&d, i) != CUDA_SUCCESS) continue;
        if (!uuid) {
            dev = d;
            break;
        }
        CUuuid id;
        CUresult r = cu->cuDeviceGetUuid_v2 ? cu->cuDeviceGetUuid_v2(&id, d)
                     : cu->cuDeviceGetUuid  ? cu->cuDeviceGetUuid(&id, d)
                                            : CUDA_ERROR_UNKNOWN;
        if (r == CUDA_SUCCESS && !memcmp(id.bytes, uuid, 16)) dev = d;
    }
    if (dev < 0) {
        say(err, errlen, "the graphics card isn't an NVIDIA one");
        *code = HCNV_NO_DRIVER;
        goto fail;
    }
    if (!cu_ok(e, cu->cuCtxCreate(&e->ctx, 0, dev), "CUDA context", err, errlen)) goto fail;
    pop(e);

    uint32_t max = 0;
    if (e->nv->NvEncodeAPIGetMaxSupportedVersion(&max) != NV_ENC_SUCCESS) {
        say(err, errlen, "the driver's video encoder doesn't say its version");
        goto fail;
    }
    uint32_t need = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;
    if (max < need) {
        say(err, errlen, "NVIDIA's driver is too old for its video encoder (it has NVENC %u.%u, 11.1 is needed: driver 470.57 or newer)",
            max >> 4, max & 0xf);
        goto fail;
    }
    e->api.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    if (!nv_ok(e, e->nv->NvEncodeAPICreateInstance(&e->api), "NVENC", err, errlen)) goto fail;

    if (!push(e, err, errlen)) goto fail;
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS open = {
        .version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
        .deviceType = NV_ENC_DEVICE_TYPE_CUDA,
        .device = e->ctx,
        .apiVersion = NVENCAPI_VERSION,
    };
    NVENCSTATUS s = e->api.nvEncOpenEncodeSessionEx(&open, &e->session);
    if (s != NV_ENC_SUCCESS) {
        e->session = NULL;
        nv_ok(e, s, "can't open the encoder", err, errlen);
        pop(e);
        goto fail;
    }

    static const GUID *presets[7] = {&NV_ENC_PRESET_P1_GUID, &NV_ENC_PRESET_P2_GUID, &NV_ENC_PRESET_P3_GUID,
                                     &NV_ENC_PRESET_P4_GUID, &NV_ENC_PRESET_P5_GUID, &NV_ENC_PRESET_P6_GUID,
                                     &NV_ENC_PRESET_P7_GUID};
    GUID preset_guid = *presets[(preset < 1 ? 1 : preset > 7 ? 7 : preset) - 1];
    NV_ENC_PRESET_CONFIG pc = {.version = NV_ENC_PRESET_CONFIG_VER, .presetCfg = {.version = NV_ENC_CONFIG_VER}};
    s = e->api.nvEncGetEncodePresetConfigEx(e->session, NV_ENC_CODEC_H264_GUID, preset_guid, NV_ENC_TUNING_INFO_LOW_LATENCY, &pc);
    if (!nv_ok(e, s, "the encoder's preset", err, errlen)) {
        pop(e);
        goto fail;
    }
    NV_ENC_CONFIG cfg = pc.presetCfg;
    cfg.version = NV_ENC_CONFIG_VER;
    cfg.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
    cfg.gopLength = gop;
    cfg.frameIntervalP = 1;
    NV_ENC_RC_PARAMS *rc = &cfg.rcParams;
    rc->version = NV_ENC_RC_PARAMS_VER;
    rc->rateControlMode = NV_ENC_PARAMS_RC_VBR;
    rc->averageBitRate = (uint32_t)kbps * 1000;
    rc->maxBitRate = (uint32_t)max_kbps * 1000;
    rc->vbvBufferSize = (uint32_t)buf_kbits * 1000;
    rc->vbvInitialDelay = rc->vbvBufferSize;
    rc->multiPass = NV_ENC_MULTI_PASS_DISABLED;
    rc->enableLookahead = 0;
    rc->zeroReorderDelay = 1;
    NV_ENC_CONFIG_H264 *h = &cfg.encodeCodecConfig.h264Config;
    h->idrPeriod = gop;
    h->repeatSPSPPS = 1;
    h->outputAUD = 0;
    h->sliceMode = 3;
    h->sliceModeData = 1;
    h->chromaFormatIDC = 1;
    h->level = NV_ENC_LEVEL_AUTOSELECT;
    // BT.709, studio range: what the frames are made in.
    NV_ENC_CONFIG_H264_VUI_PARAMETERS *vui = &h->h264VUIParameters;
    vui->videoSignalTypePresentFlag = 1;
    vui->videoFormat = 5;
    vui->videoFullRangeFlag = 0;
    vui->colourDescriptionPresentFlag = 1;
    vui->colourPrimaries = 1;
    vui->transferCharacteristics = 1;
    vui->colourMatrix = 1;

    NV_ENC_INITIALIZE_PARAMS init = {
        .version = NV_ENC_INITIALIZE_PARAMS_VER,
        .encodeGUID = NV_ENC_CODEC_H264_GUID,
        .presetGUID = preset_guid,
        .encodeWidth = width,
        .encodeHeight = height,
        .darWidth = width,
        .darHeight = height,
        .frameRateNum = fps,
        .frameRateDen = 1,
        .enableEncodeAsync = 0,
        .enablePTD = 1,
        .encodeConfig = &cfg,
        .maxEncodeWidth = width,
        .maxEncodeHeight = height,
        .tuningInfo = NV_ENC_TUNING_INFO_LOW_LATENCY,
    };
    s = e->api.nvEncInitializeEncoder(e->session, &init);
    pop(e);
    if (!nv_ok(e, s, "the encoder won't take these settings", err, errlen)) goto fail;
    *code = 0;
    return e;

fail:
    hcnv_close(e);
    return NULL;
}

// Register slot `s`'s memory with the encoder, and give it a bitstream.
static int finish_slot(Enc *e, Slot *s, char *err, int errlen) {
    NV_ENC_REGISTER_RESOURCE reg = {
        .version = NV_ENC_REGISTER_RESOURCE_VER,
        .resourceType = NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
        .width = e->width,
        .height = e->height,
        .pitch = s->pitch,
        .resourceToRegister = (void *)s->ptr,
        .bufferFormat = NV_ENC_BUFFER_FORMAT_NV12,
        .bufferUsage = NV_ENC_INPUT_IMAGE,
    };
    if (!nv_ok(e, e->api.nvEncRegisterResource(e->session, &reg), "registering a frame", err, errlen)) return 0;
    s->registered = reg.registeredResource;
    NV_ENC_CREATE_BITSTREAM_BUFFER bs = {.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER};
    if (!nv_ok(e, e->api.nvEncCreateBitstreamBuffer(e->session, &bs), "a bitstream buffer", err, errlen)) return 0;
    s->bitstream = bs.bitstreamBuffer;
    return 1;
}

// An input in Vulkan memory: `fd` (an opaque fd of a dedicated allocation
// of `alloc` bytes, taken over either way), the frame in its first `size`
// bytes at `pitch`. Its slot number, or -1.
int hcnv_add_shared(void *p, int fd, uint64_t alloc, uint64_t size, int pitch, char *err, int errlen) {
    Enc *e = p;
    if (e->count >= MAX_SLOTS) {
        say(err, errlen, "too many frames");
        close(fd);
        return -1;
    }
    if (!push(e, err, errlen)) {
        close(fd);
        return -1;
    }
    Slot *s = &e->slots[e->count];
    memset(s, 0, sizeof *s);
    s->pitch = pitch;
    CUDA_EXTERNAL_MEMORY_HANDLE_DESC h = {
        .type = CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD,
        .handle.fd = fd,
        .size = alloc,
        .flags = CUDA_EXTERNAL_MEMORY_DEDICATED,
    };
    int ok = cu_ok(e, e->cu->cuImportExternalMemory(&s->shared, &h), "CUDA won't take the frame's memory", err, errlen);
    if (!ok) {
        close(fd);
        s->shared = NULL;
    }
    if (ok) {
        CUDA_EXTERNAL_MEMORY_BUFFER_DESC b = {.offset = 0, .size = size};
        ok = cu_ok(e, e->cu->cuExternalMemoryGetMappedBuffer(&s->ptr, s->shared, &b), "mapping the frame's memory", err, errlen);
    }
    ok = ok && finish_slot(e, s, err, errlen);
    pop(e);
    // Counted even when it failed, so closing frees what was made.
    e->count++;
    return ok ? e->count - 1 : -1;
}

// An input in CUDA's own memory, for frames from the CPU (`hcnv_upload`).
int hcnv_add_own(void *p, int pitch, char *err, int errlen) {
    Enc *e = p;
    if (e->count >= MAX_SLOTS) {
        say(err, errlen, "too many frames");
        return -1;
    }
    if (!push(e, err, errlen)) return -1;
    Slot *s = &e->slots[e->count];
    memset(s, 0, sizeof *s);
    s->pitch = pitch;
    size_t bytes = (size_t)pitch * e->height * 3 / 2;
    int ok = cu_ok(e, e->cu->cuMemAlloc(&s->ptr, bytes), "CUDA memory for a frame", err, errlen);
    ok = ok && finish_slot(e, s, err, errlen);
    pop(e);
    e->count++;
    return ok ? e->count - 1 : -1;
}

// Copy an NV12 frame from the CPU (rows `width` bytes apart) into a slot of
// CUDA's own.
int hcnv_upload(void *p, int slot, const uint8_t *nv12, char *err, int errlen) {
    Enc *e = p;
    Slot *s = &e->slots[slot];
    if (!push(e, err, errlen)) return -1;
    CUDA_MEMCPY2D copy = {
        .srcMemoryType = CU_MEMORYTYPE_HOST,
        .srcHost = nv12,
        .srcPitch = e->width,
        .dstMemoryType = CU_MEMORYTYPE_DEVICE,
        .dstDevice = s->ptr,
        .dstPitch = s->pitch,
        .WidthInBytes = e->width,
        .Height = e->height * 3 / 2,
    };
    int ok = cu_ok(e, e->cu->cuMemcpy2D(&copy), "uploading a frame", err, errlen);
    pop(e);
    return ok ? 0 : -1;
}

// Encode the frame in `slot` as frame `pts`. Its bitstream is taken with
// `hcnv_lock`, in the order the frames were encoded.
int hcnv_encode(void *p, int slot, int64_t pts, char *err, int errlen) {
    Enc *e = p;
    Slot *s = &e->slots[slot];
    if (!push(e, err, errlen)) return -1;
    NV_ENC_MAP_INPUT_RESOURCE map = {.version = NV_ENC_MAP_INPUT_RESOURCE_VER, .registeredResource = s->registered};
    if (!nv_ok(e, e->api.nvEncMapInputResource(e->session, &map), "mapping a frame", err, errlen)) {
        pop(e);
        return -1;
    }
    s->mapped = map.mappedResource;
    NV_ENC_PIC_PARAMS pic = {
        .version = NV_ENC_PIC_PARAMS_VER,
        .inputWidth = e->width,
        .inputHeight = e->height,
        .inputPitch = s->pitch,
        .inputBuffer = s->mapped,
        .outputBitstream = s->bitstream,
        .bufferFmt = map.mappedBufferFmt,
        .pictureStruct = NV_ENC_PIC_STRUCT_FRAME,
        .inputTimeStamp = (uint64_t)pts,
    };
    NVENCSTATUS st = e->api.nvEncEncodePicture(e->session, &pic);
    if (st != NV_ENC_SUCCESS) {
        // Held back for later frames (NEED_MORE_INPUT) can't happen without
        // B-frames or lookahead: anything but done is a failure.
        nv_ok(e, st, "encoding a frame", err, errlen);
        e->api.nvEncUnmapInputResource(e->session, s->mapped);
        s->mapped = NULL;
        pop(e);
        return -1;
    }
    pop(e);
    return 0;
}

// Wait for `slot`'s frame to be encoded: its bitstream (Annex B) and whether
// it's a keyframe, readable until `hcnv_unlock`.
int hcnv_lock(void *p, int slot, const uint8_t **data, uint32_t *len, int *key, char *err, int errlen) {
    Enc *e = p;
    Slot *s = &e->slots[slot];
    if (!push(e, err, errlen)) return -1;
    NV_ENC_LOCK_BITSTREAM lock = {.version = NV_ENC_LOCK_BITSTREAM_VER, .outputBitstream = s->bitstream};
    int ok = nv_ok(e, e->api.nvEncLockBitstream(e->session, &lock), "taking an encoded frame", err, errlen);
    pop(e);
    if (!ok) return -1;
    *data = lock.bitstreamBufferPtr;
    *len = lock.bitstreamSizeInBytes;
    *key = lock.pictureType == NV_ENC_PIC_TYPE_IDR;
    return 0;
}

// Done with `slot`'s bitstream (locked or not): the slot can take a new frame.
void hcnv_unlock(void *p, int slot, int locked) {
    Enc *e = p;
    Slot *s = &e->slots[slot];
    if (e->cu->cuCtxPushCurrent(e->ctx) != CUDA_SUCCESS) return;
    if (locked) e->api.nvEncUnlockBitstream(e->session, s->bitstream);
    if (s->mapped) e->api.nvEncUnmapInputResource(e->session, s->mapped);
    s->mapped = NULL;
    pop(e);
}

// No more frames.
void hcnv_end(void *p) {
    Enc *e = p;
    if (e->cu->cuCtxPushCurrent(e->ctx) != CUDA_SUCCESS) return;
    NV_ENC_PIC_PARAMS pic = {.version = NV_ENC_PIC_PARAMS_VER, .encodePicFlags = NV_ENC_PIC_FLAG_EOS};
    e->api.nvEncEncodePicture(e->session, &pic);
    pop(e);
}

// Close the encoder and free its slots' memory (the Vulkan memory stays its
// owner's: only CUDA's hold on it goes). Every frame must have been taken.
void hcnv_close(void *p) {
    Enc *e = p;
    if (!e) return;
    if (e->ctx && e->cu->cuCtxPushCurrent(e->ctx) == CUDA_SUCCESS) {
        for (int i = 0; i < e->count; i++) {
            Slot *s = &e->slots[i];
            if (e->session) {
                if (s->mapped) e->api.nvEncUnmapInputResource(e->session, s->mapped);
                if (s->registered) e->api.nvEncUnregisterResource(e->session, s->registered);
                if (s->bitstream) e->api.nvEncDestroyBitstreamBuffer(e->session, s->bitstream);
            }
            if (s->ptr) e->cu->cuMemFree(s->ptr);
            if (s->shared) e->cu->cuDestroyExternalMemory(s->shared);
        }
        if (e->session) e->api.nvEncDestroyEncoder(e->session);
        pop(e);
    }
    if (e->ctx) e->cu->cuCtxDestroy(e->ctx);
    nvenc_free_functions(&e->nv);
    cuda_free_functions(&e->cu);
    free(e);
}
