/* Detours for the blacksmith transmog (feature "skin"); the C side is
 * skin.c. Every push is an even number of registers so the C calls see an
 * 8-aligned stack. */
        .syntax unified
        .arm
        .text

/* NpcUI_setBlackSmith+0x8c is its `bx lr`, reached after `pop {r4-r8}`, so
 * the stack is the caller's: reset our sub-state and return for it. */
        .global skin_smith_reset_tail
skin_smith_reset_tail:
        push    {r0, r1, r2, r3, ip, lr}
        bl      skin_smith_reset
        pop     {r0, r1, r2, r3, ip, lr}
        bx      lr

/* NpcUI_blackSmithKeyPressed entry (`push {r4-r11,lr}` displaced into the
 * orig_NpcUI_blackSmithKeyPressed trampoline). smithState 5 is ours entirely;
 * everything else is the guest's. The function returns nothing. */
        .global skin_smith_key_entry
skin_smith_key_entry:
        push    {r0, r1, r2, r3, ip, lr}
        bl      skin_smith_key
        cmp     r0, #0
        pop     {r0, r1, r2, r3, ip, lr}
        beq     orig_NpcUI_blackSmithKeyPressed
        bx      lr

/* drawItemBag+0x334: `ldrsb r1,[r8,#0x96]` with r8 = the item whose icon is
 * about to be drawn and [sp,#0x34] the alpha it will be drawn with (0xff, or
 * dimmed when the hero cannot equip it). Ours dims what our pick screens
 * cannot take. With six registers pushed the slot is at [sp,#0x4c]. */
        .global skin_dim_detour
skin_dim_detour:
        push    {r0, r1, r2, r3, ip, lr}
        mov     r0, r8
        ldr     r1, [sp, #0x4c]
        bl      skin_dim_alpha
        str     r0, [sp, #0x4c]
        pop     {r0, r1, r2, r3, ip, lr}
        ldrsb   r1, [r8, #0x96]
        b       drawItemBag+0x338

/* SPR_drawSprite+0x15c: `ldrb r6,[r4,r5]`, the first byte of the next part
 * in the frame's part list (r4 = part data, r5 = offset, r7 = the sprite).
 * The loop re-enters here for every part of every sprite drawn, so the
 * common case — not layer 5, or 방패외형 on — stays in these few
 * instructions; r3 and ip are dead here (both rewritten before their next
 * read). Only the hero's own shield goes through skin_part_byte. `opt` is
 * loaded by the time anything draws: the hero's sprites load through
 * skin_getEquip first. */
        .global skin_hide_layer_detour
skin_hide_layer_detour:
        ldrb    r6, [r4, r5]
        and     ip, r6, #0x1f
        cmp     ip, #5
        bne     1f
        ldr     ip, =opt
        ldrb    ip, [ip, #3]            @ opt.shield_hidden
        cmp     ip, #0
        beq     1f
        push    {r0, r1, r2, r3, ip, lr}
        mov     r0, r7
        mov     r1, r6
        bl      skin_part_byte
        mov     r6, r0
        pop     {r0, r1, r2, r3, ip, lr}
1:      b       SPR_drawSprite+0x160
        .ltorg
