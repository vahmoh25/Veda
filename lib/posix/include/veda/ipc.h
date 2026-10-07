/*
 * <veda/ipc.h> - Veda's own interfaces, for C programs that are Veda
 * services or talk to them directly: handles, channels and the service
 * registry, waiting, memory objects and events.
 *
 * They are the kernel's calls (but for the registry's), made by the POSIX
 * layer (lib/posix/src/native.rs). Every call returns 0, or a count, when
 * it succeeds, and a negative Veda error number when it fails. Handles are
 * the kernel's; file descriptors know nothing of them.
 *
 * Messages in Veda's protocols start with a 12-byte header (the method's
 * number, a transaction number, VEDA_MSG_REQUEST or VEDA_MSG_RESPONSE),
 * then the values in order: numbers little-endian, strings and byte
 * strings as a 32-bit length and the bytes, handles as their index in the
 * message's handle list, results as a byte (0: success) and the value.
 */

#ifndef _VEDA_IPC_H
#define _VEDA_IPC_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef uint32_t veda_handle_t;

#define VEDA_INVALID_HANDLE 0u

/* Errors (negated). */
#define VEDA_EINVALID_ARGS 1
#define VEDA_EBAD_HANDLE 2
#define VEDA_EWRONG_TYPE 3
#define VEDA_EACCESS_DENIED 4
#define VEDA_ENO_MEMORY 5
#define VEDA_ENOT_FOUND 6
#define VEDA_ESHOULD_WAIT 7
#define VEDA_ETIMED_OUT 8
#define VEDA_EPEER_CLOSED 9
#define VEDA_EBUFFER_TOO_SMALL 10
#define VEDA_EOUT_OF_RANGE 11
#define VEDA_ENOT_SUPPORTED 13
#define VEDA_ELIMIT_REACHED 16

/* Signals. */
#define VEDA_READABLE (1u << 0)
#define VEDA_WRITABLE (1u << 1)
#define VEDA_PEER_CLOSED (1u << 2)
#define VEDA_SIGNALED (1u << 3)
#define VEDA_TERMINATED (1u << 4)

/* Rights, for veda_duplicate. */
#define VEDA_RIGHT_DUPLICATE (1u << 0)
#define VEDA_RIGHT_TRANSFER (1u << 1)
#define VEDA_RIGHT_READ (1u << 2)
#define VEDA_RIGHT_WRITE (1u << 3)
#define VEDA_RIGHT_MAP (1u << 5)
#define VEDA_RIGHT_GET_INFO (1u << 6)
#define VEDA_RIGHT_SIGNAL (1u << 7)
#define VEDA_RIGHT_WAIT (1u << 8)
#define VEDA_RIGHTS_SAME (~0u)

/* Mappings. */
#define VEDA_MAP_READ (1u << 0)
#define VEDA_MAP_WRITE (1u << 1)

/* Deadlines are veda_now_ns's clock; this one never comes. */
#define VEDA_FOREVER (~(uint64_t)0)

/* Message headers. */
#define VEDA_MSG_HEADER 12
#define VEDA_MSG_REQUEST 1u
#define VEDA_MSG_RESPONSE 2u
#define VEDA_MSG_EVENT 4u

struct veda_wait_item {
   veda_handle_t handle;
   /* The signals waited for, and those active when the wait returned. */
   uint32_t signals;
   uint32_t observed;
   uint32_t reserved;
};

int veda_close(veda_handle_t handle);
int veda_duplicate(veda_handle_t handle, uint32_t rights, veda_handle_t *out);

/* The registry. A program registers itself as the provider of a service;
 * connections to it arrive on the listener (readable when one waits). */
int veda_service_register(const char *name, veda_handle_t *listener);
/* Fails with VEDA_ESHOULD_WAIT if no connection waits. */
int veda_service_accept(veda_handle_t listener, veda_handle_t *channel);
int veda_service_connect(const char *name, veda_handle_t *channel);

/* Channels. Handles written move to the receiver (closed if the write
 * fails). A read takes one message without waiting: *len and *count get
 * its size; if it is larger than the buffers, the read fails with
 * VEDA_EBUFFER_TOO_SMALL and the message stays. */
int veda_channel_write(veda_handle_t channel, const void *bytes, size_t len, const veda_handle_t *handles,
                       size_t count);
int veda_channel_read(veda_handle_t channel, void *bytes, size_t cap, size_t *len, veda_handle_t *handles,
                      size_t hcap, size_t *count);

/* Waits until an item has one of its signals, or the deadline passes;
 * returns how many are ready (0: the deadline passed). */
int veda_wait(struct veda_wait_item *items, size_t count, uint64_t deadline_ns);
uint64_t veda_now_ns(void);

/* Memory objects. */
int veda_vmo_create(uint64_t size, veda_handle_t *vmo);
int veda_vmo_map(veda_handle_t vmo, uint64_t offset, uint64_t size, uint32_t flags, void **addr);
int veda_vmo_unmap(void *addr, uint64_t size);

/* Events, and the signals of objects. */
int veda_event_create(veda_handle_t *event);
int veda_object_signal(veda_handle_t handle, uint32_t clear, uint32_t set);

#ifdef __cplusplus
}
#endif

#endif
