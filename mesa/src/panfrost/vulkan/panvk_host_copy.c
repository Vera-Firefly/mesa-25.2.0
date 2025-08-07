/*
 * Copyright © 2025 Collabora Ltd.
 * SPDX-License-Identifier: MIT
 */

#include "panvk_device.h"
#include "panvk_device_memory.h"
#include "panvk_entrypoints.h"
#include "pan_tiling.h"
#include "panvk_image.h"

#include "vk_object.h"
#include "vk_util.h"

struct image_params {
   struct panvk_image *img;
   void *ptr;
   VkOffset3D offset;
   VkImageSubresourceLayers subres;
};

struct memory_params {
   void *ptr;
   struct vk_image_buffer_layout layout;
};

static void
panvk_interleaved_copy(void *dst, void *src, unsigned size_bl,
                       unsigned block_size_B, enum pan_interleave_zs interleave,
                       bool is_store)
{
   switch (interleave) {
      case PAN_INTERLEAVE_NONE:
         if (is_store)
            memcpy(dst, src, size_bl * block_size_B);
         else
            memcpy(src, dst, size_bl * block_size_B);
         break;
      case PAN_INTERLEAVE_DEPTH:
         assert(block_size_B == 4);
         for (unsigned i = 0; i < size_bl; i++)
            pan_access_image_pixel(dst + i * 4, src + i * 4, 4, interleave,
                                   is_store);
         break;
      case PAN_INTERLEAVE_STENCIL:
         assert(block_size_B == 4);
         for (unsigned i = 0; i < size_bl; i++)
            pan_access_image_pixel(dst + i * 4, src + i, 4, interleave,
                                   is_store);
         break;
   }
}

/* Copy either memory->image or image->memory. The direction is controlled by
 * the memory_to_img argument. */
static void
panvk_copy_image_to_from_memory(struct image_params img,
                                struct memory_params mem,
                                VkExtent3D extent, VkHostImageCopyFlags flags,
                                bool memory_to_img)
{
   /* AFBC should be disabled on images used for host image copy */
   assert(img.img->vk.drm_format_mod == DRM_FORMAT_MOD_LINEAR ||
          img.img->vk.drm_format_mod == DRM_FORMAT_MOD_ARM_16X16_BLOCK_U_INTERLEAVED);
   bool linear = img.img->vk.drm_format_mod == DRM_FORMAT_MOD_LINEAR;

   /* We don't have to care about the multisample layout for image/memory
    * copies. From the Vulkan 1.4.317 spec:
    *
    *    VUID-VkCopyImageToMemoryInfo-srcImage-07973 srcImage must have a sample
    *    count equal to VK_SAMPLE_COUNT_1_BIT
    *
    *    VUID-VkCopyMemoryToImageInfo-dstImage-07973 dstImage must have a sample
    *    count equal to VK_SAMPLE_COUNT_1_BIT
    */
   assert(img.img->vk.samples == VK_SAMPLE_COUNT_1_BIT);

   /* From the Vulkan 1.4.317 spec:
    *
    *    VUID-VkImageToMemoryCopy-aspectMask-09103 The aspectMask member of
    *    imageSubresource must only have a single bit set
    */
   assert(util_bitcount(img.subres.aspectMask) == 1);
   unsigned plane_idx =
      panvk_plane_index(img.img->vk.format, img.subres.aspectMask);
   assert(plane_idx < PANVK_MAX_PLANES);
   struct panvk_image_plane *plane = &img.img->planes[plane_idx];
   const struct pan_image_layout *plane_layout = &plane->plane.layout;
   const struct pan_image_slice_layout *slice_layout =
      &plane_layout->slices[img.subres.mipLevel];

   /* D24S8 is a special case because the aspects are interleaved in a single
    * plane */
   VkFormat vkfmt = img.img->vk.format == VK_FORMAT_D24_UNORM_S8_UINT ?
      img.img->vk.format :
      vk_format_get_aspect_format(img.img->vk.format, img.subres.aspectMask);
   enum pipe_format pfmt = vk_format_to_pipe_format(vkfmt);
   const struct util_format_description *fmt = util_format_description(pfmt);

   enum pan_interleave_zs interleave = pan_get_interleave_zs(
      pfmt, img.subres.aspectMask & VK_IMAGE_ASPECT_DEPTH_BIT,
      img.subres.aspectMask & VK_IMAGE_ASPECT_STENCIL_BIT);

   unsigned block_width_px = fmt->block.width;
   unsigned block_height_px = fmt->block.height;
   assert(fmt->block.bits % 8 == 0);
   unsigned block_size_B = fmt->block.bits / 8;
   /* With stencil interleave, the memory element size will be smaller than the
    * image block size */
   if (interleave != PAN_INTERLEAVE_STENCIL)
      assert(mem.layout.element_size_B == block_size_B);

   unsigned row_size_bl = DIV_ROUND_UP(extent.width, block_width_px);

   unsigned layer_count =
      vk_image_subresource_layer_count(&img.img->vk, &img.subres);

   void *img_base_ptr = img.ptr + plane->offset + slice_layout->offset_B;
   for (unsigned layer = 0; layer < layer_count; layer++) {
      unsigned img_layer = layer + img.subres.baseArrayLayer;
      void *img_layer_ptr = img_base_ptr +
         img_layer * plane_layout->array_stride_B;
      void *mem_layer_ptr = mem.ptr + layer * mem.layout.image_stride_B;

      if (flags & VK_HOST_IMAGE_COPY_MEMCPY_BIT) {
         /* For depth/stencil interleave, we can't use a plain memcpy, but we
          * can still get some performance benefit by skipping (de)tiling and
          * strided copy logic. */
         panvk_interleaved_copy(img_layer_ptr, mem_layer_ptr,
                                slice_layout->size_B / block_size_B,
                                block_size_B,interleave, memory_to_img);
         continue;
      }

      for (unsigned z = 0; z < extent.depth; z++) {
         unsigned img_z = z + img.offset.z;
         void *img_depth_ptr = img_layer_ptr +
            img_z * slice_layout->tiled_or_linear.surface_stride_B;
         /* There is no distinction between array and 3D images in the memory
          * layout, image_stride_B applies to both */
         void *mem_depth_ptr = mem_layer_ptr + z * mem.layout.image_stride_B;

         if (linear) {
            for (unsigned y = 0; y < extent.height; y += block_height_px) {
               unsigned img_y_bl = (y + img.offset.y) / block_height_px;
               unsigned mem_y_bl = y / block_height_px;
               void *img_row_ptr = img_depth_ptr +
                  img_y_bl * slice_layout->tiled_or_linear.row_stride_B;
               void *mem_row_ptr = mem_depth_ptr +
                  mem_y_bl * mem.layout.row_stride_B;

               unsigned img_x_bl = img.offset.x / block_width_px;
               void *img_block_ptr = img_row_ptr + img_x_bl * block_size_B;

               panvk_interleaved_copy(img_block_ptr, mem_row_ptr,
                                      row_size_bl, block_size_B, interleave,
                                      memory_to_img);
            }
         } else {
            if (memory_to_img)
               pan_store_tiled_image(
                  img_depth_ptr, mem_depth_ptr,
                  img.offset.x, img.offset.y, extent.width, extent.height,
                  slice_layout->tiled_or_linear.row_stride_B,
                  mem.layout.row_stride_B,
                  pfmt, interleave);
            else
               pan_load_tiled_image(
                  mem_depth_ptr, img_depth_ptr,
                  img.offset.x, img.offset.y, extent.width, extent.height,
                  mem.layout.row_stride_B,
                  slice_layout->tiled_or_linear.row_stride_B,
                  pfmt, interleave);
         }
      }
   }
}

static void
panvk_copy_memory_to_image(struct panvk_image *dst, void *dst_cpu,
                           const VkMemoryToImageCopy *region,
                           VkHostImageCopyFlags flags)
{
   struct memory_params src_params = {
      /* Casting away const, but we don't write to it so it's fine */
      .ptr = (void *) region->pHostPointer,
      .layout = vk_memory_to_image_copy_layout(&dst->vk, region),
   };
   struct image_params dst_params = {
      .img = dst,
      .ptr = dst_cpu,
      .offset = region->imageOffset,
      .subres = region->imageSubresource,
   };

   panvk_copy_image_to_from_memory(
      dst_params, src_params, region->imageExtent, flags, true);
}

VKAPI_ATTR VkResult VKAPI_CALL
panvk_CopyMemoryToImage(VkDevice device, const VkCopyMemoryToImageInfo *info)
{
   VK_FROM_HANDLE(panvk_device, dev, device);
   VK_FROM_HANDLE(panvk_image, dst, info->dstImage);

   void *dst_cpu = pan_kmod_bo_mmap(
      dst->mem->bo, 0, pan_kmod_bo_size(dst->mem->bo), PROT_WRITE, MAP_SHARED,
      NULL);
   if (dst_cpu == MAP_FAILED)
      return panvk_errorf(dev, VK_ERROR_MEMORY_MAP_FAILED,
                          "Failed to CPU map image");

   for (unsigned i = 0; i < info->regionCount; i++) {
      panvk_copy_memory_to_image(dst, dst_cpu, &info->pRegions[i],
                                 info->flags);
   }

   ASSERTED int ret = os_munmap(dst_cpu, pan_kmod_bo_size(dst->mem->bo));
   assert(!ret);

   return VK_SUCCESS;
}

static void
panvk_copy_image_to_memory(struct panvk_image *src, void *src_cpu,
                           const VkImageToMemoryCopy *region,
                           VkHostImageCopyFlags flags)
{
   struct memory_params dst_params = {
      .ptr = region->pHostPointer,
      .layout = vk_image_to_memory_copy_layout(&src->vk, region),
   };
   struct image_params src_params = {
      .img = src,
      .ptr = src_cpu,
      .offset = region->imageOffset,
      .subres = region->imageSubresource,
   };

   panvk_copy_image_to_from_memory(
      src_params, dst_params, region->imageExtent, flags, false);
}

VKAPI_ATTR VkResult VKAPI_CALL
panvk_CopyImageToMemory(VkDevice device, const VkCopyImageToMemoryInfo *info)
{
   VK_FROM_HANDLE(panvk_device, dev, device);
   VK_FROM_HANDLE(panvk_image, src, info->srcImage);

   void *src_cpu = pan_kmod_bo_mmap(
      src->mem->bo, 0, pan_kmod_bo_size(src->mem->bo), PROT_READ, MAP_SHARED,
      NULL);
   if (src_cpu == MAP_FAILED)
      return panvk_errorf(dev, VK_ERROR_MEMORY_MAP_FAILED,
                          "Failed to CPU map image");

   for (unsigned i = 0; i < info->regionCount; i++) {
      panvk_copy_image_to_memory(src, src_cpu, &info->pRegions[i],
                                 info->flags);
   }

   ASSERTED int ret = os_munmap(src_cpu, pan_kmod_bo_size(src->mem->bo));
   assert(!ret);

   return VK_SUCCESS;
}

static void
panvk_copy_image_to_image(struct panvk_image *dst, void *dst_cpu,
                          struct panvk_image *src, void *src_cpu,
                          const VkImageCopy2 *region,
                          VkHostImageCopyFlags flags)
{
   /* AFBC should be disabled on images used for host image copy */
   assert(src->vk.drm_format_mod == DRM_FORMAT_MOD_LINEAR ||
          src->vk.drm_format_mod == DRM_FORMAT_MOD_ARM_16X16_BLOCK_U_INTERLEAVED);
   assert(dst->vk.drm_format_mod == DRM_FORMAT_MOD_LINEAR ||
          dst->vk.drm_format_mod == DRM_FORMAT_MOD_ARM_16X16_BLOCK_U_INTERLEAVED);
   bool src_linear = src->vk.drm_format_mod == DRM_FORMAT_MOD_LINEAR;
   bool dst_linear = dst->vk.drm_format_mod == DRM_FORMAT_MOD_LINEAR;

   VkImageSubresourceLayers src_subres = region->srcSubresource;
   VkImageSubresourceLayers dst_subres = region->dstSubresource;

   unsigned src_plane_idx =
      panvk_plane_index(src->vk.format, src_subres.aspectMask);
   unsigned dst_plane_idx =
      panvk_plane_index(dst->vk.format, dst_subres.aspectMask);
   assert(src_plane_idx < PANVK_MAX_PLANES);
   assert(dst_plane_idx < PANVK_MAX_PLANES);
   struct panvk_image_plane *src_plane = &src->planes[src_plane_idx];
   struct panvk_image_plane *dst_plane = &dst->planes[dst_plane_idx];
   const struct pan_image_layout *src_plane_layout = &dst_plane->plane.layout;
   const struct pan_image_layout *dst_plane_layout = &src_plane->plane.layout;
   const struct pan_image_slice_layout *src_slice_layout =
      &src_plane_layout->slices[src_subres.mipLevel];
   const struct pan_image_slice_layout *dst_slice_layout =
      &dst_plane_layout->slices[dst_subres.mipLevel];

   VkFormat src_vkfmt =
      vk_format_get_aspect_format(src->vk.format, src_subres.aspectMask);
   VkFormat dst_vkfmt =
      vk_format_get_aspect_format(dst->vk.format, dst_subres.aspectMask);
   enum pipe_format src_pfmt = vk_format_to_pipe_format(src_vkfmt);
   enum pipe_format dst_pfmt = vk_format_to_pipe_format(dst_vkfmt);
   const struct util_format_description *src_fmt =
      util_format_description(src_pfmt);
   const struct util_format_description *dst_fmt =
      util_format_description(dst_pfmt);

   unsigned block_width_px = src_fmt->block.width;
   unsigned block_height_px = src_fmt->block.height;
   assert(src_fmt->block.bits % 8 == 0);
   unsigned block_size_B = src_fmt->block.bits / 8;

   /* This doesn't actually seem to be a requirement in the spec, but that's
    * probably unintentional */
   assert(dst_fmt->block.width == block_width_px);
   assert(dst_fmt->block.height == block_height_px);
   assert(dst_fmt->block.bits == src_fmt->block.bits);

   unsigned row_size_bl = DIV_ROUND_UP(region->extent.width, block_width_px);
   unsigned row_size_B = row_size_bl * block_size_B;

   unsigned src_layer_count =
      vk_image_subresource_layer_count(&src->vk, &src_subres);
   unsigned dst_layer_count =
      vk_image_subresource_layer_count(&dst->vk, &dst_subres);
   /* This also is not explicitly required in the spec */
   assert(src_layer_count == dst_layer_count);
   unsigned layer_count = src_layer_count;

   unsigned sample_count = src->vk.samples;
   /* This also is not explicitly required in the spec */
   assert(dst->vk.samples == sample_count);
   /* Multisampled images are implemented as 3D */
   unsigned depth = sample_count > 1 ? sample_count : region->extent.depth;

   void *src_base_ptr =
      src_cpu + src_plane->offset + src_slice_layout->offset_B;
   void *dst_base_ptr =
      dst_cpu + dst_plane->offset + dst_slice_layout->offset_B;
   for (unsigned layer = 0; layer < layer_count; layer++) {
      unsigned src_layer = layer + src_subres.baseArrayLayer;
      unsigned dst_layer = layer + dst_subres.baseArrayLayer;
      void *src_layer_ptr = src_base_ptr +
         src_layer * src_slice_layout->tiled_or_linear.surface_stride_B;
      void *dst_layer_ptr = dst_base_ptr +
         dst_layer * dst_slice_layout->tiled_or_linear.surface_stride_B;

      if (flags & VK_HOST_IMAGE_COPY_MEMCPY_BIT) {
         assert(src_slice_layout->size_B == dst_slice_layout->size_B);
         memcpy(dst_layer_ptr, src_layer_ptr, src_slice_layout->size_B);
         continue;
      }

      for (unsigned z = 0; z < depth; z++) {
         unsigned src_z = z + region->srcOffset.z;
         unsigned dst_z = z + region->dstOffset.z;
         void *src_depth_ptr = src_layer_ptr +
            src_z * src_slice_layout->tiled_or_linear.surface_stride_B;
         void *dst_depth_ptr = dst_layer_ptr +
            dst_z * dst_slice_layout->tiled_or_linear.surface_stride_B;

         if (src_linear && dst_linear) {
            for (unsigned y = 0; y < region->extent.height;
                 y += block_height_px) {
               unsigned src_y_bl = (y + region->srcOffset.y) / block_height_px;
               unsigned dst_y_bl = (y + region->dstOffset.y) / block_height_px;
               unsigned src_x_bl = region->srcOffset.x / block_width_px;
               unsigned dst_x_bl = region->dstOffset.x / block_width_px;
               void *src_row_ptr = src_depth_ptr +
                  src_y_bl * src_slice_layout->tiled_or_linear.row_stride_B +
                  src_x_bl * block_size_B;
               void *dst_row_ptr = dst_depth_ptr +
                  dst_y_bl * dst_slice_layout->tiled_or_linear.row_stride_B +
                  dst_x_bl * block_size_B;

               memcpy(dst_row_ptr, src_row_ptr, row_size_B);
            }
         } else if (src_linear && !dst_linear) {
            unsigned src_y_bl = region->srcOffset.y / block_height_px;
            unsigned src_x_bl = region->srcOffset.x / block_width_px;
            void *src_row_ptr = src_depth_ptr +
               src_y_bl * src_slice_layout->tiled_or_linear.row_stride_B +
               src_x_bl * block_size_B;
            pan_store_tiled_image(
               dst_depth_ptr, src_row_ptr,
               region->dstOffset.x, region->dstOffset.y,
               region->extent.width, region->extent.height,
               dst_slice_layout->tiled_or_linear.row_stride_B,
               src_slice_layout->tiled_or_linear.row_stride_B,
               src_pfmt, PAN_INTERLEAVE_NONE);
         } else if (!src_linear && dst_linear) {
            unsigned dst_y_bl = region->dstOffset.y / block_height_px;
            unsigned dst_x_bl = region->dstOffset.x / block_width_px;
            void *dst_row_ptr = dst_depth_ptr +
               dst_y_bl * dst_slice_layout->tiled_or_linear.row_stride_B +
               dst_x_bl * block_size_B;
            pan_load_tiled_image(
               dst_row_ptr, src_depth_ptr,
               region->srcOffset.x, region->srcOffset.y,
               region->extent.width, region->extent.height,
               dst_slice_layout->tiled_or_linear.row_stride_B,
               src_slice_layout->tiled_or_linear.row_stride_B,
               dst_pfmt, PAN_INTERLEAVE_NONE);
         } else {
            pan_copy_tiled_image(
               dst_depth_ptr, src_depth_ptr, region->dstOffset.x,
               region->dstOffset.y, region->srcOffset.x, region->srcOffset.y,
               region->extent.width, region->extent.height,
               dst_slice_layout->tiled_or_linear.row_stride_B,
               src_slice_layout->tiled_or_linear.row_stride_B, src_pfmt);
         }
      }
   }
}

VKAPI_ATTR VkResult VKAPI_CALL
panvk_CopyImageToImage(VkDevice device, const VkCopyImageToImageInfo *info)
{
   VkResult result = VK_SUCCESS;

   VK_FROM_HANDLE(panvk_device, dev, device);
   VK_FROM_HANDLE(panvk_image, dst, info->dstImage);
   VK_FROM_HANDLE(panvk_image, src, info->srcImage);

   void *dst_cpu = pan_kmod_bo_mmap(
      dst->mem->bo, 0, pan_kmod_bo_size(dst->mem->bo), PROT_WRITE, MAP_SHARED,
      NULL);
   if (dst_cpu == MAP_FAILED)
      return panvk_errorf(dev, VK_ERROR_MEMORY_MAP_FAILED,
                          "Failed to CPU map image");

   void *src_cpu = pan_kmod_bo_mmap(
      src->mem->bo, 0, pan_kmod_bo_size(src->mem->bo), PROT_READ, MAP_SHARED,
      NULL);
   if (src_cpu == MAP_FAILED) {
      result = panvk_errorf(dev, VK_ERROR_MEMORY_MAP_FAILED,
                            "Failed to CPU map image");
      goto unmap_dst;
   }

   for (unsigned i = 0; i < info->regionCount; i++) {
      panvk_copy_image_to_image(dst, dst_cpu, src, src_cpu, &info->pRegions[i],
                                info->flags);
   }

   ASSERTED int ret = os_munmap(src_cpu, pan_kmod_bo_size(src->mem->bo));
   assert(!ret);
unmap_dst:
   ret = os_munmap(dst_cpu, pan_kmod_bo_size(dst->mem->bo));
   assert(!ret);

   return result;
}

VKAPI_ATTR VkResult VKAPI_CALL
panvk_TransitionImageLayout(VkDevice device, uint32_t transitionCount,
                            const VkHostImageLayoutTransitionInfo *transitions)
{
   /* We don't use image layouts, this is a no-op */
   return VK_SUCCESS;
}
