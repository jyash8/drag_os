/* sdl_vulkan_test.c — DragOS guest: SDL2 + Vulkan render test.
 *
 * Initializes SDL2 with Wayland backend, creates a window, loads Vulkan,
 * creates an instance, device, swapchain, and presents animated gradient
 * frames continuously. Handles VK_ERROR_OUT_OF_DATE_KHR by recreating the
 * swapchain. Logs every step to /dev/ttyS0.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <math.h>
#include <time.h>
#include <signal.h>
#include <SDL2/SDL.h>
#include <SDL2/SDL_vulkan.h>
#include <vulkan/vulkan.h>

static int sfd = -1;
static void say(const char *s) {
    if (sfd < 0) sfd = open("/dev/ttyS0", O_WRONLY);
    if (sfd >= 0) write(sfd, s, strlen(s));
    fputs(s, stdout); fflush(stdout);
}

/* Watchdog handler: the test may be blocked in a Vulkan call, so there is
 * nothing to clean up -- exit. */
static void on_alarm(int sig) {
    (void)sig;
    say("[sdl_vulkan] watchdog fired, exiting\n");
    _exit(0);
}

#define VK_CHECK(call)                                                  \
    do {                                                                \
        VkResult _r = (call);                                           \
        char b[160];                                                    \
        snprintf(b, sizeof b, "[sdl_vulkan] %s -> %d\n", #call, (int)_r); \
        say(b);                                                         \
        if (_r != VK_SUCCESS) {                                         \
            say("[sdl_vulkan] FAIL: Vulkan call returned error\n");     \
            return 1;                                                   \
        }                                                               \
    } while (0)

/* Wayland reports currentExtent as UINT32_MAX: the app must derive the
 * drawable size from the window instead of assuming an initial size. The
 * compositor promotes the surface to fullscreen (1280x800) after it was
 * created at 640x480, so a hardcoded 640x480 swapchain no longer matches the
 * surface and gets scaled or clipped. */
static VkExtent2D pick_extent(SDL_Window *win, const VkSurfaceCapabilitiesKHR *caps) {
    if (caps->currentExtent.width != UINT32_MAX) {
        return caps->currentExtent;
    }
    int w = 0, h = 0;
    SDL_Vulkan_GetDrawableSize(win, &w, &h);
    if (w <= 0 || h <= 0) {
        w = 640;
        h = 480;
    }
    VkExtent2D e = { (uint32_t)w, (uint32_t)h };
    return e;
}

static VkSwapchainKHR recreate_swapchain(VkDevice dev, VkSurfaceKHR surface,
                                         VkExtent2D extent,
                                         VkSurfaceFormatKHR fmt,
                                         uint32_t image_count,
                                         VkPresentModeKHR pmode,
                                         VkSurfaceTransformFlagsKHR transform,
                                         VkSwapchainKHR old) {
    VkSwapchainCreateInfoKHR sci = {
        .sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR,
        .surface = surface,
        .minImageCount = image_count,
        .imageFormat = fmt.format,
        .imageColorSpace = fmt.colorSpace,
        .imageExtent = extent,
        .imageArrayLayers = 1,
        .imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT,
        .imageSharingMode = VK_SHARING_MODE_EXCLUSIVE,
        .preTransform = transform,
        .compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
        .presentMode = pmode,
        .clipped = VK_TRUE,
        .oldSwapchain = old,
    };
    VkSwapchainKHR sc;
    VkResult r = vkCreateSwapchainKHR(dev, &sci, NULL, &sc);
    if (r != VK_SUCCESS) {
        say("[sdl_vulkan] FAIL: vkCreateSwapchainKHR (recreate)\n");
        return VK_NULL_HANDLE;
    }
    if (old != VK_NULL_HANDLE) vkDestroySwapchainKHR(dev, old, NULL);
    return sc;
}

int main(int argc, char **argv) {
    int sfd = open("/dev/ttyS0", O_WRONLY);
    if (sfd >= 0) { dup2(sfd, 1); dup2(sfd, 2); close(sfd); }

    /* Enable Mesa Venus + Wayland debug logging */
    setenv("VN_DEBUG", "init,result,vtest", 1);
    setenv("WAYLAND_DEBUG", "1", 1);

    say("[sdl_vulkan] starting\n");

    if (SDL_Init(SDL_INIT_VIDEO) < 0) {
        say("[sdl_vulkan] FAIL: SDL_Init\n");
        return 1;
    }
    say("[sdl_vulkan] SDL_Init OK\n");

    char b[256];

    SDL_Window *win = SDL_CreateWindow("sdl_vulkan_test", 100, 100, 640, 480,
                                       SDL_WINDOW_SHOWN | SDL_WINDOW_VULKAN);
    if (!win) {
        say("[sdl_vulkan] FAIL: SDL_CreateWindow\n");
        return 1;
    }
    say("[sdl_vulkan] window created\n");

    /* Watchdog.
     *
     * The test asks for fullscreen, so from that moment it owns the whole
     * output: the compositor unmaps every other window, and the terminal is
     * gone until the test exits. The Vulkan setup can block for a long time
     * (and does, on the virtio-gpu path, where the present fails), so a
     * watchdog exits the process after a fixed time whatever it is doing --
     * otherwise a hung test leaves the guest with an empty desktop.
     *
     * The handler exits rather than returning: the test may be blocked in a
     * Vulkan call, and there is nothing to clean up. */
    {
        struct sigaction sa;
        memset(&sa, 0, sizeof(sa));
        sa.sa_handler = on_alarm;
        sigaction(SIGALRM, &sa, NULL);
        alarm(10);
    }

    /* Request fullscreen from the Wayland compositor (mywima) so the
     * Vulkan swapchain fills the entire output. */
    /* No fullscreen: keep the surface in a ordinary 640x480 window so the
     * compositor keeps its tiling layout and other windows visible. */
    say("[sdl_vulkan] staying windowed (no fullscreen)\n");
    VkApplicationInfo app_info = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "sdl_vulkan_test",
        .applicationVersion = VK_MAKE_VERSION(1, 0, 0),
        .pEngineName = "none",
        .engineVersion = VK_MAKE_VERSION(1, 0, 0),
        .apiVersion = VK_API_VERSION_1_0,
    };

    unsigned int ext_count = 0;
    if (!SDL_Vulkan_GetInstanceExtensions(win, &ext_count, NULL)) {
        say("[sdl_vulkan] FAIL: SDL_Vulkan_GetInstanceExtensions\n");
        return 1;
    }
    const char **exts = calloc(ext_count, sizeof(char *));
    if (!SDL_Vulkan_GetInstanceExtensions(win, &ext_count, exts)) {
        say("[sdl_vulkan] FAIL: SDL_Vulkan_GetInstanceExtensions list\n");
        return 1;
    }

    VkInstanceCreateInfo ci = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &app_info,
        .enabledExtensionCount = ext_count,
        .ppEnabledExtensionNames = exts,
    };

    VkInstance inst;
    VK_CHECK(vkCreateInstance(&ci, NULL, &inst));

    VkSurfaceKHR surface;
    if (!SDL_Vulkan_CreateSurface(win, inst, &surface)) {
        say("[sdl_vulkan] FAIL: SDL_Vulkan_CreateSurface\n");
        return 1;
    }
    say("[sdl_vulkan] surface created\n");

    /* Pump SDL events so mywima can configure the Wayland surface before
     * we create the swapchain. Without this, the surface may not be
     * ready when vkCreateSwapchainKHR is called. */
    SDL_Event ev;
    for (int i = 0; i < 20; i++) {
        while (SDL_PollEvent(&ev)) {
            if (ev.type == SDL_QUIT) goto done;
            if (ev.type == SDL_WINDOWEVENT &&
                ev.window.event == SDL_WINDOWEVENT_CLOSE) goto done;
        }
        SDL_Delay(50);
    }

    uint32_t gpu_count = 0;
    VK_CHECK(vkEnumeratePhysicalDevices(inst, &gpu_count, NULL));
    if (gpu_count == 0) {
        say("[sdl_vulkan] FAIL: no physical devices\n");
        return 1;
    }
    VkPhysicalDevice *gpus = calloc(gpu_count, sizeof(VkPhysicalDevice));
    VK_CHECK(vkEnumeratePhysicalDevices(inst, &gpu_count, gpus));
    VkPhysicalDevice gpu = gpus[0];

    uint32_t qf_count = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(gpu, &qf_count, NULL);
    VkQueueFamilyProperties *qfs = calloc(qf_count, sizeof(VkQueueFamilyProperties));
    vkGetPhysicalDeviceQueueFamilyProperties(gpu, &qf_count, qfs);

    uint32_t qf_index = UINT32_MAX;
    for (uint32_t i = 0; i < qf_count; i++) {
        VkBool32 present = VK_FALSE;
        vkGetPhysicalDeviceSurfaceSupportKHR(gpu, i, surface, &present);
        if (qfs[i].queueFlags & VK_QUEUE_GRAPHICS_BIT && present) {
            qf_index = i;
            break;
        }
    }
    if (qf_index == UINT32_MAX) {
        say("[sdl_vulkan] FAIL: no graphics+present queue family\n");
        return 1;
    }

    float queue_priority = 1.0f;
    VkDeviceQueueCreateInfo qci = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = qf_index,
        .queueCount = 1,
        .pQueuePriorities = &queue_priority,
    };
    const char *dev_exts[] = { "VK_KHR_swapchain" };
    VkDeviceCreateInfo dci = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .queueCreateInfoCount = 1,
        .pQueueCreateInfos = &qci,
        .enabledExtensionCount = 1,
        .ppEnabledExtensionNames = dev_exts,
    };
    VkDevice dev;
    VK_CHECK(vkCreateDevice(gpu, &dci, NULL, &dev));
    say("[sdl_vulkan] device created\n");

    VkQueue queue;
    vkGetDeviceQueue(dev, qf_index, 0, &queue);

    VkSurfaceCapabilitiesKHR caps;
    VK_CHECK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(gpu, surface, &caps));
    snprintf(b, sizeof b, "[sdl_vulkan] surface caps: minImageCount=%u currentExtent=%ux%u\n",
             caps.minImageCount, caps.currentExtent.width, caps.currentExtent.height);
    say(b);

    int win_w = 0, win_h = 0, draw_w = 0, draw_h = 0;
    SDL_GetWindowSize(win, &win_w, &win_h);
    SDL_Vulkan_GetDrawableSize(win, &draw_w, &draw_h);
    snprintf(b, sizeof b,
             "[sdl_vulkan] window=%dx%d drawable=%dx%d\n",
             win_w, win_h, draw_w, draw_h);
    say(b);

    VkExtent2D extent = pick_extent(win, &caps);
    snprintf(b, sizeof b, "[sdl_vulkan] using extent: %ux%u\n", extent.width, extent.height);
    say(b);

    VkSurfaceFormatKHR fmt = { VK_FORMAT_B8G8R8A8_UNORM, VK_COLOR_SPACE_SRGB_NONLINEAR_KHR };
    VkPresentModeKHR pmode = VK_PRESENT_MODE_MAILBOX_KHR;
    uint32_t image_count = caps.minImageCount;

    VkSwapchainKHR swapchain = recreate_swapchain(dev, surface, extent, fmt,
                                                   image_count, pmode,
                                                   caps.currentTransform, VK_NULL_HANDLE);
    if (swapchain == VK_NULL_HANDLE) {
        say("[sdl_vulkan] FAIL: swapchain creation\n");
        return 1;
    }
    say("[sdl_vulkan] swapchain created\n");

    uint32_t sc_image_count = 0;
    VK_CHECK(vkGetSwapchainImagesKHR(dev, swapchain, &sc_image_count, NULL));
    VkImage *sc_images = calloc(sc_image_count, sizeof(VkImage));
    VK_CHECK(vkGetSwapchainImagesKHR(dev, swapchain, &sc_image_count, sc_images));
    snprintf(b, sizeof b, "[sdl_vulkan] swapchain images: %u\n", sc_image_count);
    say(b);

    VkCommandPoolCreateInfo cpci = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .queueFamilyIndex = qf_index,
    };
    VkCommandPool cmd_pool;
    VK_CHECK(vkCreateCommandPool(dev, &cpci, NULL, &cmd_pool));

    VkCommandBufferAllocateInfo cbai = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = cmd_pool,
        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        .commandBufferCount = 1,
    };
    VkCommandBuffer cmd;
    VK_CHECK(vkAllocateCommandBuffers(dev, &cbai, &cmd));

    VkFenceCreateInfo fence_ci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
    VkFence fence;
    VK_CHECK(vkCreateFence(dev, &fence_ci, NULL, &fence));

    /* Bounded run.
     *
     * The test asks for fullscreen, so while it runs it owns the whole
     * output: the compositor unmaps every other window, and the terminal
     * is gone until the test exits. A test that never exits therefore
     * leaves the guest with a permanently empty desktop.
     *
     * The frame count is a bound on the *presented* frames, and the wall
     * clock is the bound that matters when presenting fails (which is what
     * happens when the Vulkan present returns an error, as it does on the
     * virtio-gpu path): the loop then spins without making progress, and
     * only the clock gets it out. */
    const uint32_t max_frames = 150;   /* ~5s at 30fps */
    const uint64_t max_seconds = 5;
    struct timespec t0, tnow;
    clock_gettime(CLOCK_MONOTONIC, &t0);

    for (uint32_t frame = 0; frame < max_frames; frame++) {
        /* Pump SDL events so Wayland configure events are processed */
        SDL_Event ev;
        while (SDL_PollEvent(&ev)) {
            if (ev.type == SDL_QUIT) goto done;
            if (ev.type == SDL_WINDOWEVENT &&
                ev.window.event == SDL_WINDOWEVENT_CLOSE) goto done;
        }

        clock_gettime(CLOCK_MONOTONIC, &tnow);
        if ((uint64_t)(tnow.tv_sec - t0.tv_sec) >= max_seconds) {
            say("[sdl_vulkan] run time limit reached, exiting\n");
            break;
        }

        /* Re-evaluate the window extent every frame so the swapchain matches
         * the size Wayland actually configured (instead of staying at the
         * 640x480 initial extent). Recreate when it changed. */
        int d_w = 0, d_h = 0;
        SDL_Vulkan_GetDrawableSize(win, &d_w, &d_h);
        VkResult cap_r = vkGetPhysicalDeviceSurfaceCapabilitiesKHR(gpu, surface, &caps);
        if (cap_r == VK_SUCCESS) {
            VkExtent2D cand = pick_extent(win, &caps);
            if (cand.width > 0 && cand.height > 0 &&
                (cand.width != extent.width || cand.height != extent.height)) {
                snprintf(b, sizeof b, "[sdl_vulkan] resize: %ux%u (was %ux%u) drawable=%dx%d\n",
                         cand.width, cand.height, extent.width, extent.height, d_w, d_h);
                say(b);
                extent = cand;
                VK_CHECK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(gpu, surface, &caps));
                extent = pick_extent(win, &caps);
                swapchain = recreate_swapchain(dev, surface, extent, fmt,
                                                image_count, pmode,
                                                caps.currentTransform, swapchain);
                if (swapchain == VK_NULL_HANDLE) break;
                VK_CHECK(vkGetSwapchainImagesKHR(dev, swapchain, &sc_image_count, sc_images));
            }
        }

        uint32_t image_index;
        /* Use a finite timeout so we can pump SDL events while waiting
         * for mywima to configure the Wayland surface. Without this,
         * vkAcquireNextImageKHR blocks forever if no images are available. */
        VkResult r = vkAcquireNextImageKHR(dev, swapchain, 100000000, VK_NULL_HANDLE, VK_NULL_HANDLE, &image_index);
        if (r == VK_TIMEOUT) {
            continue; /* pump more SDL events and try again */
        }
        if (r == VK_ERROR_OUT_OF_DATE_KHR) {
            say("[sdl_vulkan] surface out of date, recreating swapchain\n");
            VK_CHECK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(gpu, surface, &caps));
            extent = pick_extent(win, &caps);
            swapchain = recreate_swapchain(dev, surface, extent, fmt,
                                            image_count, pmode,
                                            caps.currentTransform, swapchain);
            if (swapchain == VK_NULL_HANDLE) break;
            VK_CHECK(vkGetSwapchainImagesKHR(dev, swapchain, &sc_image_count, sc_images));
            continue;
        }
        VK_CHECK(r);

        /* Animate clear color as a rotating gradient */
        float t = (frame % 360) * 3.14159f / 180.0f;
        float r_c = 0.5f + 0.5f * cosf(t);
        float g_c = 0.5f + 0.5f * sinf(t);
        float b_c = 0.5f + 0.5f * cosf(t * 2.0f);
        VkClearColorValue clear_color = { { r_c, g_c, b_c, 1.0f } };

        VkCommandBufferBeginInfo cbbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                                          .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
        vkBeginCommandBuffer(cmd, &cbbi);

        VkImageMemoryBarrier barrier1 = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            .srcAccessMask = 0,
            .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
            .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
            .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
            .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .image = sc_images[image_index],
            .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 },
        };
        vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
                             0, 0, NULL, 0, NULL, 1, &barrier1);

        vkCmdClearColorImage(cmd, sc_images[image_index], VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                             &clear_color, 1,
                             &(VkImageSubresourceRange){
                                 .aspectMask = VK_IMAGE_ASPECT_COLOR_BIT,
                                 .baseMipLevel = 0,
                                 .levelCount = 1,
                                 .baseArrayLayer = 0,
                                 .layerCount = 1,
                             });

        VkImageMemoryBarrier barrier2 = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
            .dstAccessMask = VK_ACCESS_MEMORY_READ_BIT,
            .oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
            .newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR,
            .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .image = sc_images[image_index],
            .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 },
        };
        vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
                             0, 0, NULL, 0, NULL, 1, &barrier2);

        vkEndCommandBuffer(cmd);

        VkSubmitInfo si = {
            .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
            .commandBufferCount = 1,
            .pCommandBuffers = &cmd,
        };
        vkResetFences(dev, 1, &fence);
        VkResult sr = vkQueueSubmit(queue, 1, &si, fence);
        snprintf(b, sizeof b, "[sdl_vulkan] vkQueueSubmit frame=%u -> %d\n", frame, (int)sr);
        say(b);
        vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX);

        VkPresentInfoKHR pi = {
            .sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR,
            .swapchainCount = 1,
            .pSwapchains = &swapchain,
            .pImageIndices = &image_index,
        };
        say("[sdl_vulkan] marker-before-present\n");
        fflush(stdout);
        snprintf(b, sizeof b, "[sdl_vulkan] >>> entering vkQueuePresentKHR frame=%u image_idx=%u\n",
                 frame, image_index);
        say(b);
        fflush(stdout);
        VkResult pr = vkQueuePresentKHR(queue, &pi);
        snprintf(b, sizeof b, "[sdl_vulkan] <<< exited vkQueuePresentKHR frame=%u -> %d\n",
                 frame, (int)pr);
        say(b);
        fflush(stdout);
        if (pr == VK_ERROR_OUT_OF_DATE_KHR) {
            say("[sdl_vulkan] present out of date\n");
            VK_CHECK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(gpu, surface, &caps));
            extent = pick_extent(win, &caps);
            swapchain = recreate_swapchain(dev, surface, extent, fmt,
                                            image_count, pmode,
                                            caps.currentTransform, swapchain);
            if (swapchain == VK_NULL_HANDLE) break;
            VK_CHECK(vkGetSwapchainImagesKHR(dev, swapchain, &sc_image_count, sc_images));
            continue;
        }
        if (pr != VK_SUCCESS && pr != VK_SUBOPTIMAL_KHR) {
            snprintf(b, sizeof b, "[sdl_vulkan] FAIL: vkQueuePresentKHR -> %d\n", (int)pr);
            say(b);
            break;
        }

        if (frame % 30 == 0) {
            snprintf(b, sizeof b, "[sdl_vulkan] frame %u presented (color=%.2f,%.2f,%.2f)\n",
                     frame, r_c, g_c, b_c);
            say(b);
        }
    }

    say("[sdl_vulkan] done\n");

done:
    vkDestroyFence(dev, fence, NULL);
    vkDestroyCommandPool(dev, cmd_pool, NULL);
    vkDestroySwapchainKHR(dev, swapchain, NULL);
    vkDestroyDevice(dev, NULL);
    vkDestroySurfaceKHR(inst, surface, NULL);
    vkDestroyInstance(inst, NULL);

    SDL_DestroyWindow(win);
    SDL_Quit();

    say("[sdl_vulkan] exit 0\n");
    return 0;
}