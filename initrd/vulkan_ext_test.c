/* vulkan_ext_test.c — enumerate Vulkan instance extensions to see what's available */
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <fcntl.h>
#include <unistd.h>

static int sfd = -1;
static void say(const char *s) {
    if (sfd < 0) sfd = open("/dev/ttyS0", O_WRONLY);
    if (sfd >= 0) write(sfd, s, strlen(s));
    else { fputs(s, stdout); fflush(stdout); }
}

#define VK_CHECK(call)                                                  \
    do {                                                                \
        VkResult _r = (call);                                           \
        char b[160];                                                    \
        snprintf(b, sizeof b, "[vulkan_ext] %s -> %d\n", #call, (int)_r); \
        say(b);                                                         \
        if (_r != VK_SUCCESS) {                                         \
            say("[vulkan_ext] FAIL\n");                                 \
            return 1;                                                   \
        }                                                               \
    } while (0)

int main(void) {
    setenv("VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/virtio_icd.json", 1);
    setenv("LD_LIBRARY_PATH", "/lib:/usr/lib", 1);

    sfd = open("/dev/ttyS0", O_WRONLY);
    if (sfd >= 0) { dup2(sfd, 1); dup2(sfd, 2); }

    say("[vulkan_ext] starting\n");

    uint32_t ext_count = 0;
    VkResult r = vkEnumerateInstanceExtensionProperties(NULL, &ext_count, NULL);
    say("[vulkan_ext] vkEnumerateInstanceExtensionProperties count -> ");
    char b[80];
    snprintf(b, sizeof b, "%u\n", ext_count);
    say(b);

    if (ext_count > 0) {
        VkExtensionProperties *exts = calloc(ext_count, sizeof(VkExtensionProperties));
        r = vkEnumerateInstanceExtensionProperties(NULL, &ext_count, exts);
        if (r == VK_SUCCESS) {
            for (uint32_t i = 0; i < ext_count; i++) {
                snprintf(b, sizeof b, "[vulkan_ext]   %s spec=%u\n",
                         exts[i].extensionName, exts[i].specVersion);
                say(b);
            }
        }
        free(exts);
    }

    /* Try creating instance with VK_KHR_wayland_surface */
    const char *wayland_ext = "VK_KHR_wayland_surface";
    VkInstanceCreateInfo ci = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .enabledExtensionCount = 1,
        .ppEnabledExtensionNames = &wayland_ext,
    };
    VkInstance inst;
    r = vkCreateInstance(&ci, NULL, &inst);
    snprintf(b, sizeof b, "[vulkan_ext] vkCreateInstance(wayland) -> %d\n", (int)r);
    say(b);
    if (r == VK_SUCCESS) {
        vkDestroyInstance(inst, NULL);
    }

    /* Try creating instance with VK_KHR_surface only */
    const char *surface_ext = "VK_KHR_surface";
    ci.enabledExtensionCount = 1;
    ci.ppEnabledExtensionNames = &surface_ext;
    r = vkCreateInstance(&ci, NULL, &inst);
    snprintf(b, sizeof b, "[vulkan_ext] vkCreateInstance(surface) -> %d\n", (int)r);
    say(b);
    if (r == VK_SUCCESS) {
        vkDestroyInstance(inst, NULL);
    }

    /* Try creating instance with both */
    const char *both_ext[2] = { "VK_KHR_surface", "VK_KHR_wayland_surface" };
    ci.enabledExtensionCount = 2;
    ci.ppEnabledExtensionNames = both_ext;
    r = vkCreateInstance(&ci, NULL, &inst);
    snprintf(b, sizeof b, "[vulkan_ext] vkCreateInstance(both) -> %d\n", (int)r);
    say(b);
    if (r == VK_SUCCESS) {
        vkDestroyInstance(inst, NULL);
    }

    /* Try with 0 extensions (like vulkansmoke) */
    ci.enabledExtensionCount = 0;
    ci.ppEnabledExtensionNames = NULL;
    r = vkCreateInstance(&ci, NULL, &inst);
    snprintf(b, sizeof b, "[vulkan_ext] vkCreateInstance(none) -> %d\n", (int)r);
    say(b);
    if (r == VK_SUCCESS) {
        vkDestroyInstance(inst, NULL);
    }

    say("[vulkan_ext] done\n");
    return 0;
}
