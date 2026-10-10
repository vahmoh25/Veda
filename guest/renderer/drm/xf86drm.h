/*
 * Copyright © 2026 Vahid Mohammadi
 * SPDX-License-Identifier: MIT
 *
 * <xf86drm.h> for Veda's renderer: what Mesa's drivers use of libdrm's
 * interface, with libdrm's names and types, on Linux's DRM devices. The
 * functions are the kernel interface's ioctls (drm.h), and a device's
 * identity comes from sysfs, as libdrm has them.
 */

#ifndef _XF86DRM_H_
#define _XF86DRM_H_

#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>

#include "drm-uapi/drm.h"

#ifdef __cplusplus
extern "C" {
#endif

#define DRM_IOC_VOID _IOC_NONE
#define DRM_IOC_READ _IOC_READ
#define DRM_IOC_WRITE _IOC_WRITE
#define DRM_IOC_READWRITE (_IOC_READ | _IOC_WRITE)
#define DRM_IOC(dir, group, nr, size) _IOC(dir, group, nr, size)

#define DRM_MAJOR 226
#define DRM_DIR_NAME "/dev/dri"
#define DRM_RENDER_MINOR_NAME "renderD"

typedef struct _drmVersion {
   int version_major;
   int version_minor;
   int version_patchlevel;
   int name_len;
   char *name;
   int date_len;
   char *date;
   int desc_len;
   char *desc;
} drmVersion, *drmVersionPtr;

#define DRM_NODE_PRIMARY 0
#define DRM_NODE_CONTROL 1
#define DRM_NODE_RENDER 2
#define DRM_NODE_MAX 3

#define DRM_BUS_PCI 0
#define DRM_BUS_USB 1
#define DRM_BUS_PLATFORM 2
#define DRM_BUS_HOST1X 3

#define DRM_DEVICE_GET_PCI_REVISION (1 << 0)

typedef struct _drmPciBusInfo {
   uint16_t domain;
   uint8_t bus;
   uint8_t dev;
   uint8_t func;
} drmPciBusInfo, *drmPciBusInfoPtr;

typedef struct _drmPciDeviceInfo {
   uint16_t vendor_id;
   uint16_t device_id;
   uint16_t subvendor_id;
   uint16_t subdevice_id;
   uint8_t revision_id;
} drmPciDeviceInfo, *drmPciDeviceInfoPtr;

typedef struct _drmUsbBusInfo {
   uint8_t bus;
   uint8_t dev;
} drmUsbBusInfo, *drmUsbBusInfoPtr;

typedef struct _drmUsbDeviceInfo {
   uint16_t vendor;
   uint16_t product;
} drmUsbDeviceInfo, *drmUsbDeviceInfoPtr;

#define DRM_PLATFORM_DEVICE_NAME_LEN 512

typedef struct _drmPlatformBusInfo {
   char fullname[DRM_PLATFORM_DEVICE_NAME_LEN];
} drmPlatformBusInfo, *drmPlatformBusInfoPtr;

typedef struct _drmPlatformDeviceInfo {
   char **compatible;
} drmPlatformDeviceInfo, *drmPlatformDeviceInfoPtr;

#define DRM_HOST1X_DEVICE_NAME_LEN 512

typedef struct _drmHost1xBusInfo {
   char fullname[DRM_HOST1X_DEVICE_NAME_LEN];
} drmHost1xBusInfo, *drmHost1xBusInfoPtr;

typedef struct _drmHost1xDeviceInfo {
   char **compatible;
} drmHost1xDeviceInfo, *drmHost1xDeviceInfoPtr;

typedef struct _drmDevice {
   char **nodes;
   int available_nodes;
   int bustype;
   union {
      drmPciBusInfoPtr pci;
      drmUsbBusInfoPtr usb;
      drmPlatformBusInfoPtr platform;
      drmHost1xBusInfoPtr host1x;
   } businfo;
   union {
      drmPciDeviceInfoPtr pci;
      drmUsbDeviceInfoPtr usb;
      drmPlatformDeviceInfoPtr platform;
      drmHost1xDeviceInfoPtr host1x;
   } deviceinfo;
} drmDevice, *drmDevicePtr;

int drmIoctl(int fd, unsigned long request, void *arg);
int drmGetCap(int fd, uint64_t capability, uint64_t *value);
int drmCommandNone(int fd, unsigned long index);
int drmCommandRead(int fd, unsigned long index, void *data, unsigned long size);
int drmCommandWrite(int fd, unsigned long index, void *data, unsigned long size);
int drmCommandWriteRead(int fd, unsigned long index, void *data, unsigned long size);

drmVersionPtr drmGetVersion(int fd);
void drmFreeVersion(drmVersionPtr version);
int drmGetMagic(int fd, drm_magic_t *magic);

int drmPrimeHandleToFD(int fd, uint32_t handle, uint32_t flags, int *prime_fd);
int drmPrimeFDToHandle(int fd, int prime_fd, uint32_t *handle);
int drmCloseBufferHandle(int fd, uint32_t handle);

int drmGetDevice2(int fd, uint32_t flags, drmDevicePtr *device);
int drmGetDevices2(uint32_t flags, drmDevicePtr devices[], int max_devices);
int drmGetDeviceFromDevId(dev_t dev_id, uint32_t flags, drmDevicePtr *device);
void drmFreeDevice(drmDevicePtr *device);
void drmFreeDevices(drmDevicePtr devices[], int count);
int drmGetNodeTypeFromFd(int fd);
char *drmGetDeviceNameFromFd2(int fd);
char *drmGetRenderDeviceNameFromFd(int fd);

int drmSyncobjCreate(int fd, uint32_t flags, uint32_t *handle);
int drmSyncobjDestroy(int fd, uint32_t handle);
int drmSyncobjHandleToFD(int fd, uint32_t handle, int *obj_fd);
int drmSyncobjFDToHandle(int fd, int obj_fd, uint32_t *handle);
int drmSyncobjImportSyncFile(int fd, uint32_t handle, int sync_file_fd);
int drmSyncobjExportSyncFile(int fd, uint32_t handle, int *sync_file_fd);
int drmSyncobjWait(int fd, uint32_t *handles, unsigned num_handles, int64_t timeout_nsec, unsigned flags,
                   uint32_t *first_signaled);
int drmSyncobjReset(int fd, const uint32_t *handles, uint32_t handle_count);
int drmSyncobjSignal(int fd, const uint32_t *handles, uint32_t handle_count);
int drmSyncobjTimelineSignal(int fd, const uint32_t *handles, uint64_t *points, uint32_t handle_count);
int drmSyncobjTimelineWait(int fd, uint32_t *handles, uint64_t *points, unsigned num_handles, int64_t timeout_nsec,
                           unsigned flags, uint32_t *first_signaled);
int drmSyncobjQuery(int fd, uint32_t *handles, uint64_t *points, uint32_t handle_count);
int drmSyncobjQuery2(int fd, uint32_t *handles, uint64_t *points, uint32_t handle_count, uint32_t flags);
int drmSyncobjTransfer(int fd, uint32_t dst_handle, uint64_t dst_point, uint32_t src_handle, uint64_t src_point,
                       uint32_t flags);

#ifdef __cplusplus
}
#endif

#endif
