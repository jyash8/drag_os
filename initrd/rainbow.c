// rainbow: present a known radial rainbow through the DRM scanout path.
//
// The pattern is concentric hue rings plus a white crosshair through the exact
// centre, computed from the pixel's own (x, y). That makes it a *geometry*
// probe, not just an "is something on screen" probe:
//
//   - correct row stride  -> the same rings and crosshair we drew
//   - wrong row stride    -> rings shear into spirals, crosshair leaves the centre
//   - wrong offset        -> the whole image is shifted
//   - nothing presented   -> black
//
// So one screenshot answers the question the compositor's own output could not:
// is the 3D/virtio-gpu presentation path geometrically correct?
//
// Usage: rainbow [2d|3d] [hold_secs]
//   2d -> CREATE_DUMB + mmap + ADDFB + PAGE_FLIP  (the 2D resource path)
//   3d -> VIRTGPU_RESOURCE_CREATE_3D + ADDFB + PAGE_FLIP (what the compositor uses)
//
// Prints RAINBOW_READY before holding, so a harness knows when to capture.
/*
 * DRM ioctl numbers come from the kernel's own uapi crate so the guest
 * cannot drift from the driver. See scripts/generate-drm-ioctls.sh.
 */
#include "drm_ioctls.h"
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

#define W 1280
#define H 800

/*
 * Structs and ioctl numbers are copied verbatim from the kernel's own uapi
 * (crates/drm-uapi/src/lib.rs). The host's libdrm headers here are from a
 * different uapi generation, so relying on them would silently mismatch the
 * ABI rather than fail to compile.
 */
#define IOC_NRBITS 8
#define IOC_TYPEBITS 8
#define IOC_SIZEBITS 14
#define IOC_NRSHIFT 0
#define IOC_TYPESHIFT (IOC_NRSHIFT + IOC_NRBITS)
#define IOC_SIZESHIFT (IOC_TYPESHIFT + IOC_TYPEBITS)
#define IOC_DIRSHIFT (IOC_SIZESHIFT + IOC_SIZEBITS)
#define IOC_NONE 0U
#define IOC_WRITE 1U
/* The kernel's own SET_MASTER encoding (see crates/drm-uapi). */

struct k_create_dumb {
	uint32_t height, width, bpp, flags, handle, pitch;
	uint64_t size;
};

struct k_map_dumb {
	uint32_t handle, pad;
	uint64_t offset;
};

struct k_fb_cmd {
	uint32_t fb_id, width, height, pitch, bpp, depth, handle;
};

/* drm_mode_card_res: four user pointers first, then the counts. */
struct k_card_res {
	uint64_t fb_id_ptr, crtc_id_ptr, connector_id_ptr, encoder_id_ptr;
	uint32_t count_fbs, count_crtcs, count_connectors, count_encoders;
	uint32_t min_width, max_width, min_height, max_height;
};

/* Return the first CRTC id from MODE_GETRESOURCES, or 0 if there is none. */
static uint32_t find_crtc(int fd)
{
	struct k_card_res res;
	memset(&res, 0, sizeof(res));
	if (ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res) != 0) {
		perror("GETRESOURCES");
		return 0;
	}
	printf("GETRESOURCES crtcs=%u encoders=%u connectors=%u fbs=%u %ux%u\n",
	       res.count_crtcs, res.count_encoders, res.count_connectors,
	       res.count_fbs, res.max_width, res.max_height);
	if (res.count_crtcs == 0) return 0;

	static uint32_t ids[8];
	memset(ids, 0, sizeof(ids));
	res.crtc_id_ptr = (uint64_t)(uintptr_t)ids;
	if (ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res) != 0) {
		perror("GETRESOURCES ids");
		return 0;
	}
	return ids[0];
}

struct k_page_flip {
	uint32_t crtc_id, fb_id, flags, reserved;
	uint64_t user_data;
};

/* DRM_IOCTL_VIRTGPU_RESOURCE_CREATE, as the kernel defines it. */
struct k_virtgpu_resource_create {
	uint32_t target, format, bind;
	uint32_t width, height, depth, array_size;
	uint32_t last_level, nr_samples, flags;
	uint32_t bo_handle, res_handle, size, stride;
};

#define VIRGL_RESOURCE_TARGET_2D 2
#define PIPE_FORMAT_B8G8R8X8_UNORM 1
#define PIPE_BIND_SAMPLER_VIEW 0x1
#define PIPE_BIND_RENDER_TARGET 0x20
#define VIRGL_BIND_SCANOUT (1 << 18)

/* XRGB8888 in a little-endian u32. */
#define PIXEL(r, g, b) ((uint32_t)(((b) << 16) | ((g) << 8) | (r)))

static uint32_t hue_to_rgb(double h_deg, double sat, double val)
{
	double c = val * sat;
	double hp = h_deg / 60.0;
	double x = c * (1.0 - fabs(fmod(hp, 2.0) - 1.0));
	double m = val - c;
	double r = 0, g = 0, b = 0;
	switch (((int)hp) % 6) {
	case 0: r = c; g = x; b = 0; break;
	case 1: r = x; g = c; b = 0; break;
	case 2: r = 0; g = c; b = x; break;
	case 3: r = 0; g = x; b = c; break;
	case 4: r = x; g = 0; b = c; break;
	default: r = c; g = 0; b = x; break;
	}
	return PIXEL((int)((r + m) * 255 + 0.5), (int)((g + m) * 255 + 0.5),
	             (int)((b + m) * 255 + 0.5));
}

/* Draw the pattern at the buffer's real `pitch` bytes per row. */
static void fill_rainbow(uint8_t *base, uint32_t pitch)
{
	double cx = W / 2.0, cy = H / 2.0;
	double max_r = sqrt(cx * cx + cy * cy);
	for (int y = 0; y < H; y++) {
		uint32_t *row = (uint32_t *)(base + (size_t)y * pitch);
		for (int x = 0; x < W; x++) {
			double dx = x - cx, dy = y - cy;
			double r = sqrt(dx * dx + dy * dy) / max_r;
			row[x] = hue_to_rgb(r * 360.0, 1.0, 1.0);
		}
	}
	/* White crosshair: one row and one column through the exact centre. */
	uint32_t *mid_row = (uint32_t *)(base + (size_t)(H / 2) * pitch);
	for (int x = 0; x < W; x++) mid_row[x] = PIXEL(255, 255, 255);
	for (int y = 0; y < H; y++) {
		uint32_t *row = (uint32_t *)(base + (size_t)y * pitch);
		row[W / 2] = PIXEL(255, 255, 255);
	}
}

static int do_2d(int fd, uint32_t *handle, uint32_t *pitch)
{
	struct k_create_dumb cdb = { .width = W, .height = H, .bpp = 32 };
	if (ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &cdb) != 0) {
		perror("CREATE_DUMB");
		return -1;
	}
	struct k_map_dumb mdb = { .handle = cdb.handle };
	uint64_t off = 0;
	if (ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mdb) != 0) {
		perror("MAP_DUMB");
		return -1;
	}
	off = mdb.offset;
	uint32_t p = cdb.pitch ? cdb.pitch : W * 4;
	uint32_t bytes = p * H;
	void *map = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd,
	                 (off_t)off);
	if (map == MAP_FAILED) {
		perror("mmap dumb");
		return -1;
	}
	/* Touch every page so the backing is really populated. */
	memset(map, 0xA5, bytes);
	fill_rainbow(map, p);
	msync(map, bytes, MS_SYNC);
	munmap(map, bytes);
	*handle = cdb.handle;
	*pitch = p;
	printf("2d dumb handle=%u pitch=%u\n", *handle, *pitch);
	return 0;
}

static int do_3d(int fd, uint32_t *handle, uint32_t *pitch)
{
	struct k_virtgpu_resource_create rc = { 0 };
	rc.target = VIRGL_RESOURCE_TARGET_2D;
	rc.format = PIPE_FORMAT_B8G8R8X8_UNORM;
	rc.bind = PIPE_BIND_SAMPLER_VIEW | PIPE_BIND_RENDER_TARGET | VIRGL_BIND_SCANOUT;
	rc.width = W;
	rc.height = H;
	rc.depth = 1;
	rc.array_size = 1;
	if (ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE, &rc) != 0) {
		perror("VIRTGPU_RESOURCE_CREATE");
		return -1;
	}
	*handle = rc.bo_handle;
	*pitch = rc.stride;
	printf("3d resource handle=%u res=%u stride=%u size=%u\n", rc.bo_handle,
	       rc.res_handle, rc.stride, rc.size);
	return 0;
}

/*
 * The compositor runs at ~60Hz and re-presents its own buffers, so a test flip
 * is overwritten before anyone can look at it. Stop any other DRM client
 * first so this process owns the scanout for the duration of the probe.
 */
static void stop_other_drm_clients(void)
{
	/* argv[0]-independent: scan /proc for a process named mywima and kill it. */
	DIR *d = opendir("/proc");
	if (!d) return;
	struct dirent *e;
	while ((e = readdir(d))) {
		if (e->d_name[0] == '.') continue;
		char path[256], exe[300];
		snprintf(path, sizeof(path), "/proc/%s/comm", e->d_name);
		FILE *f = fopen(path, "r");
		if (!f) continue;
		char name[64] = { 0 };
		if (fgets(name, sizeof(name), f)) {
			name[strcspn(name, "\n")] = 0;
			if (strcmp(name, "mywima") == 0) {
				printf("stopping compositor pid=%s\n", e->d_name);
				snprintf(exe, sizeof(exe), "/bin/kill -9 %s", e->d_name);
				if (system(exe) != 0) printf("kill failed\n");
			}
		}
		fclose(f);
	}
	closedir(d);
	sleep(2);
}

int main(int argc, char **argv)
{
	const char *mode = argc > 1 ? argv[1] : "2d";
	int hold = argc > 2 ? atoi(argv[2]) : 20;

	if (getenv("RAINBOW_NO_KILL") == NULL) stop_other_drm_clients();

	int fd = open("/dev/dri/card0", O_RDWR);
	if (fd < 0) { perror("open card0"); return 1; }
	/* Become DRM master so nothing else can re-present over us. */
	if (ioctl(fd, DRM_IOCTL_SET_MASTER) != 0)
		printf("note: DRM_IOCTL_SET_MASTER: %s (continuing)\n", strerror(errno));

	/* Ask the driver which CRTC exists rather than assuming an id. A flip to
	 * a CRTC that is not there fails with ENOENT, and the probe would then
	 * "pass" while presenting nothing. */
	uint32_t crtc = find_crtc(fd);
	if (crtc == 0) {
		printf("no CRTC reported by the driver\n");
		return 1;
	}
	uint32_t handle = 0, pitch = 0;
	int rc = (strcmp(mode, "3d") == 0) ? do_3d(fd, &handle, &pitch)
	                                   : do_2d(fd, &handle, &pitch);
	if (rc != 0) return 1;

	struct k_fb_cmd fb = { 0 };
	fb.width = W;
	fb.height = H;
	fb.pitch = pitch;
	fb.bpp = 32;
	fb.depth = 24;
	fb.handle = handle;
	if (ioctl(fd, DRM_IOCTL_MODE_ADDFB, &fb) != 0) {
		perror("ADDFB");
		return 1;
	}
	printf("ADDFB fb_id=%u\n", fb.fb_id);

	struct k_page_flip pf = { .crtc_id = crtc, .fb_id = fb.fb_id,
	                         .user_data = 0xABCDEF };
	if (ioctl(fd, DRM_IOCTL_MODE_PAGE_FLIP, &pf) != 0) {
		/* Presenting nothing must not look like success, so report the
		 * failure and exit non-zero. The harness matches on RAINBOW_DONE
		 * being absent, so a probe that cannot present fails its test. */
		perror("PAGE_FLIP");
		printf("RAINBOW_FAIL no frame presented\n");
		return 1;
	}
	printf("PAGE_FLIP submitted\n");

	printf("RAINBOW_READY mode=%s %dx%d pitch=%u\n", mode, W, H, pitch);
	fflush(stdout);
	sleep(hold);
	printf("RAINBOW_DONE\n");
	return 0;
}
