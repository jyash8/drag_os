// gbm scanout probe: create BOs with various usage and report geometry. The
// kernel's own log tells us which allocation path each one took (create_dumb
// = 2D scanout buffer, VIRTGPU resource_create_3d = virgl texture), so all
// this needs to do is create them and print the stride it was given.
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <gbm.h>

static void try_bo(struct gbm_device *dev, const char *label, unsigned usage) {
	unsigned w = 1280, h = 800;
	struct gbm_bo *bo = gbm_bo_create(dev, w, h, GBM_FORMAT_XRGB8888, usage);
	if (!bo) {
		printf("%-24s gbm_bo_create FAILED\n", label);
		return;
	}
	unsigned stride = gbm_bo_get_stride(bo);
	unsigned height = gbm_bo_get_height(bo);
	unsigned bw = gbm_bo_get_width(bo);
	unsigned fmt = gbm_bo_get_format(bo);
	printf("%-24s stride=%u %ux%u fmt=0x%x\n", label, stride, bw, height, fmt);
	/* Keep it alive across the print so the handle stays valid. */
	fflush(stdout);
	gbm_bo_destroy(bo);
}

int main(void) {
	int fd = open("/dev/dri/card0", O_RDWR);
	if (fd < 0) { perror("open card0"); return 1; }

	struct gbm_device *dev = gbm_create_device(fd);
	if (!dev) { fprintf(stderr, "gbm_create_device failed\n"); return 1; }
	printf("gbm device ok\n");

	try_bo(dev, "SCANOUT only",      GBM_BO_USE_SCANOUT);
	try_bo(dev, "SCANOUT|RENDERING", GBM_BO_USE_SCANOUT | GBM_BO_USE_RENDERING);
	try_bo(dev, "RENDERING only",    GBM_BO_USE_RENDERING);
	try_bo(dev, "LINEAR|SCANOUT",    GBM_BO_USE_LINEAR | GBM_BO_USE_SCANOUT);

	gbm_device_destroy(dev);
	close(fd);
	printf("GBMSCANOUT_DONE\n");
	return 0;
}
