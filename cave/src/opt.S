/* Detours for the options rows (feature "options"). Each restates the guest
 * instruction(s) it displaces; the C side is opt.c. Registers named here are
 * what the guest holds at each site — see the disassembly notes in
 * cave/hooks.py. Every push is an even number of registers so the stack stays
 * 8-aligned for the C calls. */
        .syntax unified
        .arm
        .text

/* MainMenuFrame_drawOption+0x164: `movne r3,#0xa` / `moveq r3,#9` (the row
 * count, stored at [sp,#0x44] by the instruction after). Continues at +0x16c.
 * r0-r2 are live (r2 is stored right after). */
        .global opt_draw_count_detour
opt_draw_count_detour:
        push    {r0, r1, r2, r3, ip, lr}
        bl      opt_row_count
        str     r0, [sp, #12]           @ becomes r3
        pop     {r0, r1, r2, r3, ip, lr}
        b       MainMenuFrame_drawOption+0x16c

/* MainMenuFrame_drawOption+0x240: `cmp r4,#8`, the first instruction after
 * `bl Draw_Arrow` in the row loop. r4 = row, sb = the row's text colour. A row
 * of ours is drawn by opt_draw_row and the loop continues at +0x340 (`add
 * r4,r4,#1`); otherwise the displaced compare is redone for the `bgt` at
 * +0x244. */
        .global opt_draw_row_detour
opt_draw_row_detour:
        push    {r0, r1, r2, r3, ip, lr}
        mov     r0, r4
        mov     r1, sb
        bl      opt_draw_row
        cmp     r0, #0
        pop     {r0, r1, r2, r3, ip, lr}
        bne     MainMenuFrame_drawOption+0x340
        cmp     r4, #8
        b       MainMenuFrame_drawOption+0x244

/* MainMenuOptionListKey+0x18: `movne r2,#9` / `moveq r2,#8` (the last row
 * index). r0 = key, r3 = GOT base, both used after. Continues at +0x20. */
        .global opt_listkey_max_detour
opt_listkey_max_detour:
        push    {r0, r1, r2, r3, ip, lr}
        bl      opt_max_index
        str     r0, [sp, #8]            @ becomes r2
        pop     {r0, r1, r2, r3, ip, lr}
        b       MainMenuOptionListKey+0x20

/* MainMenuOptionKey entry (`push {r4-r11,lr}` displaced into the
 * orig_MainMenuOptionKey trampoline). opt_key gets the same three arguments;
 * when it took the event the guest sees nothing and the caller gets 0, the
 * guest's own "nothing happened" value. */
        .global opt_key_entry
opt_key_entry:
        push    {r0, r1, r2, r3, ip, lr}
        bl      opt_key
        cmp     r0, #0
        pop     {r0, r1, r2, r3, ip, lr}
        beq     orig_MainMenuOptionKey
        mov     r0, #0
        bx      lr

/* HERO_add_Exp entry (`push {r4-r10,lr}` displaced into orig_HERO_add_Exp).
 * r1 is the amount; it leaves scaled by 경험치배율. */
        .global opt_exp_entry
opt_exp_entry:
        push    {r0, r1, r2, r3, ip, lr}
        mov     r0, r1
        bl      opt_exp_scale
        str     r0, [sp, #4]            @ becomes r1
        pop     {r0, r1, r2, r3, ip, lr}
        b       orig_HERO_add_Exp

/* MainMenuFrame_drawOption+0x540: `add sp,sp,#0x1a4`, the first word of the
 * epilogue (feature "savexfer"). By now the screen has drawn everything it
 * draws — the list, the scroll bar, the D-pad plates and the OK button — so
 * the save rows' modal goes on top of all of it. The displaced add is then
 * redone and the `pop` at +0x544 ends the function. */
        .global opt_overlay_detour
opt_overlay_detour:
        push    {r0, r1, r2, r3, ip, lr}
        bl      opt_draw_overlay
        pop     {r0, r1, r2, r3, ip, lr}
        add     sp, sp, #0x1a4
        b       MainMenuFrame_drawOption+0x544
