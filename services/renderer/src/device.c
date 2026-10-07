/*
 * Copyright © 2026 Vahid Mohammadi
 * SPDX-License-Identifier: MIT
 *
 * Devices: a Gallium screen, and the capabilities it gives clients
 * (virgl's capability set 2, which vgl reads as it reads virglrenderer's).
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "internal.h"

#include "util/format/u_format.h"
#include "util/u_math.h"

#ifdef VR_SOFTPIPE
#include "frontend/sw_winsys.h"
#include "softpipe/sp_public.h"
#include "sw/null/null_sw_winsys.h"
#endif

#ifdef VR_IRIS
#include "iris/drm/iris_drm_public.h"
#include "util/driconf.h"
#include "util/xmlconfig.h"

/* iris's options, at their defaults (there are no configuration files). */
static const driOptionDescription iris_driconf[] = {
#include "iris/driinfo_iris.h"
};
#endif

static struct vr_device *
device_create(struct pipe_screen *screen)
{
   if (!screen)
      return NULL;
   struct vr_device *dev = calloc(1, sizeof(*dev));
   if (!dev) {
      screen->destroy(screen);
      return NULL;
   }
   dev->screen = screen;
   snprintf(dev->name, sizeof(dev->name), "%s", screen->get_name(screen));
   /* Depth in the upper 24 bits, as OpenGL hosts keep it, or only in the
    * lower (iris). VR_DEPTH_LOW=1 keeps it in the lower bits whatever the
    * driver has: the host's tests take iris's way on softpipe. */
   unsigned zs = PIPE_BIND_DEPTH_STENCIL | PIPE_BIND_SAMPLER_VIEW;
   bool low = screen->is_format_supported(screen, PIPE_FORMAT_Z24_UNORM_S8_UINT, PIPE_TEXTURE_2D, 0, 0, zs);
   bool high = screen->is_format_supported(screen, PIPE_FORMAT_S8_UINT_Z24_UNORM, PIPE_TEXTURE_2D, 0, 0, zs);
   const char *force = getenv("VR_DEPTH_LOW");
   dev->depth_low = low && (!high || (force && strcmp(force, "1") == 0));
   return dev;
}

#ifdef VR_IRIS
VR_API struct vr_device *
vr_device_create_iris(int fd)
{
   /* iris fills the cache in from the descriptions, and takes what it needs
    * of it when the screen is made. */
   driOptionCache info, options;
   memset(&options, 0, sizeof(options));
   driParseOptionInfo(&info, iris_driconf, ARRAY_SIZE(iris_driconf));
   struct pipe_screen_config config = {.options = &options, .options_info = &info};
   struct pipe_screen *screen = iris_drm_screen_create(fd, &config);
   driDestroyOptionCache(&options);
   driDestroyOptionInfo(&info);
   return device_create(screen);
}
#endif

#ifdef VR_SOFTPIPE
VR_API struct vr_device *
vr_device_create_softpipe(void)
{
   struct sw_winsys *ws = null_sw_create();
   if (!ws)
      return NULL;
   struct pipe_screen *screen = softpipe_create_screen(ws);
   if (!screen) {
      ws->destroy(ws);
      return NULL;
   }
   return device_create(screen);
}
#endif

VR_API void
vr_device_destroy(struct vr_device *dev)
{
   if (!dev)
      return;
   dev->screen->destroy(dev->screen);
   free(dev);
}

VR_API const char *
vr_device_name(struct vr_device *dev)
{
   return dev->name;
}

static void
set_bit(struct virgl_supported_format_mask *mask, uint32_t format)
{
   mask->bitmask[format / 32] |= 1u << (format % 32);
}

/* The most samples color images can have (0: none). */
static unsigned
max_samples(struct pipe_screen *s)
{
   for (unsigned n = 16; n >= 2; n /= 2) {
      if (s->is_format_supported(s, PIPE_FORMAT_R8G8B8A8_UNORM, PIPE_TEXTURE_2D, n, n,
                                 PIPE_BIND_RENDER_TARGET | PIPE_BIND_SAMPLER_VIEW) &&
          s->is_format_supported(s, PIPE_FORMAT_Z24_UNORM_S8_UINT, PIPE_TEXTURE_2D, n, n,
                                 PIPE_BIND_DEPTH_STENCIL))
         return n;
   }
   return 0;
}

static void
fill_formats(const struct vr_device *dev, union virgl_caps *caps, unsigned samples)
{
   struct pipe_screen *s = dev->screen;
   struct virgl_caps_v1 *v1 = &caps->v1;
   struct virgl_caps_v2 *v2 = &caps->v2;
   for (uint32_t f = 1; f < 32 * ARRAY_SIZE(v1->sampler.bitmask); f++) {
      enum pipe_format pf = vr_format(dev, f);
      if (pf == PIPE_FORMAT_NONE)
         continue;
      bool zs = util_format_is_depth_or_stencil(pf);
      bool sample = s->is_format_supported(s, pf, PIPE_TEXTURE_2D, 0, 0, PIPE_BIND_SAMPLER_VIEW);
      bool render = !zs && s->is_format_supported(s, pf, PIPE_TEXTURE_2D, 0, 0, PIPE_BIND_RENDER_TARGET);
      bool depth = zs && s->is_format_supported(s, pf, PIPE_TEXTURE_2D, 0, 0, PIPE_BIND_DEPTH_STENCIL);
      if (sample)
         set_bit(&v1->sampler, f);
      if (render)
         set_bit(&v1->render, f);
      if (depth)
         set_bit(&v1->depthstencil, f);
      if (s->is_format_supported(s, pf, PIPE_BUFFER, 0, 0, PIPE_BIND_VERTEX_BUFFER))
         set_bit(&v1->vertexbuffer, f);
      /* Whatever can be in an image can be read back (texture_map). */
      if (sample || render || depth)
         set_bit(&v2->supported_readback_formats, f);
      unsigned bind = zs ? PIPE_BIND_DEPTH_STENCIL : PIPE_BIND_RENDER_TARGET;
      if (samples && s->is_format_supported(s, pf, PIPE_TEXTURE_2D, samples, samples, bind))
         set_bit(&v2->supported_multisample_formats, f);
   }
}

VR_API size_t
vr_device_caps(struct vr_device *dev, void *out, size_t len)
{
   if (len < sizeof(struct virgl_caps_v2))
      return 0;
   struct pipe_screen *s = dev->screen;
   const struct pipe_caps *c = &s->caps;
   const struct pipe_shader_caps *vs = &s->shader_caps[MESA_SHADER_VERTEX];
   const struct pipe_shader_caps *fs = &s->shader_caps[MESA_SHADER_FRAGMENT];
   union virgl_caps *caps = calloc(1, sizeof(*caps));
   if (!caps)
      return 0;
   struct virgl_caps_v1 *v1 = &caps->v1;
   struct virgl_caps_v2 *v2 = &caps->v2;
   unsigned samples = max_samples(s);

   v1->max_version = 2;
   fill_formats(dev, caps, samples);
   v1->bset.indep_blend_enable = c->indep_blend_enable;
   v1->bset.indep_blend_func = c->indep_blend_func;
   v1->bset.cube_map_array = c->cube_map_array;
   v1->bset.conditional_render = c->conditional_render;
   v1->bset.start_instance = c->start_instance;
   v1->bset.primitive_restart = c->primitive_restart;
   v1->bset.blend_eq_sep = c->blend_equation_separate;
   v1->bset.instanceid = 1;
   v1->bset.vertex_element_instance_divisor = c->vertex_element_instance_divisor;
   v1->bset.seamless_cube_map = c->seamless_cube_map;
   v1->bset.occlusion_query = c->occlusion_query;
   v1->bset.texture_multisample = c->texture_multisample;
   v1->bset.ubo = 1;
   v1->glsl_level = c->glsl_feature_level;
   v1->max_texture_array_layers = c->max_texture_array_layers;
   v1->max_streamout_buffers = c->max_stream_output_buffers;
   v1->max_dual_source_render_targets = c->max_dual_source_render_targets;
   v1->max_render_targets = MIN2(c->max_render_targets, VIRGL_MAX_COLOR_BUFS);
   v1->max_samples = samples;
   v1->prim_mask = c->supported_prim_modes;
   v1->max_tbo_size = c->max_texel_buffer_elements;
   /* Slot 0 of each stage holds its plain uniforms. */
   v1->max_uniform_blocks = MIN2(vs->max_const_buffers, fs->max_const_buffers) - 1;
   v1->max_viewports = c->max_viewports;
   v1->max_texture_gather_components = c->max_texture_gather_components;

   v2->min_aliased_point_size = c->min_point_size;
   v2->max_aliased_point_size = c->max_point_size;
   v2->min_smooth_point_size = c->min_point_size_aa;
   v2->max_smooth_point_size = c->max_point_size_aa;
   v2->min_aliased_line_width = c->min_line_width;
   v2->max_aliased_line_width = c->max_line_width;
   v2->min_smooth_line_width = c->min_line_width_aa;
   v2->max_smooth_line_width = c->max_line_width_aa;
   v2->max_texture_lod_bias = c->max_texture_lod_bias;
   v2->max_geom_output_vertices = c->max_geometry_output_vertices;
   v2->max_geom_total_output_components = c->max_geometry_total_output_components;
   v2->max_vertex_outputs = vs->max_outputs;
   v2->max_vertex_attribs = MIN2(vs->max_inputs, PIPE_MAX_ATTRIBS);
   v2->max_shader_patch_varyings = c->max_shader_patch_varyings;
   v2->min_texel_offset = c->min_texel_offset;
   v2->max_texel_offset = c->max_texel_offset;
   v2->min_texture_gather_offset = c->min_texture_gather_offset;
   v2->max_texture_gather_offset = c->max_texture_gather_offset;
   v2->texture_buffer_offset_alignment = c->texture_buffer_offset_alignment;
   v2->uniform_buffer_offset_alignment = c->constant_buffer_offset_alignment;
   v2->shader_buffer_offset_alignment = c->shader_buffer_offset_alignment;
   /* Transfers go through the command stream, in both directions; images
    * have views and are copied as they are. */
   v2->capability_bits = VIRGL_CAP_COPY_TRANSFER | VIRGL_CAP_TEXTURE_VIEW | VIRGL_CAP_COPY_IMAGE;
   v2->capability_bits_v2 = VIRGL_CAP_V2_COPY_TRANSFER_BOTH_DIRECTIONS;
   if (c->texture_shadow_lod)
      v2->capability_bits_v2 |= VIRGL_CAP_V2_TEXTURE_SHADOW_LOD;
   v2->max_vertex_attrib_stride = c->max_vertex_attrib_stride;
   v2->max_texture_2d_size = c->max_texture_2d_size;
   v2->max_texture_3d_size = 1u << (MAX2(c->max_texture_3d_levels, 1) - 1);
   v2->max_texture_cube_size = 1u << (MAX2(c->max_texture_cube_levels, 1) - 1);
   v2->host_feature_check_version = 0;
   snprintf(v2->renderer, sizeof(v2->renderer), "%s", dev->name);
   v2->max_anisotropy = c->max_texture_anisotropy;
   v2->max_texture_samplers = fs->max_texture_samplers;
   for (unsigned i = 0; i < ARRAY_SIZE(v2->max_const_buffer_size); i++) {
      /* virgl numbers stages as Gallium once did. */
      static const mesa_shader_stage stages[] = {
         MESA_SHADER_VERTEX, MESA_SHADER_FRAGMENT, MESA_SHADER_GEOMETRY,
         MESA_SHADER_TESS_CTRL, MESA_SHADER_TESS_EVAL, MESA_SHADER_COMPUTE,
      };
      v2->max_const_buffer_size[i] = s->shader_caps[stages[i]].max_const_buffer0_size;
   }
   v2->max_uniform_block_size = c->max_constant_buffer_size;

   memcpy(out, caps, sizeof(struct virgl_caps_v2));
   free(caps);
   return sizeof(struct virgl_caps_v2);
}
