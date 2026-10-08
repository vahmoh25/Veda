/*
 * Copyright © 2026 Vahid Mohammadi
 * SPDX-License-Identifier: MIT
 *
 * The renderer's insides: devices, contexts and the objects of a context,
 * and what the command decoder (commands.c) uses of them.
 */

#ifndef VEDA_RENDERER_INTERNAL_H
#define VEDA_RENDERER_INTERNAL_H

#include <stdarg.h>
#include <stdbool.h>

#include "renderer.h"

#include "cso_cache/cso_context.h"
#include "pipe/p_context.h"
#include "pipe/p_screen.h"
#include "pipe/p_state.h"
#include "virgl_hw.h"
#include "virgl_protocol.h"

struct vr_device {
   struct pipe_screen *screen;
   char name[64];
   /* virgl's 24-bit depth formats with depth in the upper 24 bits of each
    * texel (as OpenGL's GL_UNSIGNED_INT_24_8 packs it), which OpenGL hosts
    * take, are kept with depth in the lower 24 bits: the driver has only
    * those (iris). See vr_format. */
   bool depth_low;
   /* How it takes memory from outside (vr_resource_import). */
   enum vr_import import;
};

/* A winsys_handle's type for memory given by its address (softpipe's
 * display targets, device.c). */
#define VR_HANDLE_MEMORY 0x7600

/* A shader, whose text may come in several commands. */
struct vr_shader {
   mesa_shader_stage stage;
   /* The driver's shader, once the text is complete. */
   void *cso;
   /* The text so far: `have` of `len` bytes. */
   char *text;
   uint32_t len, have;
   uint32_t num_tokens;
   struct pipe_stream_output_info so;
};

struct vr_query {
   struct pipe_query *pq;
   unsigned type;
   /* Where its result goes: a resource with storage in the shared
    * memory, and the offset of a struct virgl_host_query_state in it. */
   uint32_t resource, offset;
   bool active;
   /* Its result is to be written once the fence of this sequence number
    * has signaled (0: nothing is waiting). */
   uint64_t due;
};

/* An object of a context (`enum virgl_object_type`). */
struct vr_object {
   uint32_t type;
   union {
      struct pipe_blend_state blend;
      struct pipe_rasterizer_state rasterizer;
      struct pipe_depth_stencil_alpha_state dsa;
      struct pipe_sampler_state sampler;
      struct cso_velems_state elements;
      struct vr_shader shader;
      struct pipe_sampler_view *view;
      /* With a reference to its texture. */
      struct pipe_surface surface;
      struct vr_query query;
      struct pipe_stream_output_target *target;
   } u;
};

/* A resource: the driver's, or (staging, query results) only a range of
 * the shared memory. */
struct vr_resource {
   bool used;
   struct pipe_resource *pres;
   uint64_t backing_offset, backing_len;
   /* As created (virgl's numbers). */
   uint32_t target, format, bind;
   uint32_t width, height, depth, array_size, last_level, nr_samples;
};

struct vr_fence {
   uint64_t seq;
   struct pipe_fence_handle *handle;
};

/* Object handles are looked up in chunks of this many. */
#define VR_CHUNK 1024u
/* Handles are below this. */
#define VR_MAX_HANDLES (1u << 20)
/* Resources a context may have at once. */
#define VR_MAX_RESOURCES (1u << 18)

struct vr_context {
   struct vr_device *dev;
   struct pipe_context *pipe;
   struct cso_context *cso;
   uint8_t *shared;
   size_t shared_len;
   char error[256];
   /* Set when the device fails: everything since is lost. */
   bool lost;

   struct vr_object **chunks[VR_MAX_HANDLES / VR_CHUNK];

   /* Resources by id (from 1), and the ids free for reuse. */
   struct vr_resource *res;
   uint32_t res_cap, res_next;
   uint32_t *res_free;
   unsigned num_res_free;

   /* What is bound, by handle (0: nothing). Sampler states are bound
    * through the cso context, which keeps copies. */
   uint32_t blend, dsa, rasterizer, elements, vs, fs;

   /* The framebuffer's attachments: color buffers (a bit each), and what
    * the depth-stencil one has (PIPE_CLEAR_DEPTH, PIPE_CLEAR_STENCIL). */
   uint32_t fb_colors, fb_zs;

   /* Vertex buffers: their resources (referenced), offsets and strides.
    * The strides belong to Gallium's vertex elements, so the elements are
    * set at the draw, when either changes. */
   struct pipe_vertex_buffer vbufs[PIPE_MAX_ATTRIBS];
   uint32_t strides[PIPE_MAX_ATTRIBS];
   unsigned num_vbufs;
   bool vertex_dirty;

   /* The index buffer (referenced). */
   struct pipe_resource *ib;
   unsigned ib_size, ib_offset;

   /* The user constants of each stage: the driver may keep using the
    * memory until they are replaced. */
   void *constants[MESA_SHADER_STAGES];

   /* Stream output targets (referenced), set at the draw, which knows the
    * primitive. */
   struct pipe_stream_output_target *so[PIPE_MAX_SO_BUFFERS];
   unsigned num_so;
   uint32_t so_append;
   bool so_dirty;
   enum mesa_prim so_prim;

   /* Fences not yet known to have signaled, oldest first. */
   struct vr_fence *fences;
   unsigned num_fences, fences_cap;
   uint64_t last_seq, signaled;

   /* Handles of queries whose results are to be written. */
   uint32_t *pending;
   unsigned num_pending, pending_cap;
};

/* context.c */
bool vr_fail(struct vr_context *ctx, const char *fmt, ...);
struct vr_object *vr_object(struct vr_context *ctx, uint32_t handle);
struct vr_object *vr_object_of(struct vr_context *ctx, uint32_t handle, uint32_t type);
bool vr_object_put(struct vr_context *ctx, uint32_t handle, struct vr_object *obj);
void vr_object_destroy(struct vr_context *ctx, uint32_t handle);
struct vr_resource *vr_resource(struct vr_context *ctx, uint32_t id);
uint8_t *vr_backing(struct vr_context *ctx, struct vr_resource *r, uint64_t offset, uint64_t len);
bool vr_query_wait_later(struct vr_context *ctx, uint32_t handle);
bool vr_query_write(struct vr_context *ctx, struct vr_query *q, bool wait);


/* formats.c */
enum pipe_format vr_format_from_virgl(uint32_t format);
enum pipe_format vr_format(const struct vr_device *dev, uint32_t format);
bool vr_format_rotated(const struct vr_device *dev, uint32_t format);
void vr_rotate_depth(uint8_t *mem, unsigned layers, uint64_t rows, uint64_t row, uint64_t stride,
                     uint64_t layer_stride, bool to_low);

#endif
