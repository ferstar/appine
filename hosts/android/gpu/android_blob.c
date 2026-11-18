#define VK_USE_PLATFORM_ANDROID_KHR
#include <android/hardware_buffer.h>
#include <sys/socket.h>
#include <unistd.h>
#include <vulkan/vulkan.h>
#include "android_blob.h"

// A socket carrying an AHardwareBuffer preserves gralloc's metadata and every fd
// in its native handle. The receiver imports it on the same physical device.
int appine_export_android_blob(VkDevice device, VkDeviceMemory memory) {
    PFN_vkGetMemoryAndroidHardwareBufferANDROID get =
        (PFN_vkGetMemoryAndroidHardwareBufferANDROID)vkGetDeviceProcAddr(
            device, "vkGetMemoryAndroidHardwareBufferANDROID");
    VkMemoryGetAndroidHardwareBufferInfoANDROID info = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_GET_ANDROID_HARDWARE_BUFFER_INFO_ANDROID, .memory = memory};
    AHardwareBuffer *buffer;
    if (!get || get(device, &info, &buffer) != VK_SUCCESS)
        return -1;
    int pair[2];
    int fd = -1;
    if (!socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair)) {
        if (!AHardwareBuffer_sendHandleToUnixSocket(buffer, pair[0]))
            fd = pair[1];
        else
            close(pair[1]);
        close(pair[0]);
    }
    AHardwareBuffer_release(buffer);
    return fd;
}
