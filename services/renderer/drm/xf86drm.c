/*
 * Copyright © 2026 Vahid Mohammadi
 * SPDX-License-Identifier: MIT
 *
 * libdrm's functions that Mesa uses, for Veda (see xf86drm.h): each is the
 * kernel interface's ioctl on the DRM device, as libdrm makes it, and its
 * results as libdrm gives them (0 or -errno, or -1 and errno, as each of
 * libdrm's does).
 */

#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include "xf86drm.h"

int
drmIoctl(int fd, unsigned long request, void *arg)
{
   int r;
   do {
      r = ioctl(fd, request, arg);
   } while (r == -1 && (errno == EINTR || errno == EAGAIN));
   return r;
}

int
drmGetCap(int fd, uint64_t capability, uint64_t *value)
{
   struct drm_get_cap cap = {.capability = capability};
   int r = drmIoctl(fd, DRM_IOCTL_GET_CAP, &cap);
   if (r)
      return r;
   *value = cap.value;
   return 0;
}

int
drmCommandNone(int fd, unsigned long index)
{
   return drmIoctl(fd, DRM_IO(DRM_COMMAND_BASE + index), NULL) ? -errno : 0;
}

int
drmCommandRead(int fd, unsigned long index, void *data, unsigned long size)
{
   return drmIoctl(fd, DRM_IOC(DRM_IOC_READ, DRM_IOCTL_BASE, DRM_COMMAND_BASE + index, size), data) ? -errno : 0;
}

int
drmCommandWrite(int fd, unsigned long index, void *data, unsigned long size)
{
   return drmIoctl(fd, DRM_IOC(DRM_IOC_WRITE, DRM_IOCTL_BASE, DRM_COMMAND_BASE + index, size), data) ? -errno : 0;
}

int
drmCommandWriteRead(int fd, unsigned long index, void *data, unsigned long size)
{
   return drmIoctl(fd, DRM_IOC(DRM_IOC_READ | DRM_IOC_WRITE, DRM_IOCTL_BASE, DRM_COMMAND_BASE + index, size), data)
             ? -errno
             : 0;
}

drmVersionPtr
drmGetVersion(int fd)
{
   struct drm_version v;
   memset(&v, 0, sizeof(v));
   if (drmIoctl(fd, DRM_IOCTL_VERSION, &v))
      return NULL;
   drmVersionPtr out = calloc(1, sizeof(*out));
   char *name = calloc(1, v.name_len + 1), *date = calloc(1, v.date_len + 1), *desc = calloc(1, v.desc_len + 1);
   if (!out || !name || !date || !desc)
      goto fail;
   v.name = name;
   v.date = date;
   v.desc = desc;
   if (drmIoctl(fd, DRM_IOCTL_VERSION, &v))
      goto fail;
   out->version_major = v.version_major;
   out->version_minor = v.version_minor;
   out->version_patchlevel = v.version_patchlevel;
   out->name_len = v.name_len;
   out->name = name;
   out->date_len = v.date_len;
   out->date = date;
   out->desc_len = v.desc_len;
   out->desc = desc;
   return out;
fail:
   free(out);
   free(name);
   free(date);
   free(desc);
   return NULL;
}

void
drmFreeVersion(drmVersionPtr version)
{
   if (!version)
      return;
   free(version->name);
   free(version->date);
   free(version->desc);
   free(version);
}

int
drmGetMagic(int fd, drm_magic_t *magic)
{
   struct drm_auth auth = {0};
   *magic = 0;
   if (drmIoctl(fd, DRM_IOCTL_GET_MAGIC, &auth))
      return -errno;
   *magic = auth.magic;
   return 0;
}

int
drmPrimeHandleToFD(int fd, uint32_t handle, uint32_t flags, int *prime_fd)
{
   struct drm_prime_handle args = {.handle = handle, .flags = flags, .fd = -1};
   int r = drmIoctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &args);
   if (r)
      return r;
   *prime_fd = args.fd;
   return 0;
}

int
drmPrimeFDToHandle(int fd, int prime_fd, uint32_t *handle)
{
   struct drm_prime_handle args = {.fd = prime_fd};
   int r = drmIoctl(fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &args);
   if (r)
      return r;
   *handle = args.handle;
   return 0;
}

/* ---- The device ---------------------------------------------------------- */

/* A device description in one allocation: the drmDevice, its node names
 * and its PCI information. */
struct device {
   drmDevice d;
   char *nodes[DRM_NODE_MAX];
   char render[sizeof(DRM_VEDA_RENDER_NODE)];
   drmPciBusInfo bus;
   drmPciDeviceInfo info;
};

int
drmGetDevice2(int fd, uint32_t flags, drmDevicePtr *device)
{
   (void)flags;
   struct drm_veda_pci_info pci;
   memset(&pci, 0, sizeof(pci));
   if (drmIoctl(fd, DRM_IOCTL_VEDA_PCI_INFO, &pci))
      return -errno;
   struct device *dev = calloc(1, sizeof(*dev));
   if (!dev)
      return -ENOMEM;
   memcpy(dev->render, DRM_VEDA_RENDER_NODE, sizeof(dev->render));
   dev->nodes[DRM_NODE_RENDER] = dev->render;
   dev->d.nodes = dev->nodes;
   dev->d.available_nodes = 1 << DRM_NODE_RENDER;
   dev->d.bustype = DRM_BUS_PCI;
   dev->bus = (drmPciBusInfo){pci.domain, pci.bus, pci.dev, pci.func};
   dev->info = (drmPciDeviceInfo){pci.vendor_id, pci.device_id, pci.subvendor_id, pci.subdevice_id, pci.revision_id};
   dev->d.businfo.pci = &dev->bus;
   dev->d.deviceinfo.pci = &dev->info;
   *device = &dev->d;
   return 0;
}

int
drmGetDevices2(uint32_t flags, drmDevicePtr devices[], int max_devices)
{
   int fd = open(DRM_VEDA_RENDER_NODE, O_RDWR | O_CLOEXEC);
   if (fd < 0)
      return 0;
   drmDevicePtr dev = NULL;
   int r = drmGetDevice2(fd, flags, &dev);
   close(fd);
   if (r)
      return 0;
   if (devices && max_devices > 0)
      devices[0] = dev;
   else
      drmFreeDevice(&dev);
   return 1;
}

int
drmGetDeviceFromDevId(dev_t dev_id, uint32_t flags, drmDevicePtr *device)
{
   (void)dev_id;
   (void)flags;
   (void)device;
   return -ENODEV;
}

void
drmFreeDevice(drmDevicePtr *device)
{
   if (device && *device) {
      free(*device);
      *device = NULL;
   }
}

void
drmFreeDevices(drmDevicePtr devices[], int count)
{
   for (int i = 0; devices && i < count; i++)
      drmFreeDevice(&devices[i]);
}

int
drmGetNodeTypeFromFd(int fd)
{
   (void)fd;
   return DRM_NODE_RENDER;
}

char *
drmGetDeviceNameFromFd2(int fd)
{
   (void)fd;
   return strdup(DRM_VEDA_RENDER_NODE);
}

char *
drmGetRenderDeviceNameFromFd(int fd)
{
   (void)fd;
   return strdup(DRM_VEDA_RENDER_NODE);
}

/* ---- Syncobjs ------------------------------------------------------------- */

int
drmSyncobjCreate(int fd, uint32_t flags, uint32_t *handle)
{
   struct drm_syncobj_create args = {.flags = flags};
   int r = drmIoctl(fd, DRM_IOCTL_SYNCOBJ_CREATE, &args);
   if (r)
      return r;
   *handle = args.handle;
   return 0;
}

int
drmSyncobjDestroy(int fd, uint32_t handle)
{
   struct drm_syncobj_destroy args = {.handle = handle};
   return drmIoctl(fd, DRM_IOCTL_SYNCOBJ_DESTROY, &args);
}

int
drmSyncobjHandleToFD(int fd, uint32_t handle, int *obj_fd)
{
   struct drm_syncobj_handle args = {.handle = handle, .fd = -1};
   int r = drmIoctl(fd, DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &args);
   if (r)
      return r;
   *obj_fd = args.fd;
   return 0;
}

int
drmSyncobjFDToHandle(int fd, int obj_fd, uint32_t *handle)
{
   struct drm_syncobj_handle args = {.fd = obj_fd};
   int r = drmIoctl(fd, DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, &args);
   if (r)
      return r;
   *handle = args.handle;
   return 0;
}

int
drmSyncobjImportSyncFile(int fd, uint32_t handle, int sync_file_fd)
{
   struct drm_syncobj_handle args = {
      .handle = handle, .fd = sync_file_fd, .flags = DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE};
   return drmIoctl(fd, DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, &args);
}

int
drmSyncobjExportSyncFile(int fd, uint32_t handle, int *sync_file_fd)
{
   struct drm_syncobj_handle args = {
      .handle = handle, .fd = -1, .flags = DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE};
   int r = drmIoctl(fd, DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &args);
   if (r)
      return r;
   *sync_file_fd = args.fd;
   return 0;
}

int
drmSyncobjWait(int fd, uint32_t *handles, unsigned num_handles, int64_t timeout_nsec, unsigned flags,
               uint32_t *first_signaled)
{
   struct drm_syncobj_wait args = {
      .handles = (uintptr_t)handles, .timeout_nsec = timeout_nsec, .count_handles = num_handles, .flags = flags};
   if (drmIoctl(fd, DRM_IOCTL_SYNCOBJ_WAIT, &args))
      return -errno;
   if (first_signaled)
      *first_signaled = args.first_signaled;
   return 0;
}

int
drmSyncobjReset(int fd, const uint32_t *handles, uint32_t handle_count)
{
   struct drm_syncobj_array args = {.handles = (uintptr_t)handles, .count_handles = handle_count};
   return drmIoctl(fd, DRM_IOCTL_SYNCOBJ_RESET, &args);
}

int
drmSyncobjSignal(int fd, const uint32_t *handles, uint32_t handle_count)
{
   struct drm_syncobj_array args = {.handles = (uintptr_t)handles, .count_handles = handle_count};
   return drmIoctl(fd, DRM_IOCTL_SYNCOBJ_SIGNAL, &args);
}

int
drmSyncobjTimelineSignal(int fd, const uint32_t *handles, uint64_t *points, uint32_t handle_count)
{
   struct drm_syncobj_timeline_array args = {
      .handles = (uintptr_t)handles, .points = (uintptr_t)points, .count_handles = handle_count};
   return drmIoctl(fd, DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, &args);
}

int
drmSyncobjTimelineWait(int fd, uint32_t *handles, uint64_t *points, unsigned num_handles, int64_t timeout_nsec,
                       unsigned flags, uint32_t *first_signaled)
{
   struct drm_syncobj_timeline_wait args = {
      .handles = (uintptr_t)handles,
      .points = (uintptr_t)points,
      .timeout_nsec = timeout_nsec,
      .count_handles = num_handles,
      .flags = flags,
   };
   if (drmIoctl(fd, DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, &args))
      return -errno;
   if (first_signaled)
      *first_signaled = args.first_signaled;
   return 0;
}

int
drmSyncobjQuery(int fd, uint32_t *handles, uint64_t *points, uint32_t handle_count)
{
   return drmSyncobjQuery2(fd, handles, points, handle_count, 0);
}

int
drmSyncobjQuery2(int fd, uint32_t *handles, uint64_t *points, uint32_t handle_count, uint32_t flags)
{
   struct drm_syncobj_timeline_array args = {
      .handles = (uintptr_t)handles, .points = (uintptr_t)points, .count_handles = handle_count, .flags = flags};
   return drmIoctl(fd, DRM_IOCTL_SYNCOBJ_QUERY, &args);
}

int
drmSyncobjTransfer(int fd, uint32_t dst_handle, uint64_t dst_point, uint32_t src_handle, uint64_t src_point,
                   uint32_t flags)
{
   struct drm_syncobj_transfer args = {
      .src_handle = src_handle,
      .dst_handle = dst_handle,
      .src_point = src_point,
      .dst_point = dst_point,
      .flags = flags,
   };
   return drmIoctl(fd, DRM_IOCTL_SYNCOBJ_TRANSFER, &args);
}
