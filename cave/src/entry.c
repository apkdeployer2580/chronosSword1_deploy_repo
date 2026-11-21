/* The cave's front door.
 *
 * `cave_cfg` (declared in guest.h) is written by the host before the guest
 * runs: one byte per feature in `cave.rs::FEATURES` order, non-zero when that
 * feature's hooks were installed. Cave code that is shared between features,
 * or that runs from a hook belonging to another feature, checks here rather
 * than assuming. `cave_entry` is only the link entry point; nothing jumps to
 * it.
 */
#include "guest.h"

struct cave_cfg cave_cfg __attribute__((section(".data"), aligned(16))) = {0};

/* The runtime pieces clang may emit calls to, since we link without a libc.
 * memcpy/memset are the guest's own imports (PLT stubs), which the cave cannot
 * name; these are byte loops, which is fine for the sizes involved. */
void *memcpy(void *dst, const void *src, u32 n)
{
    u8 *d = dst;
    const u8 *s = src;
    while (n--)
        *d++ = *s++;
    return dst;
}

void *memset(void *dst, int c, u32 n)
{
    u8 *d = dst;
    while (n--)
        *d++ = (u8)c;
    return dst;
}

__attribute__((section(".text.cave_entry"))) void cave_entry(void)
{
}
