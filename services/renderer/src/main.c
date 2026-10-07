/*
 * Copyright © 2026 Vahid Mohammadi
 * SPDX-License-Identifier: MIT
 *
 * Veda's renderer service: serves the `gpu` protocol (lib/proto/src/gpu.rs)
 * as the virtio-gpu driver does under QEMU, with the commands carried out
 * here, on a Gallium driver, rather than by the host. Applications'
 * OpenGL ES (vgl) cannot tell the two apart.
 *
 * Each connection gets a context and a block of memory both sides map: a
 * page whose first word is the number of the last fence that signaled,
 * the command area submissions are read from, and the shared area that
 * resources may use as their storage (vgl's staging and query results).
 * Commands are copied out of the command area before they are decoded,
 * so a client changing them meanwhile cannot get them past the checks.
 *
 * `renderer [softpipe | iris] [name=SERVICE] [trace | trace=commands]`: the device
 * to render on, the name to serve under (`gpu`), and what to log.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <veda/ipc.h>

#include "renderer.h"

#define SERVICE "gpu"
/* The name served under (`name=...`; tests run a renderer of their own). */
static const char *service = SERVICE;
#define HEADER_BYTES 4096u
#define COMMAND_BYTES (1024u * 1024u)
#define MIN_SHARED (1024u * 1024u)
#define MAX_SHARED (64u * 1024u * 1024u)
#define MAX_CLIENTS 64
/* How often signaling fences are looked for while some are pending. */
#define POLL_NS 1000000ull
/* How long the GPU's driver is waited for: tries, and the time between. */
#define OPEN_TRIES 1000
#define OPEN_RETRY_NS 10000000l

/* `gpu` methods and errors. */
enum { OPEN = 1, CREATE = 2, DESTROY = 3, SUBMIT = 4, FENCE = 5 };
enum { E_UNAVAILABLE = 1, E_NO_MEMORY = 2, E_INVALID = 3, E_STATE = 4 };

struct client {
   veda_handle_t channel;
   struct vr_context *ctx;
   veda_handle_t memory, fences;
   uint8_t *map;
   size_t size;
   /* The last fence queued, and the last written to the fence word. */
   uint64_t queued, signaled;
};

static struct vr_device *device;
static uint8_t caps[4096];
static size_t caps_len;
static struct client clients[MAX_CLIENTS];
static unsigned num_clients;
/* Commands, copied out of a client's command area. */
static uint32_t commands[COMMAND_BYTES / 4];
/* `trace`: a line for every submission; `trace=commands`: for every
 * command too (the decoder's VR_TRACE). */
static int trace;

/* ---- Messages ------------------------------------------------------------ */

struct writer {
   uint8_t bytes[8192];
   size_t len;
   veda_handle_t handles[4];
   size_t count;
};

static void
put(struct writer *w, const void *p, size_t n)
{
   if (w->len + n <= sizeof(w->bytes)) {
      memcpy(w->bytes + w->len, p, n);
      w->len += n;
   }
}

static void
put_u8(struct writer *w, uint8_t v)
{
   put(w, &v, 1);
}

static void
put_u32(struct writer *w, uint32_t v)
{
   put(w, &v, 4);
}

static void
put_u64(struct writer *w, uint64_t v)
{
   put(w, &v, 8);
}

static void
put_bytes(struct writer *w, const void *p, uint32_t n)
{
   put_u32(w, n);
   put(w, p, n);
}

static void
put_handle(struct writer *w, veda_handle_t h)
{
   put_u32(w, w->count);
   w->handles[w->count++] = h;
}

struct reader {
   const uint8_t *p;
   size_t left;
   int bad;
};

static uint32_t
get_u32(struct reader *r)
{
   uint32_t v = 0;
   if (r->left < 4) {
      r->bad = 1;
      return 0;
   }
   memcpy(&v, r->p, 4);
   r->p += 4;
   r->left -= 4;
   return v;
}

static uint64_t
get_u64(struct reader *r)
{
   uint64_t lo = get_u32(r);
   return lo | (uint64_t)get_u32(r) << 32;
}

/* ---- Fences -------------------------------------------------------------- */

/* Writes the number of the last fence that signaled, and tells the client
 * when it has moved. */
static void
publish_fences(struct client *c)
{
   uint64_t done = vr_fence_signaled(c->ctx);
   if (done <= c->signaled)
      return;
   c->signaled = done;
   __atomic_store_n((uint64_t *)c->map, done, __ATOMIC_RELEASE);
   veda_object_signal(c->fences, 0, VEDA_SIGNALED);
}

/* ---- Requests ------------------------------------------------------------ */

static void
close_session(struct client *c)
{
   if (c->ctx)
      vr_context_destroy(c->ctx);
   if (c->map)
      veda_vmo_unmap(c->map, c->size);
   if (c->memory)
      veda_close(c->memory);
   if (c->fences)
      veda_close(c->fences);
   c->ctx = NULL;
   c->map = NULL;
   c->memory = c->fences = 0;
}

static void
open_session(struct client *c, struct reader *r, struct writer *w)
{
   uint64_t want = get_u64(r);
   if (r->bad || c->ctx) {
      put_u8(w, 1);
      put_u32(w, r->bad ? E_INVALID : E_STATE);
      return;
   }
   uint64_t shared = want < MIN_SHARED ? MIN_SHARED : want > MAX_SHARED ? MAX_SHARED : want;
   shared = (shared + 4095) & ~4095ull;
   c->size = HEADER_BYTES + COMMAND_BYTES + shared;
   veda_handle_t theirs = 0, their_fences = 0;
   void *map = NULL;
   if (veda_vmo_create(c->size, &c->memory) < 0 ||
       veda_vmo_map(c->memory, 0, c->size, VEDA_MAP_READ | VEDA_MAP_WRITE, &map) < 0 ||
       veda_event_create(&c->fences) < 0 ||
       veda_duplicate(c->memory,
                      VEDA_RIGHT_TRANSFER | VEDA_RIGHT_READ | VEDA_RIGHT_WRITE | VEDA_RIGHT_MAP | VEDA_RIGHT_GET_INFO,
                      &theirs) < 0 ||
       veda_duplicate(c->fences, VEDA_RIGHT_TRANSFER | VEDA_RIGHT_WAIT | VEDA_RIGHT_SIGNAL, &their_fences) < 0) {
      if (theirs)
         veda_close(theirs);
      c->map = map;
      close_session(c);
      put_u8(w, 1);
      put_u32(w, E_NO_MEMORY);
      return;
   }
   c->map = map;
   c->ctx = vr_context_create(device, c->map + HEADER_BYTES + COMMAND_BYTES, shared);
   if (!c->ctx) {
      veda_close(theirs);
      veda_close(their_fences);
      close_session(c);
      put_u8(w, 1);
      put_u32(w, E_UNAVAILABLE);
      return;
   }
   c->queued = c->signaled = 0;
   const char *name = vr_device_name(device);
   put_u8(w, 0);
   put_bytes(w, caps, caps_len);
   put_handle(w, theirs);
   put_u64(w, HEADER_BYTES);
   put_u64(w, COMMAND_BYTES);
   put_u64(w, HEADER_BYTES + COMMAND_BYTES);
   put_u64(w, shared);
   put_u64(w, 0);
   put_handle(w, their_fences);
   put_bytes(w, name, strlen(name));
}

static uint32_t
error_of(int r)
{
   return r == VR_NO_MEMORY ? E_NO_MEMORY : r == VR_LOST ? E_UNAVAILABLE : E_INVALID;
}

static void
create_resource(struct client *c, struct reader *r, struct writer *w)
{
   struct vr_resource_args a;
   uint32_t *f = (uint32_t *)&a;
   for (unsigned i = 0; i < sizeof(a) / 4; i++)
      f[i] = get_u32(r);
   uint64_t offset = get_u64(r), len = get_u64(r);
   uint32_t id = 0;
   int e = r->bad ? VR_INVALID : !c->ctx ? -100 : vr_resource_create(c->ctx, &a, offset, len, &id);
   if (e) {
      put_u8(w, 1);
      put_u32(w, e == -100 ? E_STATE : error_of(e));
      if (e != -100)
         fprintf(stderr, "a resource refused: %s\n", vr_context_error(c->ctx));
      return;
   }
   put_u8(w, 0);
   put_u32(w, id);
}

static void
submit(struct client *c, struct reader *r, struct writer *w)
{
   uint32_t words = get_u32(r);
   if (r->bad || !c->ctx || (uint64_t)words * 4 > COMMAND_BYTES) {
      put_u8(w, 1);
      put_u32(w, !c->ctx ? E_STATE : E_INVALID);
      return;
   }
   memcpy(commands, c->map + HEADER_BYTES, (size_t)words * 4);
   if (trace)
      fprintf(stderr, "submission of %u words: %08x %08x %08x %08x\n", words, words > 0 ? commands[0] : 0,
              words > 1 ? commands[1] : 0, words > 2 ? commands[2] : 0, words > 3 ? commands[3] : 0);
   int e = vr_submit(c->ctx, commands, words);
   if (e) {
      fprintf(stderr, "commands refused: %s\n", vr_context_error(c->ctx));
      put_u8(w, 1);
      put_u32(w, error_of(e));
      return;
   }
   put_u8(w, 0);
}

static void
fence(struct client *c, struct writer *w)
{
   uint64_t seq = 0;
   int e = c->ctx ? vr_fence(c->ctx, &seq) : -100;
   if (e) {
      put_u8(w, 1);
      put_u32(w, e == -100 ? E_STATE : error_of(e));
      return;
   }
   c->queued = seq;
   put_u8(w, 0);
   put_u64(w, seq);
   publish_fences(c);
}

/* Carries out one request; returns 0 if the client is to be dropped. */
static int
serve(struct client *c, const uint8_t *msg, size_t len)
{
   struct reader r = {msg + VEDA_MSG_HEADER, len >= VEDA_MSG_HEADER ? len - VEDA_MSG_HEADER : 0, 0};
   uint32_t head[3] = {0};
   if (len < VEDA_MSG_HEADER)
      return 0;
   memcpy(head, msg, sizeof(head));
   if (head[2] != VEDA_MSG_REQUEST)
      return 0;
   static struct writer w;
   w.len = w.count = 0;
   put_u32(&w, head[0]);
   put_u32(&w, head[1]);
   put_u32(&w, VEDA_MSG_RESPONSE);
   switch (head[0]) {
   case OPEN:
      open_session(c, &r, &w);
      break;
   case CREATE:
      create_resource(c, &r, &w);
      break;
   case DESTROY: {
      uint32_t id = get_u32(&r);
      if (!r.bad && c->ctx)
         vr_resource_destroy(c->ctx, id);
      break;
   }
   case SUBMIT:
      submit(c, &r, &w);
      break;
   case FENCE:
      fence(c, &w);
      break;
   default:
      fprintf(stderr, "unknown request %u\n", head[0]);
      return 0;
   }
   return veda_channel_write(c->channel, w.bytes, w.len, w.handles, w.count) == 0;
}

static void
drop_client(unsigned i)
{
   close_session(&clients[i]);
   veda_close(clients[i].channel);
   clients[i] = clients[--num_clients];
   memset(&clients[num_clients], 0, sizeof(clients[num_clients]));
}

/* Carries out what a client has sent; returns 0 if it is to be dropped. */
static int
read_client(struct client *c)
{
   static uint8_t msg[256];
   veda_handle_t handles[4];
   for (;;) {
      size_t len = 0, count = 0;
      int r = veda_channel_read(c->channel, msg, sizeof(msg), &len, handles, 4, &count);
      if (r == -VEDA_ESHOULD_WAIT)
         return 1;
      if (r < 0)
         return 0;
      /* No request of `gpu` carries handles. */
      for (size_t i = 0; i < count; i++)
         veda_close(handles[i]);
      if (!serve(c, msg, len))
         return 0;
   }
}

int
main(int argc, char **argv)
{
   const char *driver = argc > 1 ? argv[1] : "softpipe";
   for (int i = 2; i < argc; i++) {
      if (strcmp(argv[i], "trace") == 0) {
         trace = 1;
      } else if (strcmp(argv[i], "trace=commands") == 0) {
         trace = 1;
         setenv("VR_TRACE", "1", 1);
      } else if (strncmp(argv[i], "name=", 5) == 0 && argv[i][5]) {
         service = argv[i] + 5;
      }
   }
   if (strcmp(driver, "softpipe") == 0) {
      device = vr_device_create_softpipe();
   } else if (strcmp(driver, "iris") == 0) {
      /* The GPU's DRM device: Intel's driver, behind the POSIX layer. The
       * driver may still be starting (no device yet): waited for a while. */
      int fd = -1;
      for (int tries = 0; tries < OPEN_TRIES; tries++) {
         fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
         if (fd >= 0 || errno != ENOENT)
            break;
         nanosleep(&(struct timespec){0, OPEN_RETRY_NS}, NULL);
      }
      if (fd < 0) {
         fprintf(stderr, "no DRM device (%s)\n", strerror(errno));
         return 1;
      }
      device = vr_device_create_iris(fd);
   } else {
      fprintf(stderr, "no driver %s\n", driver);
      return 1;
   }
   if (!device) {
      fprintf(stderr, "%s did not start\n", driver);
      return 1;
   }
   caps_len = vr_device_caps(device, caps, sizeof(caps));
   veda_handle_t listener;
   int e = veda_service_register(service, &listener);
   if (e < 0) {
      fprintf(stderr, "cannot register %s (%d)\n", service, e);
      return 1;
   }
   printf("serving %s on %s\n", service, vr_device_name(device));
   fflush(stdout);

   struct veda_wait_item items[MAX_CLIENTS + 1];
   for (;;) {
      unsigned n = 0, pending = 0;
      items[n++] = (struct veda_wait_item){listener, VEDA_READABLE | VEDA_PEER_CLOSED, 0, 0};
      for (unsigned i = 0; i < num_clients; i++) {
         items[n++] = (struct veda_wait_item){clients[i].channel, VEDA_READABLE | VEDA_PEER_CLOSED, 0, 0};
         pending |= clients[i].ctx && clients[i].signaled < clients[i].queued;
      }
      veda_wait(items, n, pending ? veda_now_ns() + POLL_NS : VEDA_FOREVER);
      for (unsigned i = 0; i < num_clients; i++) {
         if (clients[i].ctx && clients[i].signaled < clients[i].queued)
            publish_fences(&clients[i]);
      }
      /* Clients first, last to first (dropping one moves the last into
       * its place), then new connections. What a closing client sent
       * before it closed is carried out. */
      for (unsigned k = n - 1; k >= 1; k--) {
         uint32_t seen = items[k].observed;
         if (!seen)
            continue;
         int keep = !(seen & VEDA_READABLE) || read_client(&clients[k - 1]);
         if (!keep || (seen & VEDA_PEER_CLOSED))
            drop_client(k - 1);
      }
      if (items[0].observed & VEDA_READABLE) {
         veda_handle_t ch;
         while (veda_service_accept(listener, &ch) == 0) {
            if (num_clients == MAX_CLIENTS) {
               veda_close(ch);
               continue;
            }
            clients[num_clients++] = (struct client){.channel = ch};
         }
      }
   }
}
