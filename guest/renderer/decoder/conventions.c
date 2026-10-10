/*
 * Copyright © 2026 Vahid Mohammadi
 * SPDX-License-Identifier: MIT
 *
 * A fragment shader's inputs, in the driver's conventions.
 *
 * virgl's shaders (as Veda's OpenGL ES writes them, and as virglrenderer
 * and softpipe take them) read the fragment's position at the centre of
 * its pixel (TGSI's default: x.5, y.5), the origin at the top left, and
 * FACE as an input whose x is a float, positive for a front face. A
 * driver may take the position, FACE and the point's coordinate as system
 * values instead, FACE as an integer then (~0 for a front face), and give
 * the position at its pixel's corner (iris does all but the point's
 * coordinate). Mesa's OpenGL state tracker writes such a driver's shaders
 * its way; the renderer adapts virgl's here: such an input becomes the
 * driver's system value, and the shader reads a temporary in its place,
 * made from it as the shader begins (FACE the float it was, the position
 * with the half pixel the driver leaves out).
 */

#include <string.h>

#include "internal.h"

#include "tgsi/tgsi_parse.h"
#include "tgsi/tgsi_scan.h"
#include "tgsi/tgsi_transform.h"

/* The inputs the driver may take as system values. */
enum { POSITION, FACE, POINT, INPUTS };

struct input {
   /* Its register, -1 if the shader has none. */
   int reg;
   /* The driver takes it as a system value: this one. */
   bool sysval;
   unsigned sv;
   /* The shader reads it changed (or as the system value): the temporary
    * it reads in its place. */
   bool changed;
   unsigned temp;
};

struct adapt {
   struct tgsi_transform_context base;
   struct input in[INPUTS];
   /* What the position gains: half a pixel either way, or nothing. */
   float offset;
   /* The pixel centre property the shader declares once adapted (the
    * driver's), and whether it had one. */
   unsigned center;
   bool center_seen;
   /* The immediate {offset, 1, -1, 0}. */
   unsigned imm;
};

static const unsigned semantic[INPUTS] = {TGSI_SEMANTIC_POSITION, TGSI_SEMANTIC_FACE, TGSI_SEMANTIC_PCOORD};

static struct adapt *
adapt(struct tgsi_transform_context *ctx)
{
   return (struct adapt *)ctx;
}

/* Declares what `decl` declares, but for the registers from `first` to
 * `last` (if any). */
static void
declare_part(struct tgsi_transform_context *ctx, const struct tgsi_full_declaration *decl, int first, int last)
{
   if (first > last)
      return;
   struct tgsi_full_declaration part = *decl;
   part.Range.First = first;
   part.Range.Last = last;
   ctx->emit_declaration(ctx, &part);
}

/* An input the driver takes as a system value is declared as that. */
static void
transform_declaration(struct tgsi_transform_context *ctx, struct tgsi_full_declaration *decl)
{
   struct adapt *a = adapt(ctx);
   if (decl->Declaration.File != TGSI_FILE_INPUT) {
      ctx->emit_declaration(ctx, decl);
      return;
   }
   int from = decl->Range.First;
   for (int r = decl->Range.First; r <= decl->Range.Last; r++) {
      for (unsigned i = 0; i < INPUTS; i++) {
         if (a->in[i].reg != r || !a->in[i].sysval)
            continue;
         declare_part(ctx, decl, from, r - 1);
         struct tgsi_full_declaration sv = tgsi_default_full_declaration();
         sv.Declaration.File = TGSI_FILE_SYSTEM_VALUE;
         sv.Declaration.Semantic = 1;
         sv.Semantic.Name = semantic[i];
         sv.Range.First = sv.Range.Last = a->in[i].sv;
         ctx->emit_declaration(ctx, &sv);
         from = r + 1;
      }
   }
   declare_part(ctx, decl, from, decl->Range.Last);
}

static void
transform_property(struct tgsi_transform_context *ctx, struct tgsi_full_property *prop)
{
   struct adapt *a = adapt(ctx);
   if (prop->Property.PropertyName == TGSI_PROPERTY_FS_COORD_PIXEL_CENTER && a->offset != 0.0f) {
      prop->u[0].Data = a->center;
      a->center_seen = true;
   }
   ctx->emit_property(ctx, prop);
}

/* A source: register `index` of `file`, its components as `x`, `y`, `z`
 * and `w` pick them. */
static struct tgsi_full_src_register
source(unsigned file, unsigned index, unsigned x, unsigned y, unsigned z, unsigned w)
{
   struct tgsi_full_src_register reg;
   memset(&reg, 0, sizeof(reg));
   tgsi_transform_src_reg(&reg, file, index, x, y, z, w);
   return reg;
}

static void
emit(struct tgsi_transform_context *ctx, unsigned opcode, unsigned temp, unsigned mask,
     const struct tgsi_full_src_register *src, unsigned n)
{
   struct tgsi_full_instruction inst = tgsi_default_full_instruction();
   inst.Instruction.Opcode = opcode;
   inst.Instruction.NumDstRegs = 1;
   tgsi_transform_dst_reg(&inst.Dst[0], TGSI_FILE_TEMPORARY, temp, mask);
   inst.Instruction.NumSrcRegs = n;
   for (unsigned i = 0; i < n; i++)
      inst.Src[i] = src[i];
   ctx->emit_instruction(ctx, &inst);
}

/* The changed inputs' temporaries, made as the shader begins. */
static void
prolog(struct tgsi_transform_context *ctx)
{
   struct adapt *a = adapt(ctx);
   if (a->offset != 0.0f && !a->center_seen) {
      struct tgsi_full_property prop = tgsi_default_full_property();
      prop.Property.PropertyName = TGSI_PROPERTY_FS_COORD_PIXEL_CENTER;
      prop.Property.NrTokens += 1;
      prop.u[0].Data = a->center;
      ctx->emit_property(ctx, &prop);
   }
   tgsi_transform_immediate_decl(ctx, a->offset, 1.0f, -1.0f, 0.0f);
   for (unsigned i = 0; i < INPUTS; i++) {
      if (a->in[i].changed)
         tgsi_transform_temp_decl(ctx, a->in[i].temp);
   }
   for (unsigned i = 0; i < INPUTS; i++) {
      const struct input *in = &a->in[i];
      if (!in->changed)
         continue;
      unsigned file = in->sysval ? TGSI_FILE_SYSTEM_VALUE : TGSI_FILE_INPUT;
      unsigned index = in->sysval ? in->sv : (unsigned)in->reg;
      const struct tgsi_full_src_register value =
         source(file, index, TGSI_SWIZZLE_X, TGSI_SWIZZLE_Y, TGSI_SWIZZLE_Z, TGSI_SWIZZLE_W);
      struct tgsi_full_src_register imm[4];
      for (unsigned c = 0; c < 4; c++)
         imm[c] = source(TGSI_FILE_IMMEDIATE, a->imm, c, c, c, c);
      if (i == FACE) {
         /* (1.0 or -1.0, 0, 0, 1), as FACE was. */
         const struct tgsi_full_src_register face =
            source(file, index, TGSI_SWIZZLE_X, TGSI_SWIZZLE_X, TGSI_SWIZZLE_X, TGSI_SWIZZLE_X);
         emit(ctx, TGSI_OPCODE_UCMP, in->temp, TGSI_WRITEMASK_X, (struct tgsi_full_src_register[]){face, imm[1], imm[2]},
              3);
         const struct tgsi_full_src_register rest =
            source(TGSI_FILE_IMMEDIATE, a->imm, TGSI_SWIZZLE_W, TGSI_SWIZZLE_W, TGSI_SWIZZLE_W, TGSI_SWIZZLE_Y);
         emit(ctx, TGSI_OPCODE_MOV, in->temp, TGSI_WRITEMASK_YZW, &rest, 1);
      } else if (i == POSITION && a->offset != 0.0f) {
         emit(ctx, TGSI_OPCODE_ADD, in->temp, TGSI_WRITEMASK_XY, (struct tgsi_full_src_register[]){value, imm[0]}, 2);
         emit(ctx, TGSI_OPCODE_MOV, in->temp, TGSI_WRITEMASK_ZW, &value, 1);
      } else {
         emit(ctx, TGSI_OPCODE_MOV, in->temp, TGSI_WRITEMASK_XYZW, &value, 1);
      }
   }
}

/* The shader reads a changed input's temporary in its place. */
static void
transform_instruction(struct tgsi_transform_context *ctx, struct tgsi_full_instruction *inst)
{
   struct adapt *a = adapt(ctx);
   for (unsigned s = 0; s < inst->Instruction.NumSrcRegs; s++) {
      struct tgsi_full_src_register *src = &inst->Src[s];
      if (src->Register.File != TGSI_FILE_INPUT || src->Register.Indirect)
         continue;
      for (unsigned i = 0; i < INPUTS; i++) {
         if (a->in[i].changed && a->in[i].reg == src->Register.Index) {
            src->Register.File = TGSI_FILE_TEMPORARY;
            src->Register.Index = a->in[i].temp;
         }
      }
   }
   ctx->emit_instruction(ctx, inst);
}

/* The registers of the inputs the shader declares (-1 for those it does
 * not). */
static void
find_inputs(const struct tgsi_token *tokens, struct adapt *a)
{
   for (unsigned i = 0; i < INPUTS; i++)
      a->in[i].reg = -1;
   struct tgsi_parse_context p;
   if (tgsi_parse_init(&p, tokens) != TGSI_PARSE_OK)
      return;
   while (!tgsi_parse_end_of_tokens(&p)) {
      tgsi_parse_token(&p);
      const struct tgsi_full_declaration *d = &p.FullToken.FullDeclaration;
      if (p.FullToken.Token.Type != TGSI_TOKEN_TYPE_DECLARATION || d->Declaration.File != TGSI_FILE_INPUT ||
          !d->Declaration.Semantic)
         continue;
      for (unsigned i = 0; i < INPUTS; i++) {
         if (d->Semantic.Name == semantic[i])
            a->in[i].reg = d->Range.First;
      }
   }
   tgsi_parse_free(&p);
}

bool
vr_adapt_fragment_inputs(const struct tgsi_token *tokens, struct pipe_screen *screen, struct tgsi_token **out)
{
   *out = NULL;
   struct tgsi_shader_info info;
   tgsi_scan_shader(tokens, &info);
   if (info.processor != MESA_SHADER_FRAGMENT)
      return true;
   const struct pipe_caps *caps = &screen->caps;
   struct adapt a;
   memset(&a, 0, sizeof(a));
   const bool sysval[INPUTS] = {caps->fs_position_is_sysval, caps->fs_face_is_integer_sysval,
                                caps->fs_point_is_sysval};
   unsigned next_sv = info.file_max[TGSI_FILE_SYSTEM_VALUE] + 1;
   unsigned next_temp = info.file_max[TGSI_FILE_TEMPORARY] + 1;
   find_inputs(tokens, &a);
   for (unsigned i = 0; i < INPUTS; i++) {
      if (a.in[i].reg >= 0 && sysval[i]) {
         a.in[i].sysval = true;
         a.in[i].sv = next_sv++;
      }
   }
   /* Where the position is: the origin at the top left (the driver's
    * other origin would need the framebuffer's height), its centre where
    * the shader asks. */
   if (a.in[POSITION].reg >= 0) {
      if (info.properties[TGSI_PROPERTY_FS_COORD_ORIGIN] == TGSI_FS_COORD_ORIGIN_LOWER_LEFT &&
          !caps->fs_coord_origin_lower_left)
         return false;
      bool integer = info.properties[TGSI_PROPERTY_FS_COORD_PIXEL_CENTER] == TGSI_FS_COORD_PIXEL_CENTER_INTEGER;
      if (!integer && !caps->fs_coord_pixel_center_half_integer) {
         a.offset = 0.5f;
         a.center = TGSI_FS_COORD_PIXEL_CENTER_INTEGER;
      } else if (integer && !caps->fs_coord_pixel_center_integer) {
         a.offset = -0.5f;
         a.center = TGSI_FS_COORD_PIXEL_CENTER_HALF_INTEGER;
      }
   }
   bool changes = false;
   for (unsigned i = 0; i < INPUTS; i++) {
      a.in[i].changed = a.in[i].sysval || (i == POSITION && a.in[i].reg >= 0 && a.offset != 0.0f);
      if (a.in[i].changed) {
         a.in[i].temp = next_temp++;
         changes = true;
      }
   }
   if (!changes)
      return true;
   a.imm = info.immediate_count;
   a.base.transform_declaration = transform_declaration;
   a.base.transform_property = transform_property;
   a.base.transform_instruction = transform_instruction;
   a.base.prolog = prolog;
   *out = tgsi_transform_shader(tokens, tgsi_num_tokens(tokens) + 256, &a.base);
   return *out != NULL;
}
