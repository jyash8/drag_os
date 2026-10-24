/* GENERATED FILE - do not edit.
 *
 * DRM ioctl numbers, emitted from crates/drm-uapi (see
 * scripts/generate-drm-ioctls.sh). Do not hand-copy these into a test
 * program: the guest and the kernel must agree on the exact value userspace
 * sends, and the kernel's own constants are the only place that is checked
 * against upstream Linux's headers.
 */
#pragma once

#define DRM_IOCTL_VERSION                      0xC0406400u
#define DRM_IOCTL_GET_CAP                      0xC010640Cu
#define DRM_IOCTL_SET_CLIENT_CAP               0x4010640Du
#define DRM_IOCTL_GEM_CLOSE                    0x40086409u
#define DRM_IOCTL_AUTH_MAGIC                   0x40046411u
#define DRM_IOCTL_SET_MASTER                   0x0000641Eu
#define DRM_IOCTL_DROP_MASTER                  0x0000641Fu
#define DRM_IOCTL_MODE_GETRESOURCES            0xC04064A0u
#define DRM_IOCTL_MODE_GETCRTC                 0xC06864A1u
#define DRM_IOCTL_MODE_SETCRTC                 0xC06864A2u
#define DRM_IOCTL_MODE_CURSOR                  0xC01C64A3u
#define DRM_IOCTL_MODE_GETGAMMA                0xC02064A4u
#define DRM_IOCTL_MODE_SETGAMMA                0xC02064A5u
#define DRM_IOCTL_MODE_GETENCODER              0xC01464A6u
#define DRM_IOCTL_MODE_GETCONNECTOR            0xC05064A7u
#define DRM_IOCTL_MODE_GETPROPERTY             0xC04064AAu
#define DRM_IOCTL_MODE_GETPROPBLOB             0xC01064ACu
#define DRM_IOCTL_MODE_ADDFB                   0xC01C64AEu
#define DRM_IOCTL_MODE_RMFB                    0xC00464AFu
#define DRM_IOCTL_MODE_PAGE_FLIP               0xC01864B0u
#define DRM_IOCTL_MODE_DIRTYFB                 0xC01864B1u
#define DRM_IOCTL_MODE_CREATE_DUMB             0xC02064B2u
#define DRM_IOCTL_MODE_MAP_DUMB                0xC01064B3u
#define DRM_IOCTL_MODE_DESTROY_DUMB            0xC00464B4u
#define DRM_IOCTL_MODE_GETPLANERESOURCES       0xC01064B5u
#define DRM_IOCTL_MODE_GETPLANE                0xC02064B6u
#define DRM_IOCTL_MODE_SETPLANE                0xC03064B7u
#define DRM_IOCTL_MODE_ADDFB2                  0xC06864B8u
#define DRM_IOCTL_MODE_OBJ_GETPROPERTIES       0xC02064B9u
#define DRM_IOCTL_MODE_ATOMIC                  0xC03864BCu
#define DRM_IOCTL_PRIME_HANDLE_TO_FD           0xC00C642Du
#define DRM_IOCTL_PRIME_FD_TO_HANDLE           0xC00C642Eu
#define DRM_IOCTL_SYNCOBJ_CREATE               0xC00864BFu
#define DRM_IOCTL_SYNCOBJ_DESTROY              0xC00864C0u
#define DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD         0xC01864C1u
#define DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE         0xC01864C2u
#define DRM_IOCTL_SYNCOBJ_WAIT                 0xC02864C3u
#define DRM_IOCTL_SYNCOBJ_RESET                0xC01064C4u
#define DRM_IOCTL_SYNCOBJ_SIGNAL               0xC01064C5u
#define DRM_IOCTL_VIRTGPU_MAP                  0xC0106441u
#define DRM_IOCTL_VIRTGPU_EXECBUFFER           0xC0406442u
#define DRM_IOCTL_VIRTGPU_GETPARAM             0xC0106443u
#define DRM_IOCTL_VIRTGPU_RESOURCE_CREATE      0xC0386444u
#define DRM_IOCTL_VIRTGPU_RESOURCE_INFO        0xC0106445u
#define DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST   0xC02C6446u
#define DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST     0xC02C6447u
#define DRM_IOCTL_VIRTGPU_WAIT                 0xC0086448u
#define DRM_IOCTL_VIRTGPU_GET_CAPS             0xC0186449u
#define DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB 0xC038644Au
#define DRM_IOCTL_VIRTGPU_CONTEXT_INIT         0xC010644Bu
