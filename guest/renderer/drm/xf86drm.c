/*
 * Copyright © 2026 Vahid Mohammadi
 * SPDX-License-Identifier: MIT
 *
 * libdrm's functions that Mesa uses, for Veda's renderer (see xf86drm.h):
 * each is the kernel interface's ioctl on the DRM device, or what sysfs
 * says of it, as libdrm makes them, with its results as libdrm gives them
 * (0 or -errno, or -1 and errno, as each of libdrm's does).
 */

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
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

/* ---- Buffers ------------------------------------------------------------- */

int
drmCloseBufferHandle(int fd, uint32_t handle)
{
   struct drm_gem_close args = {.handle = handle};
   return drmIoctl(fd, DRM_IOCTL_GEM_CLOSE, &args);
}

/* ---- The device ---------------------------------------------------------- */

/* A device description in one allocation: the drmDevice, its node's name
 * and its PCI information, as libdrm reads them from sysfs. */
struct device {
   drmDevice d;
   char *nodes[DRM_NODE_MAX];
   char node[32];
   drmPciBusInfo bus;
   drmPciDeviceInfo info;
};

/* The minor number of DRM node `fd` (render nodes from 128), or -1. */
static int
node_minor(int fd)
{
   struct stat st;
   if (fstat(fd, &st) || !S_ISCHR(st.st_mode) || major(st.st_rdev) != DRM_MAJOR)
      return -1;
   return minor(st.st_rdev);
}

static int
node_type(int minor_number)
{
   return minor_number >= 128 ? DRM_NODE_RENDER : DRM_NODE_PRIMARY;
}

static char *
node_name(int minor_number)
{
   char name[32];
   snprintf(name, sizeof(name), "%s/%s%d", DRM_DIR_NAME, minor_number >= 128 ? DRM_RENDER_MINOR_NAME : "card",
            minor_number);
   return strdup(name);
}

/* The hexadecimal number in the sysfs file `name` of the device `dir`. */
static unsigned
sysfs_number(const char *dir, const char *name)
{
   char path[128], text[32] = {0};
   snprintf(path, sizeof(path), "%s/%s", dir, name);
   int fd = open(path, O_RDONLY | O_CLOEXEC);
   if (fd < 0)
      return 0;
   ssize_t n = read(fd, text, sizeof(text) - 1);
   close(fd);
   return n > 0 ? (unsigned)strtoul(text, NULL, 16) : 0;
}

/* The device of the DRM node with `minor_number`: a PCI function's. */
static int
device_of(int minor_number, drmDevicePtr *device)
{
   char dir[64], link[256];
   snprintf(dir, sizeof(dir), "/sys/dev/char/%d:%d/device", DRM_MAJOR, minor_number);
   ssize_t n = readlink(dir, link, sizeof(link) - 1);
   if (n <= 0)
      return -ENODEV;
   link[n] = 0;
   /* The function's address ends the link: 0000:00:02.0. */
   const char *slot = strrchr(link, '/');
   unsigned domain, bus, dev, func;
   if (sscanf(slot ? slot + 1 : link, "%x:%x:%x.%x", &domain, &bus, &dev, &func) != 4)
      return -ENODEV;
   struct device *d = calloc(1, sizeof(*d));
   if (!d)
      return -ENOMEM;
   char *name = node_name(minor_number);
   snprintf(d->node, sizeof(d->node), "%s", name ? name : "");
   free(name);
   d->nodes[node_type(minor_number)] = d->node;
   d->d.nodes = d->nodes;
   d->d.available_nodes = 1 << node_type(minor_number);
   d->d.bustype = DRM_BUS_PCI;
   d->bus = (drmPciBusInfo){domain, bus, dev, func};
   d->info = (drmPciDeviceInfo){sysfs_number(dir, "vendor"), sysfs_number(dir, "device"),
                                sysfs_number(dir, "subsystem_vendor"), sysfs_number(dir, "subsystem_device"),
                                sysfs_number(dir, "revision")};
   d->d.businfo.pci = &d->bus;
   d->d.deviceinfo.pci = &d->info;
   *device = &d->d;
   return 0;
}

int
drmGetDevice2(int fd, uint32_t flags, drmDevicePtr *device)
{
   (void)flags;
   int minor_number = node_minor(fd);
   return minor_number < 0 ? -EINVAL : device_of(minor_number, device);
}

int
drmGetDevices2(uint32_t flags, drmDevicePtr devices[], int max_devices)
{
   (void)flags;
   int count = 0;
   for (int minor_number = 128; minor_number < 192; minor_number++) {
      drmDevicePtr dev = NULL;
      char path[32];
      snprintf(path, sizeof(path), "%s/%s%d", DRM_DIR_NAME, DRM_RENDER_MINOR_NAME, minor_number);
      if (access(path, F_OK) || device_of(minor_number, &dev))
         continue;
      if (devices && count < max_devices)
         devices[count] = dev;
      else
         drmFreeDevice(&dev);
      count++;
   }
   return count;
}

int
drmGetDeviceFromDevId(dev_t dev_id, uint32_t flags, drmDevicePtr *device)
{
   (void)flags;
   return major(dev_id) == DRM_MAJOR ? device_of(minor(dev_id), device) : -ENODEV;
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
   int minor_number = node_minor(fd);
   return minor_number < 0 ? -1 : node_type(minor_number);
}

char *
drmGetDeviceNameFromFd2(int fd)
{
   int minor_number = node_minor(fd);
   return minor_number < 0 ? NULL : node_name(minor_number);
}

char *
drmGetRenderDeviceNameFromFd(int fd)
{
   int minor_number = node_minor(fd);
   if (minor_number < 0)
      return NULL;
   if (minor_number >= 128)
      return node_name(minor_number);
   /* A card's render node: the one its device has. */
   char dir[64];
   snprintf(dir, sizeof(dir), "/sys/dev/char/%d:%d/device/drm", DRM_MAJOR, minor_number);
   DIR *d = opendir(dir);
   if (!d)
      return NULL;
   char *name = NULL;
   for (struct dirent *e; !name && (e = readdir(d));) {
      if (strncmp(e->d_name, DRM_RENDER_MINOR_NAME, strlen(DRM_RENDER_MINOR_NAME)) == 0)
         name = node_name(atoi(e->d_name + strlen(DRM_RENDER_MINOR_NAME)));
   }
   closedir(d);
   return name;
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
