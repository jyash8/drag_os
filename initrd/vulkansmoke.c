#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <vulkan/vulkan.h>

static void say(const char *s) {
    int fd = open("/dev/ttyS0", O_WRONLY);
    if (fd >= 0) { write(fd, s, strlen(s)); close(fd); }
    fputs(s, stdout); fflush(stdout);
}

int main(void) {
    int sfd = open("/dev/ttyS0", O_WRONLY);
    if (sfd >= 0) { dup2(sfd, 1); dup2(sfd, 2); close(sfd); }
    setenv("VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/virtio_icd.json", 1);
    setenv("LD_LIBRARY_PATH", "/lib:/usr/lib", 1);
    setenv("VK_LOADER_DEBUG", "all", 1);
    say("[vulkansmoke] starting\n");

    VkApplicationInfo app = {0};
    app.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO;
    app.pApplicationName = "vulkansmoke";
    app.applicationVersion = VK_MAKE_VERSION(1,0,0);
    app.pEngineName = "none";
    app.engineVersion = VK_MAKE_VERSION(1,0,0);
    app.apiVersion = VK_API_VERSION_1_0;

    VkInstanceCreateInfo ci = {0};
    ci.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO;
    ci.pApplicationInfo = &app;

    VkInstance inst = VK_NULL_HANDLE;
    VkResult r = vkCreateInstance(&ci, NULL, &inst);
    char buf[256];
    snprintf(buf, sizeof(buf), "[vulkansmoke] vkCreateInstance -> %d\n", (int)r);
    say(buf);
    if (r != VK_SUCCESS) {
        say("[vulkansmoke] FAIL: no instance\n");
        return 1;
    }

    uint32_t ndev = 0;
    vkEnumeratePhysicalDevices(inst, &ndev, NULL);
    snprintf(buf, sizeof(buf), "[vulkansmoke] physical devices: %u\n", ndev);
    say(buf);
    if (ndev == 0) {
        say("[vulkansmoke] FAIL: no physical device\n");
        vkDestroyInstance(inst, NULL);
        return 2;
    }

    VkPhysicalDevice *devs = calloc(ndev, sizeof(*devs));
    vkEnumeratePhysicalDevices(inst, &ndev, devs);
    for (uint32_t i = 0; i < ndev; i++) {
        VkPhysicalDeviceProperties p;
        vkGetPhysicalDeviceProperties(devs[i], &p);
        snprintf(buf, sizeof(buf), "[vulkansmoke] dev[%u]: %s (type=%d) api=%u.%u.%u\n", i, p.deviceName,
               (int)p.deviceType,
               VK_VERSION_MAJOR(p.apiVersion), VK_VERSION_MINOR(p.apiVersion), VK_VERSION_PATCH(p.apiVersion));
        say(buf);
    }

    for (uint32_t i = 0; i < ndev; i++) {
        uint32_t nq = 0;
        vkGetPhysicalDeviceQueueFamilyProperties(devs[i], &nq, NULL);
        snprintf(buf, sizeof(buf), "[vulkansmoke] dev[%u] queue families: %u\n", i, nq);
        say(buf);
    }

    free(devs);
    vkDestroyInstance(inst, NULL);
    say("[vulkansmoke] OK\n");
    return 0;
}