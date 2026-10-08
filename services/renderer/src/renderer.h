/*
 * Copyright © 2026 Vahid Mohammadi
 * SPDX-License-Identifier: MIT
 *
 * Veda's renderer: carries out the virgl command streams of Veda's OpenGL
 * ES library (vgl) on a Gallium driver, as virglrenderer carries them out
 * on a host's OpenGL for QEMU's virtio-gpu. vgl speaks the same protocol to
 * both, so applications render on the GPU of a real PC as they do under
 * QEMU.
 *
 * A device is a Gallium screen: softpipe (for tests), or the driver of the
 * PC's GPU. A context is one client's: its objects, its resources and its
 * fences, and the memory it shares with the renderer (its staging area and
 * query results). Everything a client sends is checked before it reaches
 * the driver: a client can fail its own context, never the renderer.
 */

#ifndef VEDA_RENDERER_H
#define VEDA_RENDERER_H

#include <stddef.h>
#include <stdint.h>

#if defined(_WIN32) && defined(VR_EXPORTS)
#define VR_API __declspec(dllexport)
#else
#define VR_API
#endif

#ifdef __cplusplus
extern "C" {
#endif

struct vr_device;
struct vr_context;

/* A resource as virgl creates it (virgl_renderer_resource_create_args,
 * without the handle). */
struct vr_resource_args {
   uint32_t target, format, bind, width, height, depth;
   uint32_t array_size, last_level, nr_samples, flags;
};

enum vr_result {
   VR_OK = 0,
   /* A malformed or impossible request: the client is at fault. */
   VR_INVALID = -1,
   VR_NO_MEMORY = -2,
   /* The device failed. */
   VR_LOST = -3,
   /* A wait ran out of time. */
   VR_TIMEOUT = -4,
};

/* A device on Gallium's software rasterizer. */
VR_API struct vr_device *vr_device_create_softpipe(void);
/* A device on an Intel GPU (iris), through the DRM device `fd`. */
VR_API struct vr_device *vr_device_create_iris(int fd);
VR_API void vr_device_destroy(struct vr_device *dev);
/* The renderer's name, for the client ("softpipe", "Intel ..."). */
VR_API const char *vr_device_name(struct vr_device *dev);
/* Fills `out` with the device's capability set 2 (struct virgl_caps_v2);
 * returns its size, or 0 if `len` is too small for it. */
VR_API size_t vr_device_caps(struct vr_device *dev, void *out, size_t len);

/* A context. Its resources may be backed by ranges of `shared` (`len`
 * bytes the client also has mapped). */
VR_API struct vr_context *vr_context_create(struct vr_device *dev, uint8_t *shared, size_t len);
VR_API void vr_context_destroy(struct vr_context *ctx);
/* Why the context's last call failed, for the log. */
VR_API const char *vr_context_error(const struct vr_context *ctx);

/* Creates a resource; `backing_len` bytes of the shared memory from
 * `backing_offset` back it (0: none). Its handle goes to `*id`. */
VR_API int vr_resource_create(struct vr_context *ctx, const struct vr_resource_args *args,
                              uint64_t backing_offset, uint64_t backing_len, uint32_t *id);
VR_API void vr_resource_destroy(struct vr_context *ctx, uint32_t id);

/* How a device takes memory from outside to render into
 * (vr_resource_import): none, a dma-buf's file descriptor (a GPU's), or
 * the memory's address in this process (softpipe). */
enum vr_import {
   VR_IMPORT_NONE = 0,
   VR_IMPORT_FD = 1,
   VR_IMPORT_MEMORY = 2,
};
VR_API enum vr_import vr_device_import(struct vr_device *dev);

/* Creates a 2D render target (`args`: one level, single sampled, a format
 * of 32-bit pixels) whose storage is memory from outside, rows `stride`
 * bytes apart: a display's picture, which the device renders into in
 * place. It is the dma-buf `fd` (VR_IMPORT_FD: the device takes what it
 * needs of it, and the caller closes it) or `memory` (VR_IMPORT_MEMORY:
 * mapped by the caller for as long as the context lives). Once a fence
 * after the drawing has signaled, the memory holds what was drawn. Its
 * handle goes to `*id`. */
VR_API int vr_resource_import(struct vr_context *ctx, const struct vr_resource_args *args, int fd, void *memory,
                              uint32_t stride, uint32_t *id);

/* Carries out `count` words of commands. When it returns, what transfers
 * read from the shared memory has been read and what they write there has
 * been written (the GPU may still be working on the rest). */
VR_API int vr_submit(struct vr_context *ctx, const uint32_t *words, size_t count);

/* Queues a fence after everything submitted so far; its number goes to
 * `*seq` (fences signal in order). */
VR_API int vr_fence(struct vr_context *ctx, uint64_t *seq);
/* The number of the last fence that has signaled. Writes the results of
 * the queries that were due by then into the shared memory first. */
VR_API uint64_t vr_fence_signaled(struct vr_context *ctx);
/* Waits up to `timeout_ns` (UINT64_MAX: for ever) for fence `seq`. */
VR_API int vr_fence_wait(struct vr_context *ctx, uint64_t seq, uint64_t timeout_ns);

#ifdef __cplusplus
}
#endif

#endif
