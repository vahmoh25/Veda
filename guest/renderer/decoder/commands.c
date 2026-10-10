/*
 * Copyright © 2026 Vahid Mohammadi
 * SPDX-License-Identifier: MIT
 *
 * The command decoder: virgl's commands (virgl_protocol.h) as Gallium
 * calls. Every word is checked first: handles name objects of the right
 * type, boxes lie inside their resources, enumerations are in range. A
 * command that is not right fails the submission there, and nothing of it
 * reaches the driver.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "internal.h"

#include "tgsi/tgsi_dump.h"
#include "tgsi/tgsi_exec.h"
#include "tgsi/tgsi_parse.h"
#include "tgsi/tgsi_text.h"
#include "util/format/u_format.h"
#include "util/u_inlines.h"
#include "util/u_math.h"
#include "util/u_prim.h"

/* The most a shader's text and tokens may take. */
#define MAX_SHADER_TEXT (16u << 20)
#define MAX_SHADER_TOKENS (4u << 20)

/* A command: the words after its header, which virgl_protocol.h numbers
 * from 1. */
struct cmd {
   const uint32_t *w;
   uint32_t len;
};

static inline uint32_t
arg(const struct cmd *c, unsigned i)
{
   return c->w[i - 1];
}

static inline float
argf(const struct cmd *c, unsigned i)
{
   return uif(c->w[i - 1]);
}

static bool
need(struct vr_context *ctx, const struct cmd *c, uint32_t words, const char *what)
{
   return c->len >= words || vr_fail(ctx, "%s in %u words, not %u", what, c->len, words);
}

/* Whether to log each command and shader (VR_TRACE set), to find what a
 * client sent last. */
static bool
tracing(void)
{
   static int on = -1;
   if (on < 0)
      on = getenv("VR_TRACE") != NULL;
   return on;
}

static struct vr_object *
new_object(struct vr_context *ctx, uint32_t type)
{
   struct vr_object *o = calloc(1, sizeof(*o));
   if (!o) {
      vr_fail(ctx, "out of memory");
      return NULL;
   }
   o->type = type;
   return o;
}

static bool
stage_of(struct vr_context *ctx, uint32_t virgl, mesa_shader_stage *stage)
{
   switch (virgl) {
   case VIRGL_SHADER_VERTEX:
      *stage = MESA_SHADER_VERTEX;
      return true;
   case VIRGL_SHADER_FRAGMENT:
      *stage = MESA_SHADER_FRAGMENT;
      return true;
   default:
      *stage = MESA_SHADER_VERTEX;
      return vr_fail(ctx, "shader stage %u", virgl);
   }
}

static struct pipe_resource *
buffer_of(struct vr_context *ctx, uint32_t id)
{
   struct vr_resource *r = vr_resource(ctx, id);
   if (!r || !r->pres || r->pres->target != PIPE_BUFFER) {
      vr_fail(ctx, "resource %u is not a buffer", id);
      return NULL;
   }
   return r->pres;
}

static struct pipe_resource *
texture_of(struct vr_context *ctx, uint32_t id)
{
   struct vr_resource *r = vr_resource(ctx, id);
   if (!r || !r->pres || r->pres->target == PIPE_BUFFER) {
      vr_fail(ctx, "resource %u is not a texture", id);
      return NULL;
   }
   return r->pres;
}

/* The layers (or, of a 3D texture, slices) of a level. */
static unsigned
level_layers(const struct pipe_resource *t, unsigned level)
{
   return t->target == PIPE_TEXTURE_3D ? u_minify(t->depth0, level) : t->array_size;
}

/* A box from words, if it fits one: sizes may be negative where `signed_size`. */
static bool
make_box(struct vr_context *ctx, uint32_t x, uint32_t y, uint32_t z, uint32_t w, uint32_t h, uint32_t d,
         bool signed_size, struct pipe_box *box)
{
   int32_t sw = (int32_t)w, sh = (int32_t)h, sd = (int32_t)d;
   bool sizes_ok = signed_size ? sd >= INT16_MIN && sd <= INT16_MAX : w <= INT32_MAX && h <= INT32_MAX && d <= INT16_MAX;
   if (x > INT32_MAX || y > INT32_MAX || z > INT16_MAX || !sizes_ok)
      return vr_fail(ctx, "a box at (%u, %u, %u), %d x %d x %d", x, y, z, sw, sh, sd);
   box->x = x;
   box->y = y;
   box->z = z;
   box->width = sw;
   box->height = sh;
   box->depth = sd;
   return true;
}

/* Whether a box (of positive sizes) lies inside a level of a resource. */
static bool
box_inside(const struct pipe_resource *t, unsigned level, const struct pipe_box *b)
{
   if (level > t->last_level || b->width < 0 || b->height < 0 || b->depth < 0)
      return false;
   int64_t w = u_minify(t->width0, level), h = 1, d = 1;
   if (t->target != PIPE_BUFFER) {
      h = u_minify(t->height0, level);
      d = level_layers(t, level);
   }
   return b->x >= 0 && b->y >= 0 && b->z >= 0 && (int64_t)b->x + b->width <= w && (int64_t)b->y + b->height <= h &&
          (int64_t)b->z + b->depth <= d;
}

static bool
boxes_overlap(const struct pipe_box *a, const struct pipe_box *b)
{
   return a->x < b->x + b->width && b->x < a->x + a->width && a->y < b->y + b->height && b->y < a->y + a->height &&
          a->z < b->z + b->depth && b->z < a->z + a->depth;
}

/* Whether a format may stand for a resource's in a view, a surface or a
 * blit: one of the same block size and kind. */
static bool
compatible(enum pipe_format view, enum pipe_format resource)
{
   if (view == resource)
      return true;
   return util_format_get_blocksize(view) == util_format_get_blocksize(resource) &&
          util_format_get_blockwidth(view) == util_format_get_blockwidth(resource) &&
          util_format_get_blockheight(view) == util_format_get_blockheight(resource) &&
          util_format_is_depth_or_stencil(view) == util_format_is_depth_or_stencil(resource);
}

/* ---- State objects ------------------------------------------------------ */

static bool
blend_factor_ok(struct vr_context *ctx, unsigned f)
{
   switch (f) {
   case PIPE_BLENDFACTOR_ONE:
   case PIPE_BLENDFACTOR_SRC_COLOR:
   case PIPE_BLENDFACTOR_SRC_ALPHA:
   case PIPE_BLENDFACTOR_DST_ALPHA:
   case PIPE_BLENDFACTOR_DST_COLOR:
   case PIPE_BLENDFACTOR_SRC_ALPHA_SATURATE:
   case PIPE_BLENDFACTOR_CONST_COLOR:
   case PIPE_BLENDFACTOR_CONST_ALPHA:
   case PIPE_BLENDFACTOR_ZERO:
   case PIPE_BLENDFACTOR_INV_SRC_COLOR:
   case PIPE_BLENDFACTOR_INV_SRC_ALPHA:
   case PIPE_BLENDFACTOR_INV_DST_ALPHA:
   case PIPE_BLENDFACTOR_INV_DST_COLOR:
   case PIPE_BLENDFACTOR_INV_CONST_COLOR:
   case PIPE_BLENDFACTOR_INV_CONST_ALPHA:
      return true;
   case PIPE_BLENDFACTOR_SRC1_COLOR:
   case PIPE_BLENDFACTOR_SRC1_ALPHA:
   case PIPE_BLENDFACTOR_INV_SRC1_COLOR:
   case PIPE_BLENDFACTOR_INV_SRC1_ALPHA:
      return ctx->dev->screen->caps.max_dual_source_render_targets > 0;
   default:
      return false;
   }
}

static bool
create_blend(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, VIRGL_OBJ_BLEND_SIZE, "a blend state"))
      return false;
   struct vr_object *o = new_object(ctx, VIRGL_OBJECT_BLEND);
   if (!o)
      return false;
   struct pipe_blend_state *b = &o->u.blend;
   uint32_t s0 = arg(c, VIRGL_OBJ_BLEND_S0);
   b->independent_blend_enable = s0 & 1;
   b->logicop_enable = (s0 >> 1) & 1;
   b->dither = (s0 >> 2) & 1;
   b->alpha_to_coverage = (s0 >> 3) & 1;
   b->alpha_to_one = (s0 >> 4) & 1;
   b->logicop_func = arg(c, VIRGL_OBJ_BLEND_S1) & 0xf;
   for (unsigned i = 0; i < VIRGL_MAX_COLOR_BUFS; i++) {
      uint32_t s2 = arg(c, VIRGL_OBJ_BLEND_S2(i));
      struct pipe_rt_blend_state *rt = &b->rt[i];
      rt->blend_enable = s2 & 1;
      rt->rgb_func = (s2 >> 1) & 7;
      rt->rgb_src_factor = (s2 >> 4) & 0x1f;
      rt->rgb_dst_factor = (s2 >> 9) & 0x1f;
      rt->alpha_func = (s2 >> 14) & 7;
      rt->alpha_src_factor = (s2 >> 17) & 0x1f;
      rt->alpha_dst_factor = (s2 >> 22) & 0x1f;
      rt->colormask = (s2 >> 27) & 0xf;
      if (rt->rgb_func > PIPE_BLEND_MAX || rt->alpha_func > PIPE_BLEND_MAX ||
          !blend_factor_ok(ctx, rt->rgb_src_factor) || !blend_factor_ok(ctx, rt->rgb_dst_factor) ||
          !blend_factor_ok(ctx, rt->alpha_src_factor) || !blend_factor_ok(ctx, rt->alpha_dst_factor)) {
         free(o);
         return vr_fail(ctx, "blending of render target %u: 0x%08x", i, s2);
      }
   }
   b->max_rt = b->independent_blend_enable ? VIRGL_MAX_COLOR_BUFS - 1 : 0;
   return vr_object_put(ctx, arg(c, VIRGL_OBJ_BLEND_HANDLE), o);
}

static bool
create_rasterizer(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, VIRGL_OBJ_RS_SIZE, "a rasterizer state"))
      return false;
   struct vr_object *o = new_object(ctx, VIRGL_OBJECT_RASTERIZER);
   if (!o)
      return false;
   struct pipe_rasterizer_state *r = &o->u.rasterizer;
   uint32_t s0 = arg(c, VIRGL_OBJ_RS_S0);
   r->flatshade = s0 & 1;
   r->depth_clip_near = r->depth_clip_far = (s0 >> 1) & 1;
   r->depth_clamp = !r->depth_clip_near;
   r->clip_halfz = (s0 >> 2) & 1;
   r->rasterizer_discard = (s0 >> 3) & 1;
   r->flatshade_first = (s0 >> 4) & 1;
   r->light_twoside = (s0 >> 5) & 1;
   r->sprite_coord_mode = (s0 >> 6) & 1;
   r->point_quad_rasterization = (s0 >> 7) & 1;
   r->cull_face = (s0 >> 8) & 3;
   r->fill_front = (s0 >> 10) & 3;
   r->fill_back = (s0 >> 12) & 3;
   r->scissor = (s0 >> 14) & 1;
   r->front_ccw = (s0 >> 15) & 1;
   r->clamp_vertex_color = (s0 >> 16) & 1;
   r->clamp_fragment_color = (s0 >> 17) & 1;
   r->offset_line = (s0 >> 18) & 1;
   r->offset_point = (s0 >> 19) & 1;
   r->offset_tri = (s0 >> 20) & 1;
   r->poly_smooth = (s0 >> 21) & 1;
   r->poly_stipple_enable = (s0 >> 22) & 1;
   r->point_smooth = (s0 >> 23) & 1;
   r->point_size_per_vertex = (s0 >> 24) & 1;
   r->multisample = (s0 >> 25) & 1;
   r->line_smooth = (s0 >> 26) & 1;
   r->line_stipple_enable = (s0 >> 27) & 1;
   r->line_last_pixel = (s0 >> 28) & 1;
   r->half_pixel_center = (s0 >> 29) & 1;
   r->bottom_edge_rule = (s0 >> 30) & 1;
   r->force_persample_interp = (s0 >> 31) & 1;
   r->point_size = argf(c, VIRGL_OBJ_RS_POINT_SIZE);
   r->sprite_coord_enable = arg(c, VIRGL_OBJ_RS_SPRITE_COORD_ENABLE);
   uint32_t s3 = arg(c, VIRGL_OBJ_RS_S3);
   r->line_stipple_pattern = s3 & 0xffff;
   r->line_stipple_factor = (s3 >> 16) & 0xff;
   r->clip_plane_enable = (s3 >> 24) & 0xff;
   r->line_width = argf(c, VIRGL_OBJ_RS_LINE_WIDTH);
   r->offset_units = argf(c, VIRGL_OBJ_RS_OFFSET_UNITS);
   r->offset_scale = argf(c, VIRGL_OBJ_RS_OFFSET_SCALE);
   r->offset_clamp = argf(c, VIRGL_OBJ_RS_OFFSET_CLAMP);
   if (r->fill_front > PIPE_POLYGON_MODE_POINT || r->fill_back > PIPE_POLYGON_MODE_POINT) {
      unsigned front = r->fill_front, back = r->fill_back;
      free(o);
      return vr_fail(ctx, "polygon modes %u and %u", front, back);
   }
   return vr_object_put(ctx, arg(c, VIRGL_OBJ_RS_HANDLE), o);
}

static bool
create_dsa(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, VIRGL_OBJ_DSA_SIZE, "a depth-stencil state"))
      return false;
   struct vr_object *o = new_object(ctx, VIRGL_OBJECT_DSA);
   if (!o)
      return false;
   struct pipe_depth_stencil_alpha_state *d = &o->u.dsa;
   uint32_t s0 = arg(c, VIRGL_OBJ_DSA_S0);
   d->depth_enabled = s0 & 1;
   d->depth_writemask = (s0 >> 1) & 1;
   d->depth_func = (s0 >> 2) & 7;
   d->alpha_enabled = (s0 >> 8) & 1;
   d->alpha_func = (s0 >> 9) & 7;
   for (unsigned i = 0; i < 2; i++) {
      uint32_t s = arg(c, VIRGL_OBJ_DSA_S1 + i);
      struct pipe_stencil_state *st = &d->stencil[i];
      st->enabled = s & 1;
      st->func = (s >> 1) & 7;
      st->fail_op = (s >> 4) & 7;
      st->zpass_op = (s >> 7) & 7;
      st->zfail_op = (s >> 10) & 7;
      st->valuemask = (s >> 13) & 0xff;
      st->writemask = (s >> 21) & 0xff;
   }
   d->alpha_ref_value = argf(c, VIRGL_OBJ_DSA_ALPHA_REF);
   return vr_object_put(ctx, arg(c, VIRGL_OBJ_DSA_HANDLE), o);
}

static bool
create_sampler_state(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, VIRGL_OBJ_SAMPLER_STATE_SIZE, "a sampler state"))
      return false;
   struct vr_object *o = new_object(ctx, VIRGL_OBJECT_SAMPLER_STATE);
   if (!o)
      return false;
   struct pipe_sampler_state *s = &o->u.sampler;
   uint32_t s0 = arg(c, VIRGL_OBJ_SAMPLER_STATE_S0);
   s->wrap_s = s0 & 7;
   s->wrap_t = (s0 >> 3) & 7;
   s->wrap_r = (s0 >> 6) & 7;
   s->min_img_filter = (s0 >> 9) & 1;
   s->min_mip_filter = (s0 >> 11) & 3;
   s->mag_img_filter = (s0 >> 13) & 1;
   s->compare_mode = (s0 >> 15) & 1;
   s->compare_func = (s0 >> 16) & 7;
   s->seamless_cube_map = (s0 >> 19) & 1;
   s->max_anisotropy = (s0 >> 20) & 0x1f;
   s->lod_bias = argf(c, VIRGL_OBJ_SAMPLER_STATE_LOD_BIAS);
   s->min_lod = argf(c, VIRGL_OBJ_SAMPLER_STATE_MIN_LOD);
   s->max_lod = argf(c, VIRGL_OBJ_SAMPLER_STATE_MAX_LOD);
   for (unsigned i = 0; i < 4; i++)
      s->border_color.ui[i] = arg(c, VIRGL_OBJ_SAMPLER_STATE_BORDER_COLOR(i));
   if (s->min_mip_filter > PIPE_TEX_MIPFILTER_NONE) {
      unsigned filter = s->min_mip_filter;
      free(o);
      return vr_fail(ctx, "mipmap filter %u", filter);
   }
   return vr_object_put(ctx, arg(c, VIRGL_OBJ_SAMPLER_STATE_HANDLE), o);
}

static bool
create_vertex_elements(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, 1, "vertex elements"))
      return false;
   uint32_t n = (c->len - 1) / 4;
   if ((c->len - 1) % 4 || n > PIPE_MAX_ATTRIBS)
      return vr_fail(ctx, "vertex elements in %u words", c->len);
   struct vr_object *o = new_object(ctx, VIRGL_OBJECT_VERTEX_ELEMENTS);
   if (!o)
      return false;
   struct cso_velems_state *v = &o->u.elements;
   v->count = n;
   for (unsigned i = 0; i < n; i++) {
      struct pipe_vertex_element *e = &v->velems[i];
      uint32_t offset = arg(c, VIRGL_OBJ_VERTEX_ELEMENTS_V0_SRC_OFFSET(i));
      uint32_t vb = arg(c, VIRGL_OBJ_VERTEX_ELEMENTS_V0_VERTEX_BUFFER_INDEX(i));
      uint32_t virgl = arg(c, VIRGL_OBJ_VERTEX_ELEMENTS_V0_SRC_FORMAT(i));
      enum pipe_format f = vr_format_from_virgl(virgl);
      /* Gallium keeps the low 8 bits of a vertex format. */
      if (offset > 0xffff || vb >= PIPE_MAX_ATTRIBS || f == PIPE_FORMAT_NONE || f > 0xff ||
          util_format_is_depth_or_stencil(f) || util_format_is_compressed(f)) {
         free(o);
         return vr_fail(ctx, "vertex element %u: buffer %u, offset %u, format %u", i, vb, offset, virgl);
      }
      e->src_offset = offset;
      e->vertex_buffer_index = vb;
      e->src_format = f;
      e->instance_divisor = arg(c, VIRGL_OBJ_VERTEX_ELEMENTS_V0_INSTANCE_DIVISOR(i));
   }
   return vr_object_put(ctx, arg(c, VIRGL_OBJ_VERTEX_ELEMENTS_HANDLE), o);
}

/* ---- Shaders ------------------------------------------------------------ */

/* Whether a shader's control flow is one TGSI's interpreter can follow:
 * every IF and UIF names its ELSE or ENDIF, every ELSE its ENDIF (it jumps
 * there; anywhere else it would loop for ever or run past the end), no
 * subroutines, and no deeper nesting than its stacks hold. */
static bool
control_flow_ok(const struct tgsi_token *tokens)
{
   struct tgsi_parse_context p;
   if (tgsi_parse_init(&p, tokens) != TGSI_PARSE_OK)
      return false;
   unsigned n = 0, ifs = 0, loops = 0;
   /* Where each open IF or ELSE says to go on. */
   unsigned target[TGSI_EXEC_MAX_COND_NESTING];
   bool ok = true;
   while (ok && !tgsi_parse_end_of_tokens(&p)) {
      tgsi_parse_token(&p);
      if (p.FullToken.Token.Type != TGSI_TOKEN_TYPE_INSTRUCTION)
         continue;
      const struct tgsi_full_instruction *inst = &p.FullToken.FullInstruction;
      unsigned label = inst->Instruction.Label ? inst->Label.Label : 0;
      switch (inst->Instruction.Opcode) {
      case TGSI_OPCODE_IF:
      case TGSI_OPCODE_UIF:
         ok = ifs < TGSI_EXEC_MAX_COND_NESTING;
         if (ok)
            target[ifs++] = label;
         break;
      case TGSI_OPCODE_ELSE:
         ok = ifs && target[ifs - 1] == n;
         if (ok)
            target[ifs - 1] = label;
         break;
      case TGSI_OPCODE_ENDIF:
         ok = ifs && target[--ifs] == n;
         break;
      case TGSI_OPCODE_BGNLOOP:
         ok = loops++ < TGSI_EXEC_MAX_LOOP_NESTING;
         break;
      case TGSI_OPCODE_ENDLOOP:
         ok = loops-- > 0;
         break;
      case TGSI_OPCODE_CAL:
      case TGSI_OPCODE_RET:
      case TGSI_OPCODE_BGNSUB:
      case TGSI_OPCODE_ENDSUB:
      case TGSI_OPCODE_SWITCH:
      case TGSI_OPCODE_CASE:
      case TGSI_OPCODE_DEFAULT:
      case TGSI_OPCODE_ENDSWITCH:
         ok = false;
         break;
      }
      n++;
   }
   tgsi_parse_free(&p);
   return ok && !ifs && !loops;
}

static bool
compile_shader(struct vr_context *ctx, struct vr_shader *s)
{
   if (s->text[s->len - 1] != '\0')
      return vr_fail(ctx, "shader text without its end");
   if (tracing()) {
      fprintf(stderr, "vr %p: shader\n%s\n", (void *)ctx, s->text);
      fflush(stderr);
   }
   /* What the client counted, and more if that was too few. */
   struct tgsi_token *tokens = NULL;
   for (uint64_t n = (uint64_t)s->num_tokens + 64; !tokens; n *= 4) {
      if (n > MAX_SHADER_TOKENS)
         return vr_fail(ctx, "a shader that does not translate:\n%.400s", s->text);
      tokens = calloc(n, sizeof(*tokens));
      if (!tokens)
         return vr_fail(ctx, "out of memory");
      if (!tgsi_text_translate(s->text, tokens, n)) {
         free(tokens);
         tokens = NULL;
      }
   }
   if (tgsi_get_processor_type(tokens) != s->stage) {
      free(tokens);
      return vr_fail(ctx, "a shader of another stage");
   }
   if (!control_flow_ok(tokens)) {
      free(tokens);
      return vr_fail(ctx, "a shader whose branches go astray:\n%.400s", s->text);
   }
   struct tgsi_token *adapted;
   if (!vr_adapt_fragment_inputs(tokens, ctx->dev->screen, &adapted)) {
      free(tokens);
      return vr_fail(ctx, "a shader whose fragment position the driver cannot give:\n%.400s", s->text);
   }
   if (adapted && tracing()) {
      fprintf(stderr, "vr %p: as the driver takes it\n", (void *)ctx);
      tgsi_dump_to_file(adapted, 0, stderr);
      fflush(stderr);
   }
   struct pipe_shader_state state;
   memset(&state, 0, sizeof(state));
   state.type = PIPE_SHADER_IR_TGSI;
   state.tokens = adapted ? adapted : tokens;
   state.stream_output = s->so;
   struct pipe_context *pipe = ctx->pipe;
   s->cso = s->stage == MESA_SHADER_VERTEX ? pipe->create_vs_state(pipe, &state) : pipe->create_fs_state(pipe, &state);
   tgsi_free_tokens(adapted);
   free(tokens);
   free(s->text);
   s->text = NULL;
   return s->cso || vr_fail(ctx, "the driver could not compile a shader");
}

/* Adds text (from `words`) to a shader, and compiles it once it is all
 * there. */
static bool
add_shader_text(struct vr_context *ctx, struct vr_shader *s, const uint32_t *words, uint32_t count)
{
   uint32_t n = MIN2((uint64_t)count * 4, s->len - s->have);
   memcpy(s->text + s->have, words, n);
   s->have += n;
   return s->have < s->len || compile_shader(ctx, s);
}

static bool
create_shader(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, VIRGL_OBJ_SHADER_HDR_SIZE(0), "a shader"))
      return false;
   uint32_t handle = arg(c, VIRGL_OBJ_SHADER_HANDLE);
   uint32_t offlen = arg(c, VIRGL_OBJ_SHADER_OFFSET);
   if (offlen & VIRGL_OBJ_SHADER_OFFSET_CONT) {
      /* More of the text of a shader begun before. */
      struct vr_object *o = vr_object_of(ctx, handle, VIRGL_OBJECT_SHADER);
      if (!o || !o->u.shader.text)
         return vr_fail(ctx, "more text for shader %u, which is not being made", handle);
      if ((offlen & ~VIRGL_OBJ_SHADER_OFFSET_CONT) != o->u.shader.have)
         return vr_fail(ctx, "shader text out of order");
      uint32_t hdr = VIRGL_OBJ_SHADER_HDR_SIZE(0);
      return add_shader_text(ctx, &o->u.shader, c->w + hdr, c->len - hdr);
   }

   mesa_shader_stage stage;
   if (!stage_of(ctx, arg(c, VIRGL_OBJ_SHADER_TYPE), &stage))
      return false;
   uint32_t len = offlen, tokens = arg(c, VIRGL_OBJ_SHADER_NUM_TOKENS);
   uint32_t nso = arg(c, VIRGL_OBJ_SHADER_SO_NUM_OUTPUTS);
   if (!len || len > MAX_SHADER_TEXT || tokens > MAX_SHADER_TOKENS || nso > PIPE_MAX_SO_OUTPUTS)
      return vr_fail(ctx, "a shader of %u bytes, %u tokens and %u outputs captured", len, tokens, nso);
   uint32_t hdr = VIRGL_OBJ_SHADER_HDR_SIZE(nso);
   if (!need(ctx, c, hdr, "a shader"))
      return false;
   struct vr_object *o = new_object(ctx, VIRGL_OBJECT_SHADER);
   if (!o)
      return false;
   struct vr_shader *s = &o->u.shader;
   s->stage = stage;
   s->len = len;
   s->num_tokens = tokens;
   s->so.num_outputs = nso;
   if (nso) {
      for (unsigned i = 0; i < PIPE_MAX_SO_BUFFERS; i++)
         s->so.stride[i] = arg(c, VIRGL_OBJ_SHADER_SO_STRIDE(i));
      for (unsigned i = 0; i < nso; i++) {
         uint32_t a = arg(c, VIRGL_OBJ_SHADER_SO_OUTPUT0(i));
         struct pipe_stream_output *out = &s->so.output[i];
         unsigned reg = a & 0xff, start = (a >> 8) & 3, comps = (a >> 10) & 7, buf = (a >> 13) & 7;
         if (reg >= 64 || !comps || start + comps > 4 || buf >= PIPE_MAX_SO_BUFFERS) {
            free(o);
            return vr_fail(ctx, "captured output %u: 0x%08x", i, a);
         }
         out->register_index = reg;
         out->start_component = start;
         out->num_components = comps;
         out->output_buffer = buf;
         out->dst_offset = a >> 16;
         out->stream = arg(c, VIRGL_OBJ_SHADER_SO_OUTPUT0_SO(i)) & 3;
      }
   }
   s->text = malloc(len);
   if (!s->text) {
      free(o);
      return vr_fail(ctx, "out of memory");
   }
   if (!vr_object_put(ctx, handle, o))
      return false;
   return add_shader_text(ctx, s, c->w + hdr, c->len - hdr);
}

/* ---- Views, surfaces, queries, stream output targets -------------------- */

static bool
create_sampler_view(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, VIRGL_OBJ_SAMPLER_VIEW_SIZE, "a sampler view"))
      return false;
   uint32_t id = arg(c, VIRGL_OBJ_SAMPLER_VIEW_RES_HANDLE);
   struct pipe_resource *t = texture_of(ctx, id);
   if (!t)
      return false;
   uint32_t fw = arg(c, VIRGL_OBJ_SAMPLER_VIEW_FORMAT);
   enum pipe_format f = vr_format(ctx->dev, fw & 0xffffff);
   unsigned target = fw >> 24 ? fw >> 24 : t->target;
   uint32_t layers = arg(c, VIRGL_OBJ_SAMPLER_VIEW_TEXTURE_LAYER);
   uint32_t levels = arg(c, VIRGL_OBJ_SAMPLER_VIEW_TEXTURE_LEVEL);
   uint32_t swizzle = arg(c, VIRGL_OBJ_SAMPLER_VIEW_SWIZZLE);
   struct pipe_sampler_view templ;
   memset(&templ, 0, sizeof(templ));
   templ.format = f;
   templ.target = target;
   templ.u.tex.first_layer = layers & 0xffff;
   templ.u.tex.last_layer = layers >> 16;
   templ.u.tex.first_level = levels & 0xff;
   templ.u.tex.last_level = (levels >> 8) & 0xff;
   templ.swizzle_r = swizzle & 7;
   templ.swizzle_g = (swizzle >> 3) & 7;
   templ.swizzle_b = (swizzle >> 6) & 7;
   templ.swizzle_a = (swizzle >> 9) & 7;
   struct pipe_screen *s = ctx->dev->screen;
   unsigned samples = t->nr_samples;
   if (f == PIPE_FORMAT_NONE || !compatible(f, t->format) || target != t->target ||
       !s->is_format_supported(s, f, target, samples, samples, PIPE_BIND_SAMPLER_VIEW))
      return vr_fail(ctx, "a view of resource %u as format %u, target %u", id, fw & 0xffffff, target);
   if (templ.u.tex.first_level > templ.u.tex.last_level || templ.u.tex.last_level > t->last_level ||
       templ.u.tex.first_layer > templ.u.tex.last_layer || templ.u.tex.last_layer >= level_layers(t, 0))
      return vr_fail(ctx, "a view of levels 0x%x, layers 0x%x of resource %u", levels, layers, id);
   if (templ.swizzle_r > PIPE_SWIZZLE_1 || templ.swizzle_g > PIPE_SWIZZLE_1 || templ.swizzle_b > PIPE_SWIZZLE_1 ||
       templ.swizzle_a > PIPE_SWIZZLE_1)
      return vr_fail(ctx, "swizzle 0x%x", swizzle);
   struct vr_object *o = new_object(ctx, VIRGL_OBJECT_SAMPLER_VIEW);
   if (!o)
      return false;
   o->u.view = ctx->pipe->create_sampler_view(ctx->pipe, t, &templ);
   if (!o->u.view) {
      free(o);
      return vr_fail(ctx, "the driver could not make a view");
   }
   return vr_object_put(ctx, arg(c, VIRGL_OBJ_SAMPLER_VIEW_HANDLE), o);
}

static bool
create_surface(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, VIRGL_OBJ_SURFACE_SIZE, "a surface"))
      return false;
   uint32_t id = arg(c, VIRGL_OBJ_SURFACE_RES_HANDLE);
   struct pipe_resource *t = texture_of(ctx, id);
   if (!t)
      return false;
   uint32_t virgl = arg(c, VIRGL_OBJ_SURFACE_FORMAT);
   enum pipe_format f = vr_format(ctx->dev, virgl);
   uint32_t level = arg(c, VIRGL_OBJ_SURFACE_TEXTURE_LEVEL);
   uint32_t layers = arg(c, VIRGL_OBJ_SURFACE_TEXTURE_LAYERS);
   unsigned first = layers & 0xffff, last = layers >> 16;
   struct pipe_screen *s = ctx->dev->screen;
   bool zs = util_format_is_depth_or_stencil(f);
   unsigned bind = zs ? PIPE_BIND_DEPTH_STENCIL : PIPE_BIND_RENDER_TARGET;
   if (f == PIPE_FORMAT_NONE || !compatible(f, t->format) ||
       !s->is_format_supported(s, f, t->target, t->nr_samples, t->nr_storage_samples, bind))
      return vr_fail(ctx, "a surface of resource %u as format %u", id, virgl);
   if (level > t->last_level || first > last || last >= level_layers(t, level))
      return vr_fail(ctx, "a surface of level %u, layers 0x%x of resource %u", level, layers, id);
   struct vr_object *o = new_object(ctx, VIRGL_OBJECT_SURFACE);
   if (!o)
      return false;
   struct pipe_surface *surf = &o->u.surface;
   surf->format = f;
   surf->level = level;
   surf->first_layer = first;
   surf->last_layer = last;
   pipe_resource_reference(&surf->texture, t);
   return vr_object_put(ctx, arg(c, VIRGL_OBJ_SURFACE_HANDLE), o);
}

static bool
create_query(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, VIRGL_OBJ_QUERY_SIZE, "a query"))
      return false;
   uint32_t ti = arg(c, VIRGL_OBJ_QUERY_TYPE_INDEX);
   unsigned type = ti & 0xffff, index = ti >> 16;
   uint32_t offset = arg(c, VIRGL_OBJ_QUERY_OFFSET), id = arg(c, VIRGL_OBJ_QUERY_RES_HANDLE);
   switch (type) {
   case PIPE_QUERY_OCCLUSION_COUNTER:
   case PIPE_QUERY_OCCLUSION_PREDICATE:
   case PIPE_QUERY_OCCLUSION_PREDICATE_CONSERVATIVE:
   case PIPE_QUERY_PRIMITIVES_GENERATED:
   case PIPE_QUERY_PRIMITIVES_EMITTED:
      break;
   default:
      return vr_fail(ctx, "queries of type %u", type);
   }
   if (index >= PIPE_MAX_VERTEX_STREAMS)
      return vr_fail(ctx, "a query of stream %u", index);
   if (offset % 8 || !vr_backing(ctx, vr_resource(ctx, id), offset, sizeof(struct virgl_host_query_state)))
      return vr_fail(ctx, "a query's result at %u of resource %u", offset, id);
   struct vr_object *o = new_object(ctx, VIRGL_OBJECT_QUERY);
   if (!o)
      return false;
   struct vr_query *q = &o->u.query;
   q->pq = ctx->pipe->create_query(ctx->pipe, type, index);
   if (!q->pq) {
      free(o);
      return vr_fail(ctx, "the driver could not make a query of type %u", type);
   }
   q->type = type;
   q->resource = id;
   q->offset = offset;
   return vr_object_put(ctx, arg(c, VIRGL_OBJ_QUERY_HANDLE), o);
}

static bool
create_streamout_target(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, VIRGL_OBJ_STREAMOUT_SIZE, "a stream output target"))
      return false;
   struct pipe_resource *b = buffer_of(ctx, arg(c, VIRGL_OBJ_STREAMOUT_RES_HANDLE));
   if (!b)
      return false;
   uint32_t offset = arg(c, VIRGL_OBJ_STREAMOUT_BUFFER_OFFSET), size = arg(c, VIRGL_OBJ_STREAMOUT_BUFFER_SIZE);
   if (offset % 4 || offset > b->width0 || size > b->width0 - offset)
      return vr_fail(ctx, "a stream output target of %u bytes at %u", size, offset);
   struct vr_object *o = new_object(ctx, VIRGL_OBJECT_STREAMOUT_TARGET);
   if (!o)
      return false;
   o->u.target = ctx->pipe->create_stream_output_target(ctx->pipe, b, offset, size);
   if (!o->u.target) {
      free(o);
      return vr_fail(ctx, "the driver could not make a stream output target");
   }
   return vr_object_put(ctx, arg(c, VIRGL_OBJ_STREAMOUT_HANDLE), o);
}

static bool
create_object(struct vr_context *ctx, uint32_t type, const struct cmd *c)
{
   switch (type) {
   case VIRGL_OBJECT_BLEND:
      return create_blend(ctx, c);
   case VIRGL_OBJECT_RASTERIZER:
      return create_rasterizer(ctx, c);
   case VIRGL_OBJECT_DSA:
      return create_dsa(ctx, c);
   case VIRGL_OBJECT_SHADER:
      return create_shader(ctx, c);
   case VIRGL_OBJECT_VERTEX_ELEMENTS:
      return create_vertex_elements(ctx, c);
   case VIRGL_OBJECT_SAMPLER_VIEW:
      return create_sampler_view(ctx, c);
   case VIRGL_OBJECT_SAMPLER_STATE:
      return create_sampler_state(ctx, c);
   case VIRGL_OBJECT_SURFACE:
      return create_surface(ctx, c);
   case VIRGL_OBJECT_QUERY:
      return create_query(ctx, c);
   case VIRGL_OBJECT_STREAMOUT_TARGET:
      return create_streamout_target(ctx, c);
   default:
      return vr_fail(ctx, "objects of type %u", type);
   }
}

static bool
bind_object(struct vr_context *ctx, uint32_t type, const struct cmd *c)
{
   if (!need(ctx, c, 1, "a binding"))
      return false;
   uint32_t h = arg(c, VIRGL_OBJ_BIND_HANDLE);
   struct vr_object *o = h ? vr_object_of(ctx, h, type) : NULL;
   if (h && !o)
      return vr_fail(ctx, "object %u is not of type %u", h, type);
   enum pipe_error e = PIPE_OK;
   switch (type) {
   case VIRGL_OBJECT_BLEND:
      if (o)
         e = cso_set_blend(ctx->cso, &o->u.blend);
      ctx->blend = h;
      break;
   case VIRGL_OBJECT_DSA:
      if (o)
         e = cso_set_depth_stencil_alpha(ctx->cso, &o->u.dsa);
      ctx->dsa = h;
      break;
   case VIRGL_OBJECT_RASTERIZER:
      if (o)
         e = cso_set_rasterizer(ctx->cso, &o->u.rasterizer);
      ctx->rasterizer = h;
      break;
   case VIRGL_OBJECT_VERTEX_ELEMENTS:
      ctx->elements = h;
      ctx->vertex_dirty = true;
      break;
   default:
      return vr_fail(ctx, "binding objects of type %u", type);
   }
   return e == PIPE_OK || vr_fail(ctx, "out of memory");
}

/* ---- Fixed state -------------------------------------------------------- */

static bool
set_viewports(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, 1, "viewports") || (c->len - 1) % 6)
      return vr_fail(ctx, "viewports in %u words", c->len);
   uint32_t start = arg(c, VIRGL_SET_VIEWPORT_START_SLOT), n = (c->len - 1) / 6;
   unsigned max = MIN2(ctx->dev->screen->caps.max_viewports, PIPE_MAX_VIEWPORTS);
   if (start > max || n > max - start)
      return vr_fail(ctx, "viewports %u to %u", start, start + n);
   struct pipe_viewport_state vp[PIPE_MAX_VIEWPORTS];
   memset(vp, 0, sizeof(vp));
   for (unsigned i = 0; i < n; i++) {
      vp[i].scale[0] = argf(c, VIRGL_SET_VIEWPORT_STATE_SCALE_0(i));
      vp[i].scale[1] = argf(c, VIRGL_SET_VIEWPORT_STATE_SCALE_1(i));
      vp[i].scale[2] = argf(c, VIRGL_SET_VIEWPORT_STATE_SCALE_2(i));
      vp[i].translate[0] = argf(c, VIRGL_SET_VIEWPORT_STATE_TRANSLATE_0(i));
      vp[i].translate[1] = argf(c, VIRGL_SET_VIEWPORT_STATE_TRANSLATE_1(i));
      vp[i].translate[2] = argf(c, VIRGL_SET_VIEWPORT_STATE_TRANSLATE_2(i));
      vp[i].swizzle_x = PIPE_VIEWPORT_SWIZZLE_POSITIVE_X;
      vp[i].swizzle_y = PIPE_VIEWPORT_SWIZZLE_POSITIVE_Y;
      vp[i].swizzle_z = PIPE_VIEWPORT_SWIZZLE_POSITIVE_Z;
      vp[i].swizzle_w = PIPE_VIEWPORT_SWIZZLE_POSITIVE_W;
   }
   if (n)
      ctx->pipe->set_viewport_states(ctx->pipe, start, n, vp);
   return true;
}

static bool
set_scissors(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, 1, "scissors") || (c->len - 1) % 2)
      return vr_fail(ctx, "scissors in %u words", c->len);
   uint32_t start = arg(c, VIRGL_SET_SCISSOR_START_SLOT), n = (c->len - 1) / 2;
   unsigned max = MIN2(ctx->dev->screen->caps.max_viewports, PIPE_MAX_VIEWPORTS);
   if (start > max || n > max - start)
      return vr_fail(ctx, "scissors %u to %u", start, start + n);
   struct pipe_scissor_state ss[PIPE_MAX_VIEWPORTS];
   for (unsigned i = 0; i < n; i++) {
      uint32_t lo = arg(c, VIRGL_SET_SCISSOR_MINX_MINY(i)), hi = arg(c, VIRGL_SET_SCISSOR_MAXX_MAXY(i));
      ss[i] = (struct pipe_scissor_state){lo & 0xffff, lo >> 16, hi & 0xffff, hi >> 16};
   }
   if (n)
      ctx->pipe->set_scissor_states(ctx->pipe, start, n, ss);
   return true;
}

static bool
set_framebuffer(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, 2, "a framebuffer"))
      return false;
   uint32_t n = arg(c, VIRGL_SET_FRAMEBUFFER_STATE_NR_CBUFS);
   if (n > PIPE_MAX_COLOR_BUFS || n > ctx->dev->screen->caps.max_render_targets ||
       !need(ctx, c, VIRGL_SET_FRAMEBUFFER_STATE_SIZE(n), "a framebuffer"))
      return vr_fail(ctx, "a framebuffer of %u color buffers", n);
   struct pipe_framebuffer_state fb;
   memset(&fb, 0, sizeof(fb));
   uint32_t width = UINT32_MAX, height = UINT32_MAX, colors = 0, zs = 0;
   unsigned samples = 0;
   bool any = false;
   for (unsigned i = 0; i <= n; i++) {
      uint32_t h = i ? arg(c, VIRGL_SET_FRAMEBUFFER_STATE_CBUF_HANDLE(i - 1))
                     : arg(c, VIRGL_SET_FRAMEBUFFER_STATE_NR_ZSURF_HANDLE);
      if (!h)
         continue;
      struct vr_object *o = vr_object_of(ctx, h, VIRGL_OBJECT_SURFACE);
      if (!o)
         return vr_fail(ctx, "object %u is not a surface", h);
      const struct pipe_surface *s = &o->u.surface;
      const struct util_format_description *desc = util_format_description(s->format);
      bool is_zs = util_format_is_depth_or_stencil(s->format);
      if (is_zs != (i == 0))
         return vr_fail(ctx, "surface %u where %s go", h, i ? "colors" : "depth and stencil");
      unsigned ns = MAX2(s->texture->nr_samples, 1);
      if (any && ns != samples)
         return vr_fail(ctx, "attachments of %u and %u samples", samples, ns);
      samples = ns;
      any = true;
      width = MIN2(width, u_minify(s->texture->width0, s->level));
      height = MIN2(height, u_minify(s->texture->height0, s->level));
      if (i) {
         fb.cbufs[i - 1] = *s;
         colors |= 1u << (i - 1);
      } else {
         fb.zsbuf = *s;
         zs = (util_format_has_depth(desc) ? PIPE_CLEAR_DEPTH : 0) |
              (util_format_has_stencil(desc) ? PIPE_CLEAR_STENCIL : 0);
      }
   }
   fb.nr_cbufs = n;
   fb.width = any ? width : 0;
   fb.height = any ? height : 0;
   cso_set_framebuffer(ctx->cso, &fb);
   ctx->fb_colors = colors;
   ctx->fb_zs = zs;
   return true;
}

static bool
set_vertex_buffers(struct vr_context *ctx, const struct cmd *c)
{
   uint32_t n = c->len / 3;
   if (c->len % 3 || n > PIPE_MAX_ATTRIBS)
      return vr_fail(ctx, "vertex buffers in %u words", c->len);
   unsigned max_stride = ctx->dev->screen->caps.max_vertex_attrib_stride;
   struct pipe_vertex_buffer vb[PIPE_MAX_ATTRIBS];
   memset(vb, 0, sizeof(vb));
   for (unsigned i = 0; i < n; i++) {
      uint32_t stride = arg(c, VIRGL_SET_VERTEX_BUFFER_STRIDE(i)), id = arg(c, VIRGL_SET_VERTEX_BUFFER_HANDLE(i));
      if (max_stride && stride > max_stride)
         return vr_fail(ctx, "a vertex stride of %u", stride);
      if (id && !(vb[i].buffer.resource = buffer_of(ctx, id)))
         return false;
      vb[i].buffer_offset = arg(c, VIRGL_SET_VERTEX_BUFFER_OFFSET(i));
   }
   for (unsigned i = 0; i < PIPE_MAX_ATTRIBS; i++) {
      if (i < n) {
         pipe_vertex_buffer_reference(&ctx->vbufs[i], &vb[i]);
         ctx->strides[i] = arg(c, VIRGL_SET_VERTEX_BUFFER_STRIDE(i));
      } else {
         pipe_vertex_buffer_unreference(&ctx->vbufs[i]);
         ctx->strides[i] = 0;
      }
   }
   ctx->num_vbufs = n;
   ctx->vertex_dirty = true;
   return true;
}

static bool
set_index_buffer(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, 1, "an index buffer"))
      return false;
   uint32_t id = arg(c, VIRGL_SET_INDEX_BUFFER_HANDLE);
   if (!id) {
      pipe_resource_reference(&ctx->ib, NULL);
      return true;
   }
   if (!need(ctx, c, 3, "an index buffer"))
      return false;
   struct pipe_resource *b = buffer_of(ctx, id);
   if (!b)
      return false;
   uint32_t size = arg(c, VIRGL_SET_INDEX_BUFFER_INDEX_SIZE), offset = arg(c, VIRGL_SET_INDEX_BUFFER_OFFSET);
   if ((size != 1 && size != 2 && size != 4) || offset % size || offset > b->width0)
      return vr_fail(ctx, "indices of %u bytes from %u", size, offset);
   pipe_resource_reference(&ctx->ib, b);
   ctx->ib_size = size;
   ctx->ib_offset = offset;
   return true;
}

static bool
set_constant_buffer(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, 2, "constants"))
      return false;
   mesa_shader_stage stage;
   if (!stage_of(ctx, arg(c, VIRGL_SET_CONSTANT_BUFFER_SHADER_TYPE), &stage))
      return false;
   if (arg(c, VIRGL_SET_CONSTANT_BUFFER_INDEX) != 0)
      return vr_fail(ctx, "constants in slot %u", arg(c, VIRGL_SET_CONSTANT_BUFFER_INDEX));
   uint32_t size = (c->len - 2) * 4;
   if (size > ctx->dev->screen->shader_caps[stage].max_const_buffer0_size)
      return vr_fail(ctx, "%u bytes of constants", size);
   void *copy = NULL;
   if (size) {
      copy = malloc(size);
      if (!copy)
         return vr_fail(ctx, "out of memory");
      memcpy(copy, c->w + 2, size);
   }
   struct pipe_constant_buffer cb = {.buffer_size = size, .user_buffer = copy};
   ctx->pipe->set_constant_buffer(ctx->pipe, stage, 0, size ? &cb : NULL);
   free(ctx->constants[stage]);
   ctx->constants[stage] = copy;
   return true;
}

static bool
set_uniform_buffer(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, VIRGL_SET_UNIFORM_BUFFER_SIZE, "a uniform buffer"))
      return false;
   mesa_shader_stage stage;
   if (!stage_of(ctx, arg(c, VIRGL_SET_UNIFORM_BUFFER_SHADER_TYPE), &stage))
      return false;
   uint32_t index = arg(c, VIRGL_SET_UNIFORM_BUFFER_INDEX), id = arg(c, VIRGL_SET_UNIFORM_BUFFER_RES_HANDLE);
   uint32_t offset = arg(c, VIRGL_SET_UNIFORM_BUFFER_OFFSET), len = arg(c, VIRGL_SET_UNIFORM_BUFFER_LENGTH);
   unsigned slots = MIN2(ctx->dev->screen->shader_caps[stage].max_const_buffers, PIPE_MAX_CONSTANT_BUFFERS);
   if (index == 0 || index >= slots)
      return vr_fail(ctx, "uniform buffer %u", index);
   if (!id) {
      ctx->pipe->set_constant_buffer(ctx->pipe, stage, index, NULL);
      return true;
   }
   struct pipe_resource *b = buffer_of(ctx, id);
   if (!b)
      return false;
   unsigned align = MAX2(ctx->dev->screen->caps.constant_buffer_offset_alignment, 1);
   if (offset % align || offset > b->width0 || len > b->width0 - offset)
      return vr_fail(ctx, "a uniform buffer of %u bytes at %u", len, offset);
   struct pipe_constant_buffer cb = {.buffer = b, .buffer_offset = offset, .buffer_size = len};
   ctx->pipe->set_constant_buffer(ctx->pipe, stage, index, &cb);
   return true;
}

static bool
set_sampler_views(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, 2, "sampler views"))
      return false;
   mesa_shader_stage stage;
   if (!stage_of(ctx, arg(c, VIRGL_SET_SAMPLER_VIEWS_SHADER_TYPE), &stage))
      return false;
   uint32_t start = arg(c, VIRGL_SET_SAMPLER_VIEWS_START_SLOT), n = c->len - 2;
   /* A driver that leaves the count of views 0 (virgl) takes as many as
    * it has samplers, as Mesa's OpenGL asks of it. */
   const struct pipe_shader_caps *caps = &ctx->dev->screen->shader_caps[stage];
   unsigned max = MIN2(caps->max_sampler_views ? caps->max_sampler_views : caps->max_texture_samplers,
                       PIPE_MAX_SHADER_SAMPLER_VIEWS);
   if (start > max || n > max - start)
      return vr_fail(ctx, "sampler views %u to %u", start, start + n);
   struct pipe_sampler_view *views[PIPE_MAX_SHADER_SAMPLER_VIEWS];
   for (unsigned i = 0; i < n; i++) {
      uint32_t h = arg(c, VIRGL_SET_SAMPLER_VIEWS_V0_HANDLE + i);
      struct vr_object *o = h ? vr_object_of(ctx, h, VIRGL_OBJECT_SAMPLER_VIEW) : NULL;
      if (h && !o)
         return vr_fail(ctx, "object %u is not a sampler view", h);
      views[i] = o ? o->u.view : NULL;
   }
   if (n)
      ctx->pipe->set_sampler_views(ctx->pipe, stage, start, n, 0, views);
   return true;
}

static bool
bind_sampler_states(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, 2, "sampler states"))
      return false;
   mesa_shader_stage stage;
   if (!stage_of(ctx, arg(c, VIRGL_BIND_SAMPLER_STATES_SHADER_TYPE), &stage))
      return false;
   uint32_t start = arg(c, VIRGL_BIND_SAMPLER_STATES_START_SLOT), n = c->len - 2;
   unsigned max = MIN2(ctx->dev->screen->shader_caps[stage].max_texture_samplers, PIPE_MAX_SAMPLERS);
   if (start > max || n > max - start)
      return vr_fail(ctx, "sampler states %u to %u", start, start + n);
   for (unsigned i = 0; i < n; i++) {
      uint32_t h = arg(c, VIRGL_BIND_SAMPLER_STATES_S0_HANDLE + i);
      struct vr_object *o = h ? vr_object_of(ctx, h, VIRGL_OBJECT_SAMPLER_STATE) : NULL;
      if (h && !o)
         return vr_fail(ctx, "object %u is not a sampler state", h);
   }
   for (unsigned i = 0; i < n; i++) {
      struct vr_object *o = vr_object(ctx, arg(c, VIRGL_BIND_SAMPLER_STATES_S0_HANDLE + i));
      cso_single_sampler(ctx->cso, stage, start + i, o ? &o->u.sampler : NULL);
   }
   cso_single_sampler_done(ctx->cso, stage);
   return true;
}

static bool
bind_shader(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, VIRGL_BIND_SHADER_SIZE, "a shader binding"))
      return false;
   mesa_shader_stage stage;
   if (!stage_of(ctx, arg(c, VIRGL_BIND_SHADER_TYPE), &stage))
      return false;
   uint32_t h = arg(c, VIRGL_BIND_SHADER_HANDLE);
   struct vr_object *o = h ? vr_object_of(ctx, h, VIRGL_OBJECT_SHADER) : NULL;
   if (h && (!o || !o->u.shader.cso || o->u.shader.stage != stage))
      return vr_fail(ctx, "object %u is not a finished shader of that stage", h);
   void *cso = o ? o->u.shader.cso : NULL;
   if (stage == MESA_SHADER_VERTEX) {
      cso_set_vertex_shader_handle(ctx->cso, cso);
      ctx->vs = h;
   } else {
      cso_set_fragment_shader_handle(ctx->cso, cso);
      ctx->fs = h;
   }
   return true;
}

static bool
set_streamout_targets(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, 1, "stream output targets"))
      return false;
   uint32_t n = c->len - 1;
   if (n > PIPE_MAX_SO_BUFFERS || n > ctx->dev->screen->caps.max_stream_output_buffers)
      return vr_fail(ctx, "%u stream output targets", n);
   struct pipe_stream_output_target *t[PIPE_MAX_SO_BUFFERS] = {0};
   for (unsigned i = 0; i < n; i++) {
      uint32_t h = arg(c, VIRGL_SET_STREAMOUT_TARGETS_H0 + i);
      struct vr_object *o = h ? vr_object_of(ctx, h, VIRGL_OBJECT_STREAMOUT_TARGET) : NULL;
      if (h && !o)
         return vr_fail(ctx, "object %u is not a stream output target", h);
      t[i] = o ? o->u.target : NULL;
   }
   for (unsigned i = 0; i < PIPE_MAX_SO_BUFFERS; i++)
      pipe_so_target_reference(&ctx->so[i], t[i]);
   ctx->num_so = n;
   ctx->so_append = arg(c, VIRGL_SET_STREAMOUT_TARGETS_APPEND_BITMASK);
   ctx->so_dirty = true;
   return true;
}

/* ---- Clears, draws, blits, copies --------------------------------------- */

static bool
clear(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, VIRGL_OBJ_CLEAR_SIZE, "a clear"))
      return false;
   /* Only what the framebuffer has. */
   unsigned buffers = arg(c, VIRGL_OBJ_CLEAR_BUFFERS) & ((ctx->fb_colors << 2) | ctx->fb_zs);
   union pipe_color_union color;
   memcpy(color.ui, c->w + VIRGL_OBJ_CLEAR_COLOR_0 - 1, sizeof(color.ui));
   double depth;
   memcpy(&depth, c->w + VIRGL_OBJ_CLEAR_DEPTH_0 - 1, sizeof(depth));
   if (!(depth >= 0.0 && depth <= 1.0))
      depth = depth > 1.0 ? 1.0 : 0.0;
   if (buffers)
      ctx->pipe->clear(ctx->pipe, buffers, ~0u, 0xff, NULL, &color, depth, arg(c, VIRGL_OBJ_CLEAR_STENCIL) & 0xff);
   return true;
}

/* Sets what the next draw needs that waited for it: vertex elements with
 * the buffers' strides, and stream output for its primitive. */
static bool
prepare_draw(struct vr_context *ctx, enum mesa_prim mode)
{
   if (!ctx->vs || !ctx->fs || !ctx->blend || !ctx->dsa || !ctx->rasterizer || !ctx->elements)
      return vr_fail(ctx, "a draw without shaders, blending, depth-stencil, rasterizer or vertex state");
   if (ctx->vertex_dirty) {
      struct vr_object *o = vr_object_of(ctx, ctx->elements, VIRGL_OBJECT_VERTEX_ELEMENTS);
      struct cso_velems_state v = o->u.elements;
      unsigned count = 0;
      for (unsigned i = 0; i < v.count; i++) {
         unsigned vb = v.velems[i].vertex_buffer_index;
         if (vb >= ctx->num_vbufs || !ctx->vbufs[vb].buffer.resource)
            return vr_fail(ctx, "vertex element %u reads vertex buffer %u, which is not set", i, vb);
         v.velems[i].src_stride = ctx->strides[vb];
         count = MAX2(count, vb + 1);
      }
      cso_set_vertex_buffers_and_elements(ctx->cso, &v, count, false, ctx->vbufs);
      ctx->vertex_dirty = false;
   }
   enum mesa_prim prim = u_reduced_prim(mode);
   if (ctx->so_dirty || (ctx->num_so && prim != ctx->so_prim)) {
      /* Targets set again for another primitive go on where they were. */
      unsigned offsets[PIPE_MAX_SO_BUFFERS];
      for (unsigned i = 0; i < PIPE_MAX_SO_BUFFERS; i++)
         offsets[i] = !ctx->so_dirty || (ctx->so_append & (1u << i)) ? (unsigned)-1 : 0;
      cso_set_stream_outputs(ctx->cso, ctx->num_so, ctx->so, offsets, prim);
      ctx->so_dirty = false;
      ctx->so_prim = prim;
   }
   return true;
}

static bool
draw_vbo(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, VIRGL_DRAW_VBO_SIZE, "a draw"))
      return false;
   for (unsigned i = VIRGL_DRAW_VBO_SIZE + 1; i <= c->len; i++) {
      /* Patches and indirect draws: none of OpenGL ES 3.0's. */
      if (arg(c, i))
         return vr_fail(ctx, "a draw with word %u set", i);
   }
   uint32_t mode = arg(c, VIRGL_DRAW_VBO_MODE), count = arg(c, VIRGL_DRAW_VBO_COUNT);
   uint32_t instances = arg(c, VIRGL_DRAW_VBO_INSTANCE_COUNT);
   bool indexed = arg(c, VIRGL_DRAW_VBO_INDEXED);
   if (mode >= MESA_PRIM_COUNT || !(ctx->dev->screen->caps.supported_prim_modes & (1u << mode)))
      return vr_fail(ctx, "primitive %u", mode);
   if (arg(c, VIRGL_DRAW_VBO_COUNT_FROM_SO))
      return vr_fail(ctx, "a draw of what stream output captured");
   if (!count || !instances)
      return true;
   struct pipe_draw_info info;
   memset(&info, 0, sizeof(info));
   struct pipe_draw_start_count_bias draw = {
      .start = arg(c, VIRGL_DRAW_VBO_START),
      .count = count,
      .index_bias = (int)arg(c, VIRGL_DRAW_VBO_INDEX_BIAS),
   };
   info.mode = mode;
   info.instance_count = instances;
   info.start_instance = arg(c, VIRGL_DRAW_VBO_START_INSTANCE);
   info.primitive_restart = arg(c, VIRGL_DRAW_VBO_PRIMITIVE_RESTART) != 0;
   info.restart_index = arg(c, VIRGL_DRAW_VBO_RESTART_INDEX);
   if (indexed) {
      if (!ctx->ib)
         return vr_fail(ctx, "an indexed draw without indices");
      /* The indices' offset goes into the start. */
      uint64_t first = (uint64_t)draw.start + ctx->ib_offset / ctx->ib_size;
      if ((first + count) * ctx->ib_size > ctx->ib->width0)
         return vr_fail(ctx, "%u indices from %llu, past the end of their buffer", count,
                        (unsigned long long)first);
      draw.start = first;
      info.index_size = ctx->ib_size;
      info.index.resource = ctx->ib;
   } else {
      info.primitive_restart = false;
   }
   if (!prepare_draw(ctx, mode))
      return false;
   cso_draw_vbo(ctx->cso, &info, 0, NULL, &draw, 1);
   return true;
}

/* One side of a blit: its resource, level, format and box. */
static bool
blit_side(struct vr_context *ctx, const struct cmd *c, unsigned at, struct pipe_resource **res, unsigned *level,
          enum pipe_format *format, struct pipe_box *box)
{
   uint32_t id = arg(c, at);
   if (!(*res = texture_of(ctx, id)))
      return false;
   *level = arg(c, at + 1);
   uint32_t virgl = arg(c, at + 2);
   *format = vr_format(ctx->dev, virgl);
   if (*format == PIPE_FORMAT_NONE || !compatible(*format, (*res)->format) || *level > (*res)->last_level)
      return vr_fail(ctx, "a blit of level %u of resource %u as format %u", *level, id, virgl);
   return make_box(ctx, arg(c, at + 3), arg(c, at + 4), arg(c, at + 5), arg(c, at + 6), arg(c, at + 7),
                   arg(c, at + 8), true, box);
}

static void
flip(int32_t *dst_at, int32_t *dst_size, int32_t *src_at, int32_t *src_size)
{
   if (*dst_size < 0) {
      *dst_at += *dst_size;
      *dst_size = -*dst_size;
      *src_at += *src_size;
      *src_size = -*src_size;
   }
}

static bool
blit(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, VIRGL_CMD_BLIT_SIZE, "a blit"))
      return false;
   struct pipe_blit_info info;
   memset(&info, 0, sizeof(info));
   uint32_t s0 = arg(c, VIRGL_CMD_BLIT_S0);
   info.mask = s0 & 0xff;
   info.filter = (s0 >> 8) & 3;
   info.scissor_enable = (s0 >> 10) & 1;
   info.render_condition_enable = (s0 >> 11) & 1;
   info.alpha_blend = (s0 >> 12) & 1;
   uint32_t lo = arg(c, VIRGL_CMD_BLIT_SCISSOR_MINX_MINY), hi = arg(c, VIRGL_CMD_BLIT_SCISSOR_MAXX_MAXY);
   info.scissor = (struct pipe_scissor_state){lo & 0xffff, lo >> 16, hi & 0xffff, hi >> 16};
   if (info.mask & ~PIPE_MASK_RGBAZS || info.filter > PIPE_TEX_FILTER_LINEAR)
      return vr_fail(ctx, "a blit of mask 0x%x, filter %u", info.mask, info.filter);
   if (!blit_side(ctx, c, VIRGL_CMD_BLIT_DST_RES_HANDLE, &info.dst.resource, &info.dst.level, &info.dst.format,
                  &info.dst.box) ||
       !blit_side(ctx, c, VIRGL_CMD_BLIT_SRC_RES_HANDLE, &info.src.resource, &info.src.level, &info.src.format,
                  &info.src.box))
      return false;
   /* Gallium mirrors by the source's sizes only. */
   struct pipe_box *d = &info.dst.box, *s = &info.src.box;
   int32_t dz = d->z, dd = d->depth, sz = s->z, sd = s->depth;
   flip(&d->x, &d->width, &s->x, &s->width);
   flip(&d->y, &d->height, &s->y, &s->height);
   flip(&dz, &dd, &sz, &sd);
   d->z = dz;
   d->depth = dd;
   s->z = sz;
   s->depth = sd;
   /* The source's box, its sizes made positive, lies inside it too. */
   struct pipe_box src = *s;
   if (src.width < 0) {
      src.x += src.width;
      src.width = -src.width;
   }
   if (src.height < 0) {
      src.y += src.height;
      src.height = -src.height;
   }
   if (src.depth < 0) {
      src.z += src.depth;
      src.depth = -src.depth;
   }
   if (!box_inside(info.dst.resource, info.dst.level, d) || !box_inside(info.src.resource, info.src.level, &src))
      return vr_fail(ctx, "a blit outside its images");
   if (!d->width || !d->height || !d->depth || !src.width || !src.height || !src.depth)
      return true;
   const struct util_format_description *dd_desc = util_format_description(info.dst.format);
   const struct util_format_description *sd_desc = util_format_description(info.src.format);
   bool zs = info.mask & PIPE_MASK_ZS;
   if ((info.mask & PIPE_MASK_Z && (!util_format_has_depth(dd_desc) || !util_format_has_depth(sd_desc))) ||
       (info.mask & PIPE_MASK_S && (!util_format_has_stencil(dd_desc) || !util_format_has_stencil(sd_desc))) ||
       (info.mask & PIPE_MASK_RGBA && (util_format_is_depth_or_stencil(info.dst.format) ||
                                       util_format_is_depth_or_stencil(info.src.format))))
      return vr_fail(ctx, "a blit of mask 0x%x between formats %s and %s", info.mask,
                     util_format_name(info.src.format), util_format_name(info.dst.format));
   struct pipe_screen *scr = ctx->dev->screen;
   unsigned ds = info.dst.resource->nr_samples, ss = info.src.resource->nr_samples;
   if (MAX2(ds, 1) > 1 && ds != ss)
      return vr_fail(ctx, "a blit from %u samples to %u", ss, ds);
   if (!zs && (!scr->is_format_supported(scr, info.dst.format, info.dst.resource->target, ds, ds,
                                         PIPE_BIND_RENDER_TARGET) ||
               !scr->is_format_supported(scr, info.src.format, info.src.resource->target, ss, ss,
                                         PIPE_BIND_SAMPLER_VIEW)))
      return vr_fail(ctx, "a blit from %s to %s", util_format_name(info.src.format),
                     util_format_name(info.dst.format));
   ctx->pipe->blit(ctx->pipe, &info);
   return true;
}

static bool
copy_region(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, VIRGL_CMD_RESOURCE_COPY_REGION_SIZE, "a copy"))
      return false;
   uint32_t did = arg(c, VIRGL_CMD_RCR_DST_RES_HANDLE), sid = arg(c, VIRGL_CMD_RCR_SRC_RES_HANDLE);
   struct vr_resource *dr = vr_resource(ctx, did), *sr = vr_resource(ctx, sid);
   if (!dr || !dr->pres || !sr || !sr->pres)
      return vr_fail(ctx, "a copy from resource %u to %u", sid, did);
   struct pipe_resource *dst = dr->pres, *src = sr->pres;
   unsigned dlevel = arg(c, VIRGL_CMD_RCR_DST_LEVEL), slevel = arg(c, VIRGL_CMD_RCR_SRC_LEVEL);
   struct pipe_box sbox, dbox;
   if (!make_box(ctx, arg(c, VIRGL_CMD_RCR_SRC_X), arg(c, VIRGL_CMD_RCR_SRC_Y), arg(c, VIRGL_CMD_RCR_SRC_Z),
                 arg(c, VIRGL_CMD_RCR_SRC_W), arg(c, VIRGL_CMD_RCR_SRC_H), arg(c, VIRGL_CMD_RCR_SRC_D), false, &sbox) ||
       !make_box(ctx, arg(c, VIRGL_CMD_RCR_DST_X), arg(c, VIRGL_CMD_RCR_DST_Y), arg(c, VIRGL_CMD_RCR_DST_Z),
                 arg(c, VIRGL_CMD_RCR_SRC_W), arg(c, VIRGL_CMD_RCR_SRC_H), arg(c, VIRGL_CMD_RCR_SRC_D), false, &dbox))
      return false;
   if ((dst->target == PIPE_BUFFER) != (src->target == PIPE_BUFFER) || !compatible(dst->format, src->format) ||
       MAX2(dst->nr_samples, 1) != MAX2(src->nr_samples, 1) || !box_inside(src, slevel, &sbox) ||
       !box_inside(dst, dlevel, &dbox))
      return vr_fail(ctx, "a copy from resource %u to %u", sid, did);
   if (dst == src && dlevel == slevel && boxes_overlap(&sbox, &dbox))
      return vr_fail(ctx, "a copy onto itself");
   if (sbox.width && sbox.height && sbox.depth)
      ctx->pipe->resource_copy_region(ctx->pipe, dst, dlevel, dbox.x, dbox.y, dbox.z, src, slevel, &sbox);
   return true;
}

/* Copies `box` of level `level` of `r` from (`read`) or to the client's
 * memory at `mem`, rows `stride` and images `layer_stride` bytes apart. */
static bool
transfer_box(struct vr_context *ctx, struct vr_resource *r, unsigned level, const struct pipe_box *box, uint8_t *mem,
             uint64_t stride, uint64_t layer_stride, bool read)
{
   struct pipe_context *pipe = ctx->pipe;
   struct pipe_resource *t = r->pres;
   uint64_t row = util_format_get_stride(t->format, box->width);
   uint64_t rows = util_format_get_nblocksy(t->format, box->height);
   uint64_t size = (uint64_t)(box->depth - 1) * layer_stride + (rows - 1) * stride + row;
   /* Depth kept the other way round from the client's texels (vr_format). */
   bool rotated = t->target != PIPE_BUFFER && vr_format_rotated(ctx->dev, r->format);
   if (read) {
      struct pipe_transfer *x = NULL;
      const uint8_t *map = t->target == PIPE_BUFFER ? pipe->buffer_map(pipe, t, 0, PIPE_MAP_READ, box, &x)
                                                    : pipe->texture_map(pipe, t, level, PIPE_MAP_READ, box, &x);
      if (!map)
         return vr_fail(ctx, "a resource could not be read");
      for (int z = 0; z < box->depth; z++) {
         for (uint64_t y = 0; y < rows; y++)
            memcpy(mem + z * layer_stride + y * stride, map + z * x->layer_stride + y * x->stride, row);
      }
      if (t->target == PIPE_BUFFER)
         pipe->buffer_unmap(pipe, x);
      else
         pipe->texture_unmap(pipe, x);
      if (rotated)
         vr_rotate_depth(mem, box->depth, rows, row, stride, layer_stride, false);
   } else if (t->target == PIPE_BUFFER) {
      pipe->buffer_subdata(pipe, t, PIPE_MAP_WRITE, box->x, box->width, mem);
   } else if (rotated) {
      /* Rotated in a copy: the shared memory is the client's. */
      uint8_t *copy = malloc(size);
      if (!copy)
         return vr_fail(ctx, "out of memory");
      memcpy(copy, mem, size);
      vr_rotate_depth(copy, box->depth, rows, row, stride, layer_stride, true);
      pipe->texture_subdata(pipe, t, level, PIPE_MAP_WRITE, box, copy, stride, layer_stride);
      free(copy);
   } else {
      pipe->texture_subdata(pipe, t, level, PIPE_MAP_WRITE, box, mem, stride, layer_stride);
   }
   return true;
}

/* Copies between a resource and the shared memory, in command order. */
static bool
copy_transfer(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, VIRGL_COPY_TRANSFER3D_SIZE, "a transfer"))
      return false;
   uint32_t id = arg(c, VIRGL_RESOURCE_IW_RES_HANDLE), sid = arg(c, VIRGL_COPY_TRANSFER3D_SRC_RES_HANDLE);
   struct vr_resource *r = vr_resource(ctx, id), *staging = vr_resource(ctx, sid);
   if (!r || !r->pres || !staging)
      return vr_fail(ctx, "a transfer between resources %u and %u", id, sid);
   struct pipe_resource *t = r->pres;
   unsigned level = arg(c, VIRGL_RESOURCE_IW_LEVEL);
   struct pipe_box box;
   if (!make_box(ctx, arg(c, VIRGL_RESOURCE_IW_X), arg(c, VIRGL_RESOURCE_IW_Y), arg(c, VIRGL_RESOURCE_IW_Z),
                 arg(c, VIRGL_RESOURCE_IW_W), arg(c, VIRGL_RESOURCE_IW_H), arg(c, VIRGL_RESOURCE_IW_D), false, &box))
      return false;
   if (!box_inside(t, level, &box) || MAX2(t->nr_samples, 1) > 1)
      return vr_fail(ctx, "a transfer of (%d, %d, %d) %d x %d x %d of level %u of resource %u", box.x, box.y, box.z,
                     box.width, box.height, box.depth, level, id);
   if (!box.width || !box.height || !box.depth)
      return true;
   enum pipe_format f = t->format;
   unsigned bw = util_format_get_blockwidth(f), bh = util_format_get_blockheight(f);
   if (box.x % bw || box.y % bh)
      return vr_fail(ctx, "a transfer between blocks");
   uint64_t row = util_format_get_stride(f, box.width);
   uint64_t rows = util_format_get_nblocksy(f, box.height);
   uint64_t stride = arg(c, VIRGL_RESOURCE_IW_STRIDE) ? arg(c, VIRGL_RESOURCE_IW_STRIDE) : row;
   uint64_t image = (rows - 1) * stride + row;
   uint64_t layer_stride = arg(c, VIRGL_RESOURCE_IW_LAYER_STRIDE) ? arg(c, VIRGL_RESOURCE_IW_LAYER_STRIDE)
                                                                   : stride * rows;
   if (stride < row || (box.depth > 1 && layer_stride < image) || stride > UINT32_MAX)
      return vr_fail(ctx, "a transfer of %llu-byte rows %llu bytes apart", (unsigned long long)row,
                     (unsigned long long)stride);
   uint64_t size = (uint64_t)(box.depth - 1) * layer_stride + image;
   uint8_t *mem = vr_backing(ctx, staging, arg(c, VIRGL_COPY_TRANSFER3D_SRC_RES_OFFSET), size);
   if (!mem)
      return vr_fail(ctx, "a transfer of %llu bytes at %u of resource %u", (unsigned long long)size,
                     arg(c, VIRGL_COPY_TRANSFER3D_SRC_RES_OFFSET), sid);
   /* A cube map's faces one at a time, as Mesa's OpenGL moves them: virgl's
    * host takes no more in one transfer. */
   bool read = arg(c, VIRGL_COPY_TRANSFER3D_FLAGS) & VIRGL_COPY_TRANSFER3D_FLAGS_READ_FROM_HOST;
   if (t->target == PIPE_TEXTURE_CUBE) {
      for (int z = 0; z < box.depth; z++) {
         struct pipe_box face = box;
         face.z += z;
         face.depth = 1;
         if (!transfer_box(ctx, r, level, &face, mem + z * layer_stride, stride, layer_stride, read))
            return false;
      }
      return true;
   }
   return transfer_box(ctx, r, level, &box, mem, stride, layer_stride, read);
}

/* ---- Queries ------------------------------------------------------------ */

static struct vr_query *
query_of(struct vr_context *ctx, const struct cmd *c)
{
   if (!need(ctx, c, 1, "a query"))
      return NULL;
   struct vr_object *o = vr_object_of(ctx, arg(c, 1), VIRGL_OBJECT_QUERY);
   if (!o)
      vr_fail(ctx, "object %u is not a query", arg(c, 1));
   return o ? &o->u.query : NULL;
}

static bool
begin_query(struct vr_context *ctx, const struct cmd *c)
{
   struct vr_query *q = query_of(ctx, c);
   if (!q)
      return false;
   if (q->active)
      return vr_fail(ctx, "a query begun twice");
   if (!ctx->pipe->begin_query(ctx->pipe, q->pq))
      return vr_fail(ctx, "the driver could not begin a query");
   q->active = true;
   return true;
}

static bool
end_query(struct vr_context *ctx, const struct cmd *c)
{
   struct vr_query *q = query_of(ctx, c);
   if (!q)
      return false;
   if (!q->active)
      return vr_fail(ctx, "a query ended that had not begun");
   ctx->pipe->end_query(ctx->pipe, q->pq);
   q->active = false;
   return true;
}

static bool
get_query_result(struct vr_context *ctx, const struct cmd *c)
{
   struct vr_query *q = query_of(ctx, c);
   if (!q || !need(ctx, c, VIRGL_QUERY_RESULT_SIZE, "a query result"))
      return false;
   if (q->active)
      return vr_fail(ctx, "the result of a query still running");
   if (arg(c, VIRGL_QUERY_RESULT_WAIT)) {
      vr_query_write(ctx, q, true);
      return true;
   }
   /* Now if it is in, otherwise once the next fence signals. */
   return vr_query_write(ctx, q, false) || vr_query_wait_later(ctx, arg(c, VIRGL_QUERY_RESULT_HANDLE));
}

/* ---- Commands ----------------------------------------------------------- */

static bool
command(struct vr_context *ctx, uint32_t cmd, uint32_t obj, const struct cmd *c)
{
   switch (cmd) {
   case VIRGL_CCMD_NOP:
      return true;
   case VIRGL_CCMD_CREATE_OBJECT:
      return create_object(ctx, obj, c);
   case VIRGL_CCMD_BIND_OBJECT:
      return bind_object(ctx, obj, c);
   case VIRGL_CCMD_DESTROY_OBJECT:
      if (!need(ctx, c, 1, "a destruction"))
         return false;
      vr_object_destroy(ctx, arg(c, VIRGL_OBJ_DESTROY_HANDLE));
      return true;
   case VIRGL_CCMD_SET_VIEWPORT_STATE:
      return set_viewports(ctx, c);
   case VIRGL_CCMD_SET_FRAMEBUFFER_STATE:
      return set_framebuffer(ctx, c);
   case VIRGL_CCMD_SET_VERTEX_BUFFERS:
      return set_vertex_buffers(ctx, c);
   case VIRGL_CCMD_CLEAR:
      return clear(ctx, c);
   case VIRGL_CCMD_DRAW_VBO:
      return draw_vbo(ctx, c);
   case VIRGL_CCMD_SET_SAMPLER_VIEWS:
      return set_sampler_views(ctx, c);
   case VIRGL_CCMD_SET_INDEX_BUFFER:
      return set_index_buffer(ctx, c);
   case VIRGL_CCMD_SET_CONSTANT_BUFFER:
      return set_constant_buffer(ctx, c);
   case VIRGL_CCMD_SET_STENCIL_REF:
      if (!need(ctx, c, VIRGL_SET_STENCIL_REF_SIZE, "a stencil reference"))
         return false;
      cso_set_stencil_ref(ctx->cso, (struct pipe_stencil_ref){{arg(c, 1) & 0xff, (arg(c, 1) >> 8) & 0xff}});
      return true;
   case VIRGL_CCMD_SET_BLEND_COLOR: {
      if (!need(ctx, c, VIRGL_SET_BLEND_COLOR_SIZE, "a blend color"))
         return false;
      struct pipe_blend_color color;
      for (unsigned i = 0; i < 4; i++)
         color.color[i] = argf(c, VIRGL_SET_BLEND_COLOR(i));
      ctx->pipe->set_blend_color(ctx->pipe, &color);
      return true;
   }
   case VIRGL_CCMD_SET_SCISSOR_STATE:
      return set_scissors(ctx, c);
   case VIRGL_CCMD_BLIT:
      return blit(ctx, c);
   case VIRGL_CCMD_RESOURCE_COPY_REGION:
      return copy_region(ctx, c);
   case VIRGL_CCMD_BIND_SAMPLER_STATES:
      return bind_sampler_states(ctx, c);
   case VIRGL_CCMD_BEGIN_QUERY:
      return begin_query(ctx, c);
   case VIRGL_CCMD_END_QUERY:
      return end_query(ctx, c);
   case VIRGL_CCMD_GET_QUERY_RESULT:
      return get_query_result(ctx, c);
   case VIRGL_CCMD_SET_SAMPLE_MASK:
      if (!need(ctx, c, VIRGL_SET_SAMPLE_MASK_SIZE, "a sample mask"))
         return false;
      cso_set_sample_mask(ctx->cso, arg(c, VIRGL_SET_SAMPLE_MASK_MASK));
      return true;
   case VIRGL_CCMD_SET_STREAMOUT_TARGETS:
      return set_streamout_targets(ctx, c);
   case VIRGL_CCMD_SET_UNIFORM_BUFFER:
      return set_uniform_buffer(ctx, c);
   case VIRGL_CCMD_BIND_SHADER:
      return bind_shader(ctx, c);
   case VIRGL_CCMD_COPY_TRANSFER3D:
      return copy_transfer(ctx, c);
   default:
      return vr_fail(ctx, "command %u", cmd);
   }
}

VR_API int
vr_submit(struct vr_context *ctx, const uint32_t *words, size_t count)
{
   if (ctx->lost)
      return VR_LOST;
   size_t at = 0;
   while (at < count) {
      uint32_t h = words[at];
      struct cmd c = {words + at + 1, h >> 16};
      if (c.len > count - at - 1) {
         vr_fail(ctx, "command %u at word %zu runs past the end", h & 0xff, at);
         return VR_INVALID;
      }
      if (tracing()) {
         fprintf(stderr, "vr %p: command %u, object %u, %u words:", (void *)ctx, h & 0xff, (h >> 8) & 0xff, c.len);
         for (uint32_t i = 0; i < MIN2(c.len, 24); i++)
            fprintf(stderr, " %x", c.w[i]);
         fprintf(stderr, "\n");
         fflush(stderr);
      }
      if (!command(ctx, h & 0xff, (h >> 8) & 0xff, &c)) {
         char why[sizeof(ctx->error)];
         memcpy(why, ctx->error, sizeof(why));
         vr_fail(ctx, "command %u (object %u) at word %zu: %s", h & 0xff, (h >> 8) & 0xff, at, why);
         return VR_INVALID;
      }
      at += 1 + c.len;
   }
   return VR_OK;
}
