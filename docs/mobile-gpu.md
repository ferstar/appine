# Mobile GPU support for Alpine and Debian guests

On a Samsung Galaxy S23, an unprivileged Alpine or Debian shell can run headless Vulkan buffer compute on the Adreno 740 using the distribution's unmodified Mesa Venus driver. Appine provides the DRM render node and the Android renderer. The loader discovers the driver through the distribution's normal ICD manifest.

## Run it

Install the packages as the guest administrator. In Alpine use `apk add mesa-vulkan-virtio vulkan-tools`. In Debian use `apt-get install mesa-vulkan-drivers vulkan-tools`. Then run `vulkaninfo --summary` from the regular user's shell. Look for `Virtio-GPU Venus (Adreno (TM) 740)`, `PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU` and `DRIVER_ID_MESA_VENUS`. If the distribution also installed Lavapipe, the tool lists a second device named llvmpipe. Headless runs may print warnings about the display.

For a short command that requires the hardware GPU, prefix the command with `appine-gpu`. For example, `appine-gpu vulkaninfo --summary` lists the Venus device. The launcher sets `VK_DRIVER_FILES` to the packaged Venus manifest for that invocation. Run `vulkaninfo --summary` directly to list all installed drivers. Appine installs the launcher in new and existing guests at machine startup.

The tested workload uses ordinary storage buffers, a compute shader and a fence. Window presentation, OpenGL, OpenCL, external image sharing and PRIME fd exchange require additional interfaces.

## How a submission reaches the phone

1. At machine startup the private Android `:venus` service tests Vulkan export, reimport, GPU copy, fence completion and CPU readback. Appine registers `/dev/dri/renderD128` (226:128, mode 0666) and the platform device in sysfs when the probe and mapper initialization succeed. The CLI starts without a renderer. Both packaged libdrm versions inspect `/sys/dev/char/226:128/device/drm`, its sibling `subsystem` link and the device's `uevent`.
2. Each open file description has a Venus context, a private handle table and its own command connection. Duplicated descriptors share them. The kernel implements the DRM version, parameter, capset, context, blob, map, resource-info, execbuffer and GEM-close requests. The two packaged ICDs emulate sync objects with output fences, poll and fd duplication. Appine returns the decoder's Venus capset and reports zero PRIME, cross-device and kernel syncobj support. [Mesa Venus documentation](https://docs.mesa3d.org/drivers/venus.html) · [Kernel ioctl implementation](../core/kernel/src/syscall/gpu.rs)
3. A dedicated service thread decodes each connection using virglrenderer 1.3.0 and the system Vulkan loader. The service is nonexported and runs in a separate process under Appine's ordinary Android UID. On the S23, a service using an Android isolated UID received `EACCES` when the vendor Vulkan implementation opened its GPU device. [Android service](../android/app/src/main/java/io/github/mthli/appine/GpuService.kt) · [Renderer worker](../hosts/android/gpu/worker.c)
4. Venus command and reply rings use shared memory. For host-visible ordinary buffer allocations, the patched decoder requests an exportable Android Hardware Buffer BLOB. The application receives its native handle over a Unix socket, checks its format, size, memory type and physical-device UUID, imports it on a second logical device, and maps coherent memory into the guest page table. The decoder hides host-visible memory types that this mapper cannot keep coherent. This path supports multiple buffers bound at different offsets in one allocation. [Decoder patch](../hosts/android/gpu/android-blob.patch) · [Mapper](../hosts/android/gpu/mapper.c) · [Vulkan device memory](https://docs.vulkan.org/spec/latest/chapters/memory.html)
5. A guest VMA keeps its mapping through GEM close, descriptor close and fork. Execbuffer retains its referenced buffers until the ring fence retires. Fence waits and driver cleanup run outside the big kernel lock. Context shutdown stops the command rings and drains outstanding GPU work before destroying Vulkan objects. [Host lifetimes](../hosts/android/src/gpu.rs) · [Guest mappings](../core/kernel/src/mm.rs)

## Measurements on the Galaxy S23

SM-S9110, firmware `S9110ZCS8FZJ1`, Android API 36, 4,096-byte host pages (the firmware reports `ro.product.cpu.pagesize.max=4096`), Adreno 740, Vulkan 1.3.128, vendor driver `0x802a4001`. The native probe ran with ordinary app privileges. DMA-BUF failed at the missing `VK_EXT_external_memory_dma_buf` extension. Generic opaque-fd export required a dedicated resource. Its dedicated-buffer control passed. Android Hardware Buffer export/import reported feature bits `0x6` and passed a two-buffer, two-device copy with all 1,024 words verified.

The guest tests ran with Tier 1 in independent temporary stores as UID 1000. Both the packaged `vulkaninfo --summary` and the compute executable ran without driver-selection or debug environment overrides. The sample uploads 256 KiB, dispatches 1,024 workgroups, waits for a fence and checks all 65,536 output words. End-to-end time includes loader, device and pipeline setup through final verification. These are individual measured runs.

| Guest | ICD manifest | Mesa | Upload | Queue submit | Fence wait | Invalidate and check | End to end |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: |
| Alpine 3.24 | `virtio_icd.aarch64.json` | 26.1.6-r0 | 0.071 ms | 0.581 ms | 9.353 ms | 0.200 ms | 287.975 ms |
| Debian trixie | `virtio_icd.json` | 25.0.7-2+deb13u1 | 0.069 ms | 0.427 ms | 0.629 ms | 0.161 ms | 203.833 ms |

A separate comparison timed the same workload in fresh processes in a warmed guest, from Vulkan instance creation through result verification. With Lavapipe also installed, median time fell from 132 to 68 ms on Debian and from 106 to 56 ms on Alpine when the process selected only the Venus manifest. Each median covers 12 runs with the configuration order reversed between two guest starts. The comparison set `VK_DRIVER_FILES` directly and timed the compute program itself.

A second test on the debug APK timed each command from fork through process exit, including the launcher. After four warm-up runs, 12 runs per mode gave medians of 117.2 ms directly and 57.9 ms with `appine-gpu` on Debian, and 113.4 ms directly and 54.1 ms with the launcher on Alpine. All runs verified 65,536 words.

Both distributions also passed packaged discovery and the compute check against the R8-shrunk release APK. Both passed the shared-mapping test after GEM close, descriptor close and fork. With two concurrent clients, one process received SIGKILL after confirming its submitted fence was unsignaled. The shell stayed responsive, the survivor verified all words, and a fresh machine completed the same computation. The instrumentation checked the main process for leftover Venus shared mappings and retirement threads after shutdown.

Separate guest `strace` runs on the S23 release APK verified all 65,536 words in both distributions. Each trace showed five blob creations, five map ioctls and five GEM closes. Alpine made 43 execbuffer calls, duplicated a fence fd with `F_DUPFD_CLOEXEC` and observed `POLLIN` on the duplicate through `ppoll`. Debian made 26 execbuffer calls and completed this run without a `ppoll` call. Neither run issued a DRM syncobj or PRIME ioctl. The timing table uses the untraced runs.

On the API 37, 16,384-byte-page emulator, Goldfish GFXStream reported zero Android Hardware Buffer external-buffer features with both llvmpipe and host acceleration on an Apple M2 Pro. The negative test confirmed that the guest could start and both the render node and corresponding sysfs path were absent. Physical Mali and positive hardware-backed 16 KiB results remain unverified.

## Reproduce

Build the Android app using the [Android build guide](../android/README.md#building). The disposable fixture builder uses ARM64 Docker images and the pinned DRM headers for the low-level lifetime test. The Vulkan sample links the selected distribution's own loader. From the repository root with a debug build installed on the selected device:

```sh
scripts/fetch-virglrenderer
android/gradlew -p android installDebug installDebugAndroidTest
mkdir -p target/gpu-survey
for distro in alpine debian; do
    tests/survey/gpu/fixture "$distro" "target/gpu-survey/venus-$distro.tgz"
    adb push "target/gpu-survey/venus-$distro.tgz" "/data/local/tmp/venus-$distro.tgz"
    adb shell run-as io.github.mthli.appine mkdir -p cache
    adb shell run-as io.github.mthli.appine cp "/data/local/tmp/venus-$distro.tgz" cache/
    adb shell am instrument --user 0 -w -r -e gpuDistro "$distro" \
        -e class io.github.mthli.appine.GpuTest#distributionVenus \
        io.github.mthli.appine.test/androidx.test.runner.AndroidJUnitRunner
done
adb logcat -d -s appine-gpu-test:I
```

For the native driver probe, set `ANDROID_HOME` to the Android SDK directory. Each invocation reports the device, extensions, first failing operation or verified word count.

```sh
android/gradlew -p tests/survey/gpu/android assembleDebug
adb install -r --user 0 tests/survey/gpu/android/build/outputs/apk/debug/gpu-probe-debug.apk
for path in dma-buf opaque-fd dedicated-control android-blob; do
    adb shell am instrument --user 0 -w -r -e path "$path" io.github.mthli.appine.gpuprobe/.Probe
done
```
