// CUDA for decoding on NVIDIA's card (`decode.rs`, `avdec.c`): one context
// on the card the app draws with (found by its Vulkan UUID), the app's
// frame buffers (Vulkan memory, as opaque fds) taken in, and each decoded
// frame copied into one of them on the card. libavcodec decodes through
// NVIDIA's decoder (NVDEC) in this context.
//
// libcuda comes with NVIDIA's driver and is opened at run time, through
// nv-codec-headers' loader (ffnvcodec/), as for the encoder (`nvenc.c`).

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

// The loader's complaints go into the error text handed back.
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

typedef struct {
    CudaFunctions *cu;
    CUcontext ctx;
} Card;

static void say(char *err, int errlen, const char *fmt, ...) {
    if (!err || errlen <= 0) return;
    va_list args;
    va_start(args, fmt);
    vsnprintf(err, errlen, fmt, args);
    va_end(args);
}

static int ok(Card *c, CUresult r, const char *what, char *err, int errlen) {
    if (r == CUDA_SUCCESS) return 1;
    const char *name = NULL;
    if (c->cu && c->cu->cuGetErrorName) c->cu->cuGetErrorName(r, &name);
    say(err, errlen, "%s: %s", what, name ? name : "CUDA error");
    return 0;
}

void hccu_close(void *p);

// A context on the CUDA device whose UUID is `uuid` (the Vulkan device's).
// NULL, with why in `err`, without NVIDIA's driver or that card.
void *hccu_open(const uint8_t *uuid, char *err, int errlen) {
    Card *c = calloc(1, sizeof *c);
    if (!c) {
        say(err, errlen, "out of memory");
        return NULL;
    }
    Why why = {err, errlen};
    if (cuda_load_functions(&c->cu, &why) < 0) goto fail;
    CudaFunctions *cu = c->cu;
    if (!ok(c, cu->cuInit(0), "CUDA won't start", err, errlen)) goto fail;
    int count = 0;
    if (!ok(c, cu->cuDeviceGetCount(&count), "CUDA's devices", err, errlen)) goto fail;
    CUdevice dev = -1;
    for (int i = 0; i < count && dev < 0; i++) {
        CUdevice d;
        CUuuid id;
        if (cu->cuDeviceGet(&d, i) != CUDA_SUCCESS) continue;
        CUresult r = cu->cuDeviceGetUuid_v2 ? cu->cuDeviceGetUuid_v2(&id, d)
                     : cu->cuDeviceGetUuid  ? cu->cuDeviceGetUuid(&id, d)
                                            : CUDA_ERROR_UNKNOWN;
        if (r == CUDA_SUCCESS && !memcmp(id.bytes, uuid, 16)) dev = d;
    }
    if (dev < 0) {
        say(err, errlen, "the graphics card isn't an NVIDIA one");
        goto fail;
    }
    if (!ok(c, cu->cuCtxCreate(&c->ctx, 0, dev), "CUDA context", err, errlen)) goto fail;
    CUcontext dummy;
    cu->cuCtxPopCurrent(&dummy);
    return c;

fail:
    hccu_close(c);
    return NULL;
}

// The context, for libavcodec's decoder to work in.
void *hccu_context(void *p) {
    return ((Card *)p)->ctx;
}

// Take in Vulkan memory: `fd` (an opaque fd of a dedicated allocation of
// `alloc` bytes, taken over either way), its first `size` bytes mapped for
// copying into. 0 with the mapping and the memory's handle set, else -1.
int hccu_import(void *p, int fd, uint64_t alloc, uint64_t size, uint64_t *ptr, void **mem, char *err, int errlen) {
    Card *c = p;
    CudaFunctions *cu = c->cu;
    if (!ok(c, cu->cuCtxPushCurrent(c->ctx), "CUDA context", err, errlen)) {
        close(fd);
        return -1;
    }
    CUexternalMemory shared = NULL;
    CUDA_EXTERNAL_MEMORY_HANDLE_DESC h = {
        .type = CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD,
        .handle.fd = fd,
        .size = alloc,
        .flags = CUDA_EXTERNAL_MEMORY_DEDICATED,
    };
    int done = ok(c, cu->cuImportExternalMemory(&shared, &h), "CUDA won't take the frame's memory", err, errlen);
    if (!done) close(fd);
    CUdeviceptr at = 0;
    if (done) {
        CUDA_EXTERNAL_MEMORY_BUFFER_DESC b = {.offset = 0, .size = size};
        done = ok(c, cu->cuExternalMemoryGetMappedBuffer(&at, shared, &b), "mapping the frame's memory", err, errlen);
        if (!done) cu->cuDestroyExternalMemory(shared);
    }
    CUcontext dummy;
    cu->cuCtxPopCurrent(&dummy);
    if (!done) return -1;
    *ptr = at;
    *mem = shared;
    return 0;
}

// Let go of what `hccu_import` took in (the Vulkan memory stays its owner's).
void hccu_release(void *p, uint64_t ptr, void *mem) {
    Card *c = p;
    if (c->cu->cuCtxPushCurrent(c->ctx) != CUDA_SUCCESS) return;
    if (ptr) c->cu->cuMemFree((CUdeviceptr)ptr);
    if (mem) c->cu->cuDestroyExternalMemory((CUexternalMemory)mem);
    CUcontext dummy;
    c->cu->cuCtxPopCurrent(&dummy);
}

// Copy an NV12 frame on the card (luma at `y`, rows `y_pitch` bytes apart;
// the U, V pairs at `uv`, `uv_pitch`) to `dst`: the luma's `height` rows
// `dst_pitch` apart, then the pairs' rows. Done when this returns (after
// the decoder's own work on the frame, in the same stream). 0, else -1.
int hccu_copy_nv12(void *p, uint64_t y, int y_pitch, uint64_t uv, int uv_pitch, uint64_t dst, int dst_pitch, int width, int height,
                   char *err, int errlen) {
    Card *c = p;
    CudaFunctions *cu = c->cu;
    if (!ok(c, cu->cuCtxPushCurrent(c->ctx), "CUDA context", err, errlen)) return -1;
    CUDA_MEMCPY2D luma = {
        .srcMemoryType = CU_MEMORYTYPE_DEVICE,
        .srcDevice = (CUdeviceptr)y,
        .srcPitch = (size_t)y_pitch,
        .dstMemoryType = CU_MEMORYTYPE_DEVICE,
        .dstDevice = (CUdeviceptr)dst,
        .dstPitch = (size_t)dst_pitch,
        .WidthInBytes = (size_t)width,
        .Height = (size_t)height,
    };
    CUDA_MEMCPY2D chroma = luma;
    chroma.srcDevice = (CUdeviceptr)uv;
    chroma.srcPitch = (size_t)uv_pitch;
    chroma.dstDevice = (CUdeviceptr)dst + (size_t)dst_pitch * (size_t)height;
    chroma.WidthInBytes = (size_t)((width + 1) / 2 * 2);
    chroma.Height = (size_t)((height + 1) / 2);
    int done = ok(c, cu->cuMemcpy2DAsync(&luma, 0), "copying a frame", err, errlen) &&
               ok(c, cu->cuMemcpy2DAsync(&chroma, 0), "copying a frame", err, errlen) &&
               ok(c, cu->cuStreamSynchronize(0), "copying a frame", err, errlen);
    CUcontext dummy;
    cu->cuCtxPopCurrent(&dummy);
    return done ? 0 : -1;
}

void hccu_close(void *p) {
    Card *c = p;
    if (!c) return;
    if (c->ctx) c->cu->cuCtxDestroy(c->ctx);
    cuda_free_functions(&c->cu);
    free(c);
}
