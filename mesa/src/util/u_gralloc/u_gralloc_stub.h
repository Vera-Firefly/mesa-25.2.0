#ifndef MESA_STUB_GRALLOC_H
#define MESA_STUB_GRALLOC_H

#include "u_gralloc_internal.h"
#include <GLES2/gl2.h>

#ifdef __cplusplus
extern "C" {
#endif

struct stub_gralloc_buffer {
    GLuint fbo;
    GLuint tex;
    int width;
    int height;
};

struct stub_gralloc {
    struct u_gralloc base;
    struct stub_gralloc_buffer *buffer;
};

struct u_gralloc *u_gralloc_stub_create(void);

#ifdef __cplusplus
}
#endif

#endif /* MESA_STUB_GRALLOC_H */
