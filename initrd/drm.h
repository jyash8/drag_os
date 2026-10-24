/* DRM ioctl definitions for DragOS test program.
 * Generated to match kernel src/drivers/drm/uapi.rs and mod.rs.
 */

#ifndef DRM_H_
#define DRM_H_

/* IOCTL commands */
#define DRM_IOCTL_VERSION          0xC0406400
#define DRM_IOCTL_GET_CAP          0xC010640C
#define DRM_IOCTL_MODE_GETRESOURCES  0xC04064A0
#define DRM_IOCTL_MODE_GETCRTC     0xC06864A1
#define DRM_IOCTL_MODE_SETCRTC     0xC06864A2
#define DRM_IOCTL_MODE_GETENCODER  0xC01464A6
#define DRM_IOCTL_MODE_GETCONNECTOR 0xC05064A7
#define DRM_IOCTL_MODE_ADDFB       0xC01C64AE
#define DRM_IOCTL_MODE_ADDFB2      0xC04464B8
#define DRM_IOCTL_MODE_RMFB        0xC00464AF
#define DRM_IOCTL_MODE_PAGE_FLIP   0xC01864B0
#define DRM_IOCTL_MODE_CREATE_DUMB 0xC02064B2
#define DRM_IOCTL_MODE_MAP_DUMB    0xC01064B3
#define DRM_IOCTL_MODE_DESTROY_DUMB 0xC00464B4

/* DRM mode settings */
#define DRM_MODE_CAP_DUMB_BUFFER   (1 << 0)

#define DRM_MODE_TYPE_PREFERRED   (1 << 3)
#define DRM_MODE_FLAG_PHSYNC       (1)
#define DRM_MODE_FLAG_NHSYNC       (2)
#define DRM_MODE_FLAG_PVSYNC       (4)

#define DRM_MODE_CONNECTED        1
#define DRM_MODE_DISCONNECTED     2

#define DRM_MODE_ENCODER_TMDS     2

/* drm_version */
struct drm_version {
    int version_major;
    int version_minor;
    int version_patchlevel;
    char name[32];
    char date[16];
    char desc[32];
};

/* drm_mode_create_dumb */
struct drm_mode_create_dumb {
    uint32_t width;
    uint32_t height;
    uint32_t bpp;
    uint32_t flags;
    uint32_t handle;
    uint32_t pitch;
    uint64_t size;
};

/* drm_mode_map_dumb */
struct drm_mode_map_dumb {
    uint32_t handle;
    uint32_t pad;
    uint64_t offset;
};

/* drm_mode_destroy_dumb */
struct drm_mode_destroy_dumb {
    uint32_t handle;
};

/* drm_mode_map_dumb - mmap data */
#define MAP_DEVICE 0x100000

#endif /* DRM_H_ */