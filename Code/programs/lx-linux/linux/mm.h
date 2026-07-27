/* linux/mm.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 * Узкий subset управления памятью/страницами под e1000 (jumbo-RX берёт страницы). */
#ifndef _LINUX_MM_H_SHIM
#define _LINUX_MM_H_SHIM

#include <linux/types.h>
#include <linux/gfp.h>
#include <linux/slab.h>
#include <linux/io.h> /* phys_addr_t */

#define PAGE_SHIFT 12
#define PAGE_SIZE  (1UL << PAGE_SHIFT)
#define PAGE_MASK  (~(PAGE_SIZE - 1))
#define PAGE_ALIGN(x) (((x) + PAGE_SIZE - 1) & PAGE_MASK)

struct page; /* непрозрачно: у нас страница = кусок кучи */

void *page_address(const struct page *page);
struct page *virt_to_page(const void *addr);
void  get_page(struct page *page);
void  put_page(struct page *page);
struct page *alloc_pages(gfp_t gfp, unsigned int order);
#define alloc_page(gfp) alloc_pages((gfp), 0)
void  __free_pages(struct page *page, unsigned int order);
#define __free_page(p) __free_pages((p), 0)
phys_addr_t page_to_phys(const struct page *page);

#endif /* _LINUX_MM_H_SHIM */
