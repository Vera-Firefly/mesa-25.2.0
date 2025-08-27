#include <stdlib.h>
#include <GLES2/gl2.h>
#include "u_gralloc_stub.h"
#include "util/log.h"
#include "util/u_memory.h"

static int stub_get_buffer_info(struct u_gralloc *base,
                                struct u_gralloc_buffer_handle *hnd,
                                struct u_gralloc_buffer_basic_info *out) {
    struct stub_gralloc *gr = (struct stub_gralloc*)base;
    out->drm_fourcc = 0;
    out->modifier = 0;
    out->num_planes = 1;
    out->fds[0] = 0;
    out->offsets[0] = 0;
    out->strides[0] = gr->buffer->width;
    return 0;
}

static int stub_get_front_rendering_usage(struct u_gralloc *base, uint64_t *out_usage) {
    *out_usage = 0;
    return 0;
}

static int stub_destroy(struct u_gralloc *base) {
    struct stub_gralloc *gr = (struct stub_gralloc*)base;
    if (gr->buffer) {
        if (gr->buffer->fbo) glDeleteFramebuffers(1, &gr->buffer->fbo);
        if (gr->buffer->tex) glDeleteTextures(1, &gr->buffer->tex);
        free(gr->buffer);
    }
    free(gr);
    return 0;
}

struct u_gralloc *u_gralloc_stub_create(void)
{
    struct stub_gralloc *gr = CALLOC_STRUCT(stub_gralloc);
    if (!gr) return NULL;

    gr->buffer = CALLOC_STRUCT(stub_gralloc_buffer);
    if (!gr->buffer) {
        free(gr);
        return NULL;
    }

    gr->buffer->width = 512;
    gr->buffer->height = 512;

    glGenTextures(1, &gr->buffer->tex);
    glBindTexture(GL_TEXTURE_2D, gr->buffer->tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, gr->buffer->width,
                 gr->buffer->height, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);

    glGenFramebuffers(1, &gr->buffer->fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, gr->buffer->fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D, gr->buffer->tex, 0);

    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        mesa_loge("FBO incomplete");
        stub_destroy(&gr->base);
        return NULL;
    }

    gr->base.ops.get_buffer_basic_info = stub_get_buffer_info;
    gr->base.ops.get_front_rendering_usage = stub_get_front_rendering_usage;
    gr->base.ops.destroy = stub_destroy;

    mesa_logi("Using stub gralloc (AHardwareBuffer replacement)");

    return &gr->base;
}
