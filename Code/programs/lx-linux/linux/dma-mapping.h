/* linux/dma-mapping.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 *
 * DMA-API: кольца TX/RX e1000 и буферы отображаются для устройства. На VOID настоящий DMA идёт
 * через DMA-capability (SYS_DMA_ALLOC, Веха 51, [[userspace-drivers]]): физически-непрерывная
 * память, для которой phys == адрес для устройства. Тела — в lx_kit.c; здесь API/типы/направления. */
#ifndef _LINUX_DMA_MAPPING_H_SHIM
#define _LINUX_DMA_MAPPING_H_SHIM

#include <linux/types.h>
#include <linux/device.h>
#include <linux/gfp.h> /* gfp_t */

/* Направление переноса. */
enum dma_data_direction {
	DMA_BIDIRECTIONAL = 0,
	DMA_TO_DEVICE     = 1,
	DMA_FROM_DEVICE   = 2,
	DMA_NONE          = 3,
};

#define DMA_BIT_MASK(n) (((n) == 64) ? ~0ULL : ((1ULL << (n)) - 1))
#define DMA_MAPPING_ERROR (~(dma_addr_t)0)

void *dma_alloc_coherent(struct device *dev, size_t size, dma_addr_t *dma_handle, gfp_t gfp);
void  dma_free_coherent(struct device *dev, size_t size, void *vaddr, dma_addr_t dma_handle);
dma_addr_t dma_map_single(struct device *dev, void *ptr, size_t size, int dir);
void  dma_unmap_single(struct device *dev, dma_addr_t addr, size_t size, int dir);
struct page;
dma_addr_t dma_map_page(struct device *dev, struct page *page, size_t offset, size_t size, int dir);
void  dma_unmap_page(struct device *dev, dma_addr_t addr, size_t size, int dir);
int   dma_mapping_error(struct device *dev, dma_addr_t addr);
int   dma_set_mask(struct device *dev, u64 mask);
int   dma_set_coherent_mask(struct device *dev, u64 mask);
int   dma_set_mask_and_coherent(struct device *dev, u64 mask);
void  dma_sync_single_for_cpu(struct device *dev, dma_addr_t addr, size_t size, int dir);
void  dma_sync_single_for_device(struct device *dev, dma_addr_t addr, size_t size, int dir);

#endif /* _LINUX_DMA_MAPPING_H_SHIM */
