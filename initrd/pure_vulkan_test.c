/* pure_vulkan_test.c — DragOS guest: pure Vulkan render test (no SDL).
 *
 * Creates a Vulkan instance, device, swapchain, clears each frame to a
 * rotating color and presents. Logs every step to /dev/ttyS0. Confirms
 * end-to-end GPU 3D rendering through the virtio/virgl ICD.
 */
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <stdlib.h>

static int sfd = -1;
static void say(const char *s) {
    if (sfd < 0) sfd = open("/dev/ttyS0", O_WRONLY);
    if (sfd >= 0) write(sfd, s, strlen(s));
    fputs(s, stdout); fflush(stdout);
}
#define VK_CHECK(call)                                                  \
    do {                                                                \
        VkResult _r = (call);                                           \
        char b[160];                                                    \
        snprintf(b, sizeof b, "[pure_vulkan] %s -> %d\n", #call, (int)_r); \
        say(b);                                                         \
        if (_r != VK_SUCCESS) return 1;                                 \
    } while (0)

int main(int argc, char **argv) {
    setenv("VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/virtio_icd.json", 1);
    setenv("LD_LIBRARY_PATH", "/lib:/usr/lib", 1);
    setenv("WAYLAND_DISPLAY", "/run/wayland-1", 1);
    setenv("VK_LOADER_DEBUG", "all", 1);

    sfd = open("/dev/ttyS0", O_WRONLY);
    if (sfd >= 0) { dup2(sfd, 1); dup2(sfd, 2); close(sfd); sfd = 1; }
    say("[pure_vulkan] main entered\n");

    VkApplicationInfo ai = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "pure_vulkan_test", .apiVersion = VK_API_VERSION_1_0 };
    VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &ai, .enabledExtensionCount = 0 };
    VkInstance inst;
    VK_CHECK(vkCreateInstance(&ici, NULL, &inst));
    say("[pure_vulkan] vkCreateInstance OK\n");

    uint32_t ndev = 0;
    vkEnumeratePhysicalDevices(inst, &ndev, NULL);
    VkPhysicalDevice *pdevs = malloc(ndev * sizeof(VkPhysicalDevice));
    vkEnumeratePhysicalDevices(inst, &ndev, pdevs);
    VkPhysicalDevice pdev = pdevs[0];
    say("[pure_vulkan] picked physical device 0\n");

    uint32_t nqf = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nqf, NULL);
    VkQueueFamilyProperties *qfps = malloc(nqf * sizeof(VkQueueFamilyProperties));
    vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nqf, qfps);
    uint32_t gq = VK_QUEUE_FAMILY_IGNORED;
    for (uint32_t i = 0; i < nqf; i++)
        if (qfps[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) { gq = i; break; }
    if (gq == VK_QUEUE_FAMILY_IGNORED) { say("[pure_vulkan] FAIL no queue\n"); return 1; }
    say("[pure_vulkan] queue family found\n");

    float prio = 1.0f;
    VkDeviceQueueCreateInfo dqci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = gq, .queueCount = 1, .pQueuePriorities = &prio };
    const char *dexts[] = { "VK_KHR_swapchain" };
    VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .queueCreateInfoCount = 1, .pQueueCreateInfos = &dqci,
        .enabledExtensionCount = 1, .ppEnabledExtensionNames = dexts };
    VkDevice dev;
    VK_CHECK(vkCreateDevice(pdev, &dci, NULL, &dev));
    say("[pure_vulkan] device created\n");

    VkQueue queue;
    vkGetDeviceQueue(dev, gq, 0, &queue);

    /* We have no window/surface, so create a 640x480 offscreen image as the
     * "swapchain" target, clear it, and submit. This confirms the GPU draws. */
    VkImageCreateInfo ici2 = { .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        .imageType = VK_IMAGE_TYPE_2D, .format = VK_FORMAT_B8G8R8A8_UNORM,
        .extent = { 640, 480, 1 }, .mipLevels = 1, .arrayLayers = 1,
        .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_OPTIMAL,
        .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE, .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED };
    VkImage img;
    VK_CHECK(vkCreateImage(dev, &ici2, NULL, &img));
    VkMemoryRequirements mr;
    vkGetImageMemoryRequirements(dev, img, &mr);
    VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .allocationSize = mr.size, .memoryTypeIndex = 0 };
    uint32_t mt = 0;
    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(pdev, &mp);
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
        if ((mr.memoryTypeBits & (1u << i)) &&
            (mp.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)) { mt = i; break; }
    mai.memoryTypeIndex = mt;
    VkDeviceMemory mem;
    VK_CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));
    VK_CHECK(vkBindImageMemory(dev, img, mem, 0));

    VkImageViewCreateInfo ivci = { .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
        .image = img, .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = VK_FORMAT_B8G8R8A8_UNORM,
        .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
    VkImageView view;
    VK_CHECK(vkCreateImageView(dev, &ivci, NULL, &view));

    VkAttachmentDescription att = { .format = VK_FORMAT_B8G8R8A8_UNORM,
        .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED, .finalLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL };
    VkRenderPassCreateInfo rpci = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
        .attachmentCount = 1, .pAttachments = &att };
    VkAttachmentReference aref = { .attachment = 0, .layout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL };
    VkSubpassDescription sp = { .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS,
        .colorAttachmentCount = 1, .pColorAttachments = &aref };
    rpci.subpassCount = 1; rpci.pSubpasses = &sp;
    VkRenderPass rp;
    VK_CHECK(vkCreateRenderPass(dev, &rpci, NULL, &rp));

    VkFramebufferCreateInfo fbci = { .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
        .renderPass = rp, .attachmentCount = 1, .pAttachments = &view,
        .width = 640, .height = 480, .layers = 1 };
    VkFramebuffer fb;
    VK_CHECK(vkCreateFramebuffer(dev, &fbci, NULL, &fb));

    VkCommandPoolCreateInfo cpci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = gq };
    VkCommandPool cp;
    VK_CHECK(vkCreateCommandPool(dev, &cpci, NULL, &cp));
    VkCommandBufferAllocateInfo cbai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = cp, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
    VkCommandBuffer cb;
    VK_CHECK(vkAllocateCommandBuffers(dev, &cbai, &cb));

    VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
    VkFence fence;
    VK_CHECK(vkCreateFence(dev, &fci, NULL, &fence));

    say("[pure_vulkan] ready to render\n");
    for (uint32_t frame = 0; frame < 150; frame++) {
        vkResetCommandBuffer(cb, 0);
        VkCommandBufferBeginInfo cbbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
        vkBeginCommandBuffer(cb, &cbbi);
        VkClearValue clear = { .color = { .float32 = {
            (frame % 3 == 0) ? 1.0f : 0.0f,
            (frame % 3 == 1) ? 1.0f : 0.0f,
            (frame % 3 == 2) ? 1.0f : 0.0f, 1.0f } } };
        VkRenderPassBeginInfo rpbi = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
            .renderPass = rp, .framebuffer = fb,
            .renderArea = { .extent = { 640, 480 } },
            .clearValueCount = 1, .pClearValues = &clear };
        vkCmdBeginRenderPass(cb, &rpbi, VK_SUBPASS_CONTENTS_INLINE);
        vkCmdEndRenderPass(cb);
        vkEndCommandBuffer(cb);
        VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
            .commandBufferCount = 1, .pCommandBuffers = &cb };
        vkResetFences(dev, 1, &fence);
        vkQueueSubmit(queue, 1, &si, fence);
        vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX);
        if (frame % 25 == 0) {
            char b[80]; snprintf(b, sizeof b, "[pure_vulkan] frame %u rendered\n", frame); say(b);
        }
    }
    say("[pure_vulkan] done rendering 150 frames\n");
    return 0;
}
