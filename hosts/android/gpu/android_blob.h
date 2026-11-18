#ifndef APPINE_ANDROID_BLOB_H
#define APPINE_ANDROID_BLOB_H

// The caller includes either the Venus protocol headers or the Android Vulkan headers.
int appine_export_android_blob(VkDevice device, VkDeviceMemory memory);
#endif
