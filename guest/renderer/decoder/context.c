/*
 * Copyright © 2026 Vahid Mohammadi
 * SPDX-License-Identifier: MIT
 *
 * Contexts: their objects and resources, fences, and the query results
 * written into the memory shared with the client.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "internal.h"

#include "frontend/winsys_handle.h"
#include "util/format/u_format.h"
#include "util/u_inlines.h"
#include "util/u_math.h"

bool
vr_fail(struct vr_context *ctx, const char *fmt, ...)
{
   va_list ap;
   va_start(ap, fmt);
   vsnprintf(ctx->error, sizeof(ctx->error), fmt, ap);
   va_end(ap);
   return false;
}

VR_API const char *
vr_context_error(const struct vr_context *ctx)
{
   return ctx->error;
}

/* ---- Objects ------------------------------------------------------------ */

struct vr_object *
vr_object(struct vr_context *ctx, uint32_t handle)
{
   if (handle == 0 || handle >= VR_MAX_HANDLES)
      return NULL;
   struct vr_object **chunk = ctx->chunks[handle / VR_CHUNK];
   return chunk ? chunk[handle % VR_CHUNK] : NULL;
}

struct vr_object *
vr_object_of(struct vr_context *ctx, uint32_t handle, uint32_t type)
{
   struct vr_object *o = vr_object(ctx, handle);
   return o && o->type == type ? o : NULL;
}

/* Frees an object, unbinding it first wherever it is bound. */
static void
object_free(struct vr_context *ctx, uint32_t handle, struct vr_object *o)
{
   struct pipe_context *pipe = ctx->pipe;
   switch (o->type) {
   case VIRGL_OBJECT_BLEND:
      if (ctx->blend == handle)
         ctx->blend = 0;
      break;
   case VIRGL_OBJECT_DSA:
      if (ctx->dsa == handle)
         ctx->dsa = 0;
      break;
   case VIRGL_OBJECT_RASTERIZER:
      if (ctx->rasterizer == handle)
         ctx->rasterizer = 0;
      break;
   case VIRGL_OBJECT_VERTEX_ELEMENTS:
      if (ctx->elements == handle) {
         ctx->elements = 0;
         ctx->vertex_dirty = true;
      }
      break;
   case VIRGL_OBJECT_SHADER: {
      struct vr_shader *s = &o->u.shader;
      if (s->cso) {
         if (s->stage == MESA_SHADER_VERTEX) {
            if (ctx->vs == handle) {
               cso_set_vertex_shader_handle(ctx->cso, NULL);
               ctx->vs = 0;
            }
            pipe->delete_vs_state(pipe, s->cso);
         } else {
            if (ctx->fs == handle) {
               cso_set_fragment_shader_handle(ctx->cso, NULL);
               ctx->fs = 0;
            }
            pipe->delete_fs_state(pipe, s->cso);
         }
      }
      free(s->text);
      break;
   }
   case VIRGL_OBJECT_SAMPLER_STATE:
      /* Bound ones stay bound: the cso context has copies. */
      break;
   case VIRGL_OBJECT_SAMPLER_VIEW:
      /* The driver holds references to the views it has bound. */
      pipe_sampler_view_release(o->u.view);
      break;
   case VIRGL_OBJECT_SURFACE:
      pipe_resource_reference(&o->u.surface.texture, NULL);
      break;
   case VIRGL_OBJECT_QUERY:
      if (o->u.query.active)
         pipe->end_query(pipe, o->u.query.pq);
      pipe->destroy_query(pipe, o->u.query.pq);
      break;
   case VIRGL_OBJECT_STREAMOUT_TARGET:
      pipe_so_target_reference(&o->u.target, NULL);
      break;
   }
   free(o);
}

bool
vr_object_put(struct vr_context *ctx, uint32_t handle, struct vr_object *obj)
{
   if (handle == 0 || handle >= VR_MAX_HANDLES) {
      object_free(ctx, 0, obj);
      return vr_fail(ctx, "object handle %u is out of range", handle);
   }
   struct vr_object ***chunk = &ctx->chunks[handle / VR_CHUNK];
   if (!*chunk) {
      *chunk = calloc(VR_CHUNK, sizeof(**chunk));
      if (!*chunk) {
         object_free(ctx, 0, obj);
         return vr_fail(ctx, "out of memory");
      }
   }
   /* A handle in use names the new object from now on. */
   struct vr_object *old = (*chunk)[handle % VR_CHUNK];
   if (old)
      object_free(ctx, handle, old);
   (*chunk)[handle % VR_CHUNK] = obj;
   return true;
}

void
vr_object_destroy(struct vr_context *ctx, uint32_t handle)
{
   struct vr_object *o = vr_object(ctx, handle);
   if (!o)
      return;
   ctx->chunks[handle / VR_CHUNK][handle % VR_CHUNK] = NULL;
   object_free(ctx, handle, o);
}

/* ---- Resources ---------------------------------------------------------- */

struct vr_resource *
vr_resource(struct vr_context *ctx, uint32_t id)
{
   if (id == 0 || id >= ctx->res_next || !ctx->res[id].used)
      return NULL;
   return &ctx->res[id];
}

uint8_t *
vr_backing(struct vr_context *ctx, struct vr_resource *r, uint64_t offset, uint64_t len)
{
   if (!r || offset > r->backing_len || len > r->backing_len - offset)
      return NULL;
   return ctx->shared + r->backing_offset + offset;
}

static unsigned
pipe_bind(uint32_t virgl)
{
   unsigned bind = 0;
   if (virgl & VIRGL_BIND_DEPTH_STENCIL)
      bind |= PIPE_BIND_DEPTH_STENCIL;
   if (virgl & VIRGL_BIND_RENDER_TARGET)
      bind |= PIPE_BIND_RENDER_TARGET;
   if (virgl & VIRGL_BIND_SAMPLER_VIEW)
      bind |= PIPE_BIND_SAMPLER_VIEW;
   return bind;
}

/* Whether the driver can make the resource `t` describes, as far as its
 * limits go. */
static bool
check_texture(struct vr_context *ctx, const struct pipe_resource *t)
{
   struct pipe_screen *s = ctx->dev->screen;
   const struct pipe_caps *c = &s->caps;
   unsigned max = c->max_texture_2d_size;
   unsigned layers = 1;
   switch (t->target) {
   case PIPE_TEXTURE_2D:
      break;
   case PIPE_TEXTURE_3D:
      max = 1u << (MAX2(c->max_texture_3d_levels, 1) - 1);
      break;
   case PIPE_TEXTURE_CUBE:
      max = 1u << (MAX2(c->max_texture_cube_levels, 1) - 1);
      layers = 6;
      if (t->width0 != t->height0)
         return vr_fail(ctx, "a cube map %ux%u", t->width0, t->height0);
      break;
   case PIPE_TEXTURE_2D_ARRAY:
      layers = t->array_size;
      if (layers > c->max_texture_array_layers)
         return vr_fail(ctx, "%u layers", layers);
      break;
   default:
      return vr_fail(ctx, "texture target %u", t->target);
   }
   if (t->target != PIPE_TEXTURE_2D_ARRAY && t->array_size != layers)
      return vr_fail(ctx, "%u layers for target %u", t->array_size, t->target);
   if (t->target != PIPE_TEXTURE_3D && t->depth0 != 1)
      return vr_fail(ctx, "depth %u for target %u", t->depth0, t->target);
   if (!t->width0 || !t->height0 || !t->depth0 || !t->array_size ||
       t->width0 > max || t->height0 > max || t->depth0 > max)
      return vr_fail(ctx, "a %ux%ux%u texture", t->width0, t->height0, t->depth0);
   unsigned biggest = MAX3(t->width0, t->height0, t->target == PIPE_TEXTURE_3D ? t->depth0 : 1);
   if (t->last_level > util_logbase2(biggest))
      return vr_fail(ctx, "%u levels of a %u texture", t->last_level + 1, biggest);
   if (t->nr_samples > 1 && (t->target != PIPE_TEXTURE_2D || t->last_level))
      return vr_fail(ctx, "%u samples for target %u", t->nr_samples, t->target);
   if (!s->is_format_supported(s, t->format, t->target, t->nr_samples, t->nr_storage_samples, t->bind))
      return vr_fail(ctx, "format %s with %u samples and binding 0x%x", util_format_name(t->format),
                     t->nr_samples, t->bind);
   return true;
}

/* A new resource id (0 if there are too many). */
static uint32_t
new_resource_id(struct vr_context *ctx)
{
   if (ctx->num_res_free)
      return ctx->res_free[--ctx->num_res_free];
   if (ctx->res_next == 0)
      ctx->res_next = 1;
   if (ctx->res_next >= VR_MAX_RESOURCES)
      return 0;
   if (ctx->res_next >= ctx->res_cap) {
      uint32_t cap = MAX2(ctx->res_cap * 2, 256);
      struct vr_resource *res = realloc(ctx->res, cap * sizeof(*res));
      uint32_t *free_ids = realloc(ctx->res_free, cap * sizeof(*free_ids));
      if (res)
         ctx->res = res;
      if (free_ids)
         ctx->res_free = free_ids;
      if (!res || !free_ids)
         return 0;
      memset(ctx->res + ctx->res_cap, 0, (cap - ctx->res_cap) * sizeof(*res));
      ctx->res_cap = cap;
   }
   return ctx->res_next++;
}

/* Gives `pres` (the reference) an id, as virgl's `a` describes it. */
static int
add_resource(struct vr_context *ctx, struct pipe_resource *pres, const struct vr_resource_args *a,
             uint64_t backing_offset, uint64_t backing_len, uint32_t *id)
{
   uint32_t n = new_resource_id(ctx);
   if (!n) {
      pipe_resource_reference(&pres, NULL);
      vr_fail(ctx, "too many resources");
      return VR_NO_MEMORY;
   }
   struct vr_resource *r = &ctx->res[n];
   memset(r, 0, sizeof(*r));
   r->used = true;
   r->pres = pres;
   r->backing_offset = backing_offset;
   r->backing_len = backing_len;
   r->target = a->target;
   r->format = a->format;
   r->bind = a->bind;
   r->width = a->width;
   r->height = a->height;
   r->depth = a->depth;
   r->array_size = a->array_size;
   r->last_level = a->last_level;
   r->nr_samples = a->nr_samples;
   *id = n;
   return VR_OK;
}

VR_API int
vr_resource_create(struct vr_context *ctx, const struct vr_resource_args *a, uint64_t backing_offset,
                   uint64_t backing_len, uint32_t *id)
{
   *id = 0;
   if (backing_offset > ctx->shared_len || backing_len > ctx->shared_len - backing_offset) {
      vr_fail(ctx, "storage at %llu, %llu bytes, outside the shared memory", (unsigned long long)backing_offset,
              (unsigned long long)backing_len);
      return VR_INVALID;
   }
   struct pipe_resource *pres = NULL;
   if (a->bind & (VIRGL_BIND_STAGING | VIRGL_BIND_CUSTOM)) {
      /* Memory only: staging, or where query results go. */
      if (a->target != PIPE_BUFFER || !backing_len) {
         vr_fail(ctx, "a staging resource without storage");
         return VR_INVALID;
      }
   } else {
      struct pipe_resource t = {0};
      t.target = a->target;
      t.usage = PIPE_USAGE_DEFAULT;
      if (a->target == PIPE_BUFFER) {
         if (!a->width) {
            vr_fail(ctx, "an empty buffer");
            return VR_INVALID;
         }
         /* A buffer may be bound anywhere later, whatever it was made for
          * (see vr_device). */
         t.format = PIPE_FORMAT_R8_UNORM;
         t.bind = ctx->dev->buffer_bind;
         t.width0 = a->width;
         t.height0 = t.depth0 = t.array_size = 1;
      } else {
         t.format = vr_format(ctx->dev, a->format);
         if (t.format == PIPE_FORMAT_NONE) {
            vr_fail(ctx, "format %u", a->format);
            return VR_INVALID;
         }
         if (a->height > 0xFFFF || a->depth > 0xFFFF || a->array_size > 0xFFFF || a->last_level > 15 ||
             a->nr_samples > 16) {
            vr_fail(ctx, "a %ux%ux%u texture of %u layers", a->width, a->height, a->depth, a->array_size);
            return VR_INVALID;
         }
         t.bind = pipe_bind(a->bind);
         t.width0 = a->width;
         t.height0 = a->height;
         t.depth0 = a->depth;
         t.array_size = a->array_size;
         t.last_level = a->last_level;
         t.nr_samples = t.nr_storage_samples = a->nr_samples > 1 ? a->nr_samples : 0;
         if (!check_texture(ctx, &t))
            return VR_INVALID;
      }
      pres = ctx->dev->screen->resource_create(ctx->dev->screen, &t);
      if (!pres) {
         vr_fail(ctx, "the driver could not make a resource");
         return VR_NO_MEMORY;
      }
   }
   return add_resource(ctx, pres, a, backing_offset, backing_len, id);
}

VR_API int
vr_resource_import(struct vr_context *ctx, const struct vr_resource_args *a, int fd, void *memory, uint32_t stride,
                   uint32_t *id)
{
   *id = 0;
   struct pipe_screen *s = ctx->dev->screen;
   enum pipe_format format = vr_format(ctx->dev, a->format);
   if (a->target != PIPE_TEXTURE_2D || a->depth != 1 || a->array_size != 1 || a->last_level || a->nr_samples > 1 ||
       !a->width || !a->height || a->width > 16384 || a->height > 16384) {
      vr_fail(ctx, "memory from outside as a %ux%u texture of target %u", a->width, a->height, a->target);
      return VR_INVALID;
   }
   if (format == PIPE_FORMAT_NONE || util_format_get_blocksize(format) != 4 || stride / 4 < a->width) {
      vr_fail(ctx, "memory from outside in format %u, rows %u bytes apart", a->format, stride);
      return VR_INVALID;
   }
   struct pipe_resource t = {0};
   t.target = PIPE_TEXTURE_2D;
   t.format = format;
   t.width0 = a->width;
   t.height0 = a->height;
   t.depth0 = t.array_size = 1;
   t.usage = PIPE_USAGE_DEFAULT;
   /* A picture a display shows: drawn into, and read from in copies. */
   t.bind = PIPE_BIND_RENDER_TARGET | PIPE_BIND_SAMPLER_VIEW | PIPE_BIND_SCANOUT | PIPE_BIND_SHARED;
   if (!s->is_format_supported(s, format, PIPE_TEXTURE_2D, 0, 0, PIPE_BIND_RENDER_TARGET)) {
      vr_fail(ctx, "format %s cannot be drawn into", util_format_name(format));
      return VR_INVALID;
   }
   struct winsys_handle h;
   memset(&h, 0, sizeof(h));
   h.stride = stride;
   h.format = format;
   /* DRM_FORMAT_MOD_LINEAR: rows one after the other, as displays scan
    * them out. */
   h.modifier = 0;
   switch (ctx->dev->import) {
#ifdef VR_IRIS
   case VR_IMPORT_FD:
      if (fd < 0)
         return VR_INVALID;
      h.type = WINSYS_HANDLE_TYPE_FD;
      h.handle = fd;
      break;
#endif
   case VR_IMPORT_MEMORY:
      if (!memory)
         return VR_INVALID;
      h.type = VR_HANDLE_MEMORY;
      h.com_obj = memory;
      break;
   default:
      vr_fail(ctx, "the device cannot render into memory it is given");
      return VR_INVALID;
   }
   struct pipe_resource *pres = s->resource_from_handle(s, &t, &h, PIPE_HANDLE_USAGE_FRAMEBUFFER_WRITE);
   if (!pres) {
      vr_fail(ctx, "the driver could not render into the memory");
      return VR_NO_MEMORY;
   }
   return add_resource(ctx, pres, a, 0, 0, id);
}

VR_API void
vr_resource_destroy(struct vr_context *ctx, uint32_t id)
{
   struct vr_resource *r = vr_resource(ctx, id);
   if (!r)
      return;
   /* Whatever still uses it (bound state, views, surfaces) has its own
    * reference. */
   pipe_resource_reference(&r->pres, NULL);
   r->used = false;
   ctx->res_free[ctx->num_res_free++] = id;
}

/* ---- Queries ------------------------------------------------------------ */

static bool
is_predicate(unsigned type)
{
   return type == PIPE_QUERY_OCCLUSION_PREDICATE || type == PIPE_QUERY_OCCLUSION_PREDICATE_CONSERVATIVE ||
          type == PIPE_QUERY_SO_OVERFLOW_PREDICATE || type == PIPE_QUERY_SO_OVERFLOW_ANY_PREDICATE ||
          type == PIPE_QUERY_GPU_FINISHED;
}

bool
vr_query_write(struct vr_context *ctx, struct vr_query *q, bool wait)
{
   union pipe_query_result result;
   memset(&result, 0, sizeof(result));
   if (!ctx->pipe->get_query_result(ctx->pipe, q->pq, wait, &result)) {
      if (!wait)
         return false;
      /* The device failed: say that samples passed, the safe answer. */
      result.u64 = 1;
      result.b = true;
   }
   uint64_t value = is_predicate(q->type) ? result.b : result.u64;
   uint8_t *at = vr_backing(ctx, vr_resource(ctx, q->resource), q->offset, sizeof(struct virgl_host_query_state));
   q->due = 0;
   if (!at)
      return true;
   struct virgl_host_query_state *st = (struct virgl_host_query_state *)at;
   memcpy(&st->result, &value, sizeof(value));
   st->result_size = sizeof(value);
   /* The client may read it at any time: the result before the state. */
   __atomic_store_n(&st->query_state, VIRGL_QUERY_STATE_DONE, __ATOMIC_RELEASE);
   return true;
}

bool
vr_query_wait_later(struct vr_context *ctx, uint32_t handle)
{
   struct vr_object *o = vr_object_of(ctx, handle, VIRGL_OBJECT_QUERY);
   if (!o)
      return false;
   bool listed = o->u.query.due != 0;
   o->u.query.due = ctx->last_seq + 1;
   if (listed)
      return true;
   if (ctx->num_pending == ctx->pending_cap) {
      unsigned cap = MAX2(ctx->pending_cap * 2, 16);
      uint32_t *p = realloc(ctx->pending, cap * sizeof(*p));
      if (!p)
         return vr_fail(ctx, "out of memory");
      ctx->pending = p;
      ctx->pending_cap = cap;
   }
   ctx->pending[ctx->num_pending++] = handle;
   return true;
}

/* Writes the results that the fences signaled so far make due. */
static void
write_due_results(struct vr_context *ctx)
{
   unsigned kept = 0;
   for (unsigned i = 0; i < ctx->num_pending; i++) {
      uint32_t h = ctx->pending[i];
      struct vr_object *o = vr_object_of(ctx, h, VIRGL_OBJECT_QUERY);
      if (!o || !o->u.query.due)
         continue;
      if (o->u.query.due <= ctx->signaled)
         vr_query_write(ctx, &o->u.query, true);
      else
         ctx->pending[kept++] = h;
   }
   ctx->num_pending = kept;
}

/* ---- Fences ------------------------------------------------------------- */

VR_API int
vr_fence(struct vr_context *ctx, uint64_t *seq)
{
   if (ctx->num_fences == ctx->fences_cap) {
      unsigned cap = MAX2(ctx->fences_cap * 2, 16);
      struct vr_fence *f = realloc(ctx->fences, cap * sizeof(*f));
      if (!f) {
         vr_fail(ctx, "out of memory");
         return VR_NO_MEMORY;
      }
      ctx->fences = f;
      ctx->fences_cap = cap;
   }
   struct pipe_fence_handle *handle = NULL;
   if (!ctx->lost)
      ctx->pipe->flush(ctx->pipe, &handle, 0);
   ctx->fences[ctx->num_fences++] = (struct vr_fence){++ctx->last_seq, handle};
   *seq = ctx->last_seq;
   return ctx->lost ? VR_LOST : VR_OK;
}

/* Forgets the fences that have signaled, oldest first. */
static void
retire(struct vr_context *ctx)
{
   struct pipe_screen *s = ctx->dev->screen;
   unsigned n = 0;
   while (n < ctx->num_fences) {
      struct vr_fence *f = &ctx->fences[n];
      if (f->handle && !s->fence_finish(s, ctx->pipe, f->handle, 0))
         break;
      s->fence_reference(s, &f->handle, NULL);
      ctx->signaled = f->seq;
      n++;
   }
   if (n) {
      ctx->num_fences -= n;
      memmove(ctx->fences, ctx->fences + n, ctx->num_fences * sizeof(*ctx->fences));
      write_due_results(ctx);
   }
}

VR_API uint64_t
vr_fence_signaled(struct vr_context *ctx)
{
   retire(ctx);
   return ctx->signaled;
}

VR_API int
vr_fence_wait(struct vr_context *ctx, uint64_t seq, uint64_t timeout_ns)
{
   if (seq > ctx->last_seq) {
      vr_fail(ctx, "fence %llu was never queued", (unsigned long long)seq);
      return VR_INVALID;
   }
   retire(ctx);
   if (seq <= ctx->signaled)
      return VR_OK;
   /* Fences without a handle (queued after the device failed) signal with
    * the fence before them. */
   struct pipe_screen *s = ctx->dev->screen;
   unsigned i = seq - ctx->fences[0].seq;
   while (i > 0 && !ctx->fences[i].handle)
      i--;
   struct vr_fence *f = &ctx->fences[i];
   if (f->handle && !s->fence_finish(s, ctx->pipe, f->handle, timeout_ns))
      return VR_TIMEOUT;
   retire(ctx);
   return VR_OK;
}

/* ---- Contexts ----------------------------------------------------------- */

VR_API struct vr_context *
vr_context_create(struct vr_device *dev, uint8_t *shared, size_t len)
{
   struct vr_context *ctx = calloc(1, sizeof(*ctx));
   if (!ctx)
      return NULL;
   ctx->dev = dev;
   ctx->shared = shared;
   ctx->shared_len = len;
   ctx->pipe = dev->screen->context_create(dev->screen, NULL, 0);
   if (!ctx->pipe) {
      free(ctx);
      return NULL;
   }
   /* Vertices only ever come from buffers, of 32-bit components at most:
    * the cso context translates only what the driver cannot fetch. */
   ctx->cso = cso_create_context(ctx->pipe, CSO_NO_USER_VERTEX_BUFFERS | CSO_NO_64B_VERTEX_BUFFERS);
   if (!ctx->cso) {
      ctx->pipe->destroy(ctx->pipe);
      free(ctx);
      return NULL;
   }
   return ctx;
}

/* Unbinds what the decoder binds on the driver itself (the cso context
 * unbinds its own when it goes): drivers are not all ready to be destroyed
 * with views bound (softpipe frees their textures' caches first, and then
 * the views' textures through them). */
static void
unbind(struct vr_context *ctx)
{
   struct pipe_screen *s = ctx->dev->screen;
   struct pipe_sampler_view *none[PIPE_MAX_SHADER_SAMPLER_VIEWS] = {NULL};
   for (unsigned st = 0; st <= MESA_SHADER_FRAGMENT; st++) {
      unsigned views = MIN2(s->shader_caps[st].max_sampler_views, PIPE_MAX_SHADER_SAMPLER_VIEWS);
      if (views)
         ctx->pipe->set_sampler_views(ctx->pipe, st, 0, views, 0, none);
      unsigned buffers = MIN2(s->shader_caps[st].max_const_buffers, PIPE_MAX_CONSTANT_BUFFERS);
      for (unsigned i = 0; i < buffers; i++)
         ctx->pipe->set_constant_buffer(ctx->pipe, st, i, NULL);
   }
}

VR_API void
vr_context_destroy(struct vr_context *ctx)
{
   if (!ctx)
      return;
   struct pipe_screen *s = ctx->dev->screen;
   unbind(ctx);
   for (unsigned c = 0; c < ARRAY_SIZE(ctx->chunks); c++) {
      if (!ctx->chunks[c])
         continue;
      for (unsigned i = 0; i < VR_CHUNK; i++) {
         if (ctx->chunks[c][i])
            vr_object_destroy(ctx, c * VR_CHUNK + i);
      }
      free(ctx->chunks[c]);
   }
   cso_destroy_context(ctx->cso);
   for (unsigned i = 0; i < ARRAY_SIZE(ctx->vbufs); i++)
      pipe_vertex_buffer_unreference(&ctx->vbufs[i]);
   pipe_resource_reference(&ctx->ib, NULL);
   for (unsigned i = 0; i < ARRAY_SIZE(ctx->so); i++)
      pipe_so_target_reference(&ctx->so[i], NULL);
   for (uint32_t id = 1; id < ctx->res_next; id++)
      pipe_resource_reference(&ctx->res[id].pres, NULL);
   for (unsigned i = 0; i < ctx->num_fences; i++)
      s->fence_reference(s, &ctx->fences[i].handle, NULL);
   ctx->pipe->destroy(ctx->pipe);
   for (unsigned i = 0; i < ARRAY_SIZE(ctx->constants); i++)
      free(ctx->constants[i]);
   free(ctx->res);
   free(ctx->res_free);
   free(ctx->fences);
   free(ctx->pending);
   free(ctx);
}
