#pragma once
#include <stdbool.h>
#include <stdio.h>
enum path { DMA_BUF, OPAQUE_FD, DEDICATED_CONTROL, ANDROID_BLOB };
bool appine_gpu_probe(FILE *out, enum path path);
