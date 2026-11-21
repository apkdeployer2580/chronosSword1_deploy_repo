/* Ten bag pages: the detours that turn the five-tab column into two columns of
 * five and make the keys/touches address all ten. One entry per hooked site,
 * each ending with a branch back to the instruction after the ones it
 * displaced. The displaced instructions are re-stated here verbatim, and
 * hooks.py refuses to build if the site does not hold the words we expect.
 *
 * Geometry (screen centre W/2, H/2; 27 px tab pitch):
 *   column 0 at x = W/2 - 0x6c, column 1 at x = W/2 - 0x51 (the old column)
 *   row r at y = H/2 - 0x45 + 27 r, r = tab % 5
 *   the cursor mark sits 16 px left of its tab: x = W/2 - 0x61 (col 1),
 *   W/2 - 0x7c (col 0)
 * The tab index register and the y-offset register differ per site; each block
 * names them.
 */
        .syntax unified
        .arm
        .text

/* ---- drawItemBag ------------------------------------------------------------ */

/* +0x8b8: tab icon. r7 = tab, r6 = 27*tab, r1 = W/2, r2 = H/2-0x45.
 * displaced: sub r1,r1,#0x51 ; add r2,r2,r6 */
        .global bag10_draw_tab_a
bag10_draw_tab_a:
        add     r2, r2, r6
        cmp     r7, #5
        subge   r2, r2, #0x87
        sub     r1, r1, #0x6c
        addge   r1, r1, #0x1b
        b       drawItemBag+0x8c0

/* +0x948: cursor mark. r3 = current bag, r8 = 0x1b, r2 = H/2-0x43, r1 = W (not halved yet).
 * displaced: mla r2,r8,r3,r2 ; mov r3,lr ; ldr lr,[ip,#4] ; asr r1,r1,#1 ;
 *            ldr r0,[r0,#4] ; mov r8,#0xff ; sub r1,r1,#0x61 */
        .global bag10_draw_cursor_a
bag10_draw_cursor_a:
        cmp     r3, #5
        subge   r3, r3, #5
        mla     r2, r8, r3, r2
        mov     r3, lr
        ldr     lr, [ip, #4]
        asr     r1, r1, #1
        ldr     r0, [r0, #4]
        mov     r8, #0xff
        sub     r1, r1, #0x61
        sublt   r1, r1, #0x1b
        b       drawItemBag+0x964

/* +0xa1c: the open tab's icon. r7 = tab, r6 = 27*tab.
 * displaced: sub r1,r1,#0x51 ; add r2,r6,r2 */
        .global bag10_draw_tab_c
bag10_draw_tab_c:
        add     r2, r6, r2
        cmp     r7, #5
        subge   r2, r2, #0x87
        sub     r1, r1, #0x6c
        addge   r1, r1, #0x1b
        b       drawItemBag+0xa24

/* ---- ItemBagKey --------------------------------------------------------------- */

/* +0x94, tab-select mode, r5 = key. LEFT/'4' and RIGHT/'6' hop between the
 * columns; every other key takes the original jump table.
 * displaced: add r5,r5,#0x10 */
        .global bag10_key_column
bag10_key_column:
        ldr     r0, =storage10
        ldrb    r1, [r0, #1]            @ current bag
        ldrsb   r2, [r0, #2]            @ bag count
        cmn     r5, #3
        beq     1f
        cmp     r5, #0x34
        beq     1f
        cmn     r5, #4
        beq     2f
        cmp     r5, #0x36
        beq     2f
        add     r5, r5, #0x10
        b       ItemBagKey+0x98
1:      cmp     r1, #5
        subge   r1, r1, #5
        b       3f
2:      add     r3, r1, #5
        cmp     r3, r2
        movlt   r1, r3
3:      strb    r1, [r0, #1]
        b       ItemBagKey+0x50          @ return 0

/* +0x2f4: touch hit-test rectangle of tab r7. r8 = 27*tab, r2 = W/2, r3 = H/2-0x45.
 * displaced: add r3,r3,r8 ; sub r2,r2,#0x51 */
        .global bag10_touch_tab
bag10_touch_tab:
        add     r3, r3, r8
        cmp     r7, #5
        subge   r3, r3, #0x87
        sub     r2, r2, #0x6c
        addge   r2, r2, #0x1b
        b       ItemBagKey+0x2fc

/* +0x650: D-pad left tapped. Flags hold `cmp _isSelectBag,#0`; eq = slot mode,
 * which keeps the original path. In tab mode, hop to column 0.
 * displaced: bne ItemBagKey+0x6c4 */
        .global bag10_dpad_left
bag10_dpad_left:
        beq     ItemBagKey+0x654
        ldr     r3, =storage10
        ldrb    r2, [r3, #1]
        cmp     r2, #5
        subge   r2, r2, #5
        strb    r2, [r3, #1]
        b       ItemBagKey+0x6c4

/* +0x6e0: D-pad right tapped; tab mode hops to column 1 (if it exists).
 * displaced: bne ItemBagKey+0x58 */
        .global bag10_dpad_right
bag10_dpad_right:
        beq     ItemBagKey+0x6e4
        ldr     r3, =storage10
        ldrb    r2, [r3, #1]
        ldrsb   r1, [r3, #2]
        add     r0, r2, #5
        cmp     r0, r1
        movlt   r2, r0
        strb    r2, [r3, #1]
        b       ItemBagKey+0x58

/* ---- GameMenuFrame_simpleBagKeyOnCube (the cube's view of the bag) ------------- */

/* +0x34, tab-select mode, r1 = key. displaced: cmn r1,#1 */
        .global bag10_cube_key_column
bag10_cube_key_column:
        ldr     r0, =storage10
        ldrb    r2, [r0, #1]
        ldrsb   r3, [r0, #2]
        cmn     r1, #3
        beq     1f
        cmp     r1, #0x34
        beq     1f
        cmn     r1, #4
        beq     2f
        cmp     r1, #0x36
        beq     2f
        cmn     r1, #1
        b       GameMenuFrame_simpleBagKeyOnCube+0x38
1:      cmp     r2, #5
        subge   r2, r2, #5
        b       3f
2:      add     ip, r2, #5
        cmp     ip, r3
        movlt   r2, ip
3:      strb    r2, [r0, #1]
        mvn     r6, #0
        b       GameMenuFrame_simpleBagKeyOnCube+0x5c

/* +0x24c: D-pad left. displaced: bne +0x2a8 */
        .global bag10_cube_dpad_left
bag10_cube_dpad_left:
        beq     GameMenuFrame_simpleBagKeyOnCube+0x250
        ldr     r3, =storage10
        ldrb    r2, [r3, #1]
        cmp     r2, #5
        subge   r2, r2, #5
        strb    r2, [r3, #1]
        b       GameMenuFrame_simpleBagKeyOnCube+0x2a8

/* +0x2c0: D-pad right. displaced: bne +0x5c */
        .global bag10_cube_dpad_right
bag10_cube_dpad_right:
        beq     GameMenuFrame_simpleBagKeyOnCube+0x2c4
        ldr     r3, =storage10
        ldrb    r2, [r3, #1]
        ldrsb   r1, [r3, #2]
        add     r0, r2, #5
        cmp     r0, r1
        movlt   r2, r0
        strb    r2, [r3, #1]
        b       GameMenuFrame_simpleBagKeyOnCube+0x5c

/* +0x4d4: touch hit-test of tab r8, sl = 27*tab. displaced: add r3,r3,sl ; sub r2,r2,#0x51 */
        .global bag10_cube_touch_tab
bag10_cube_touch_tab:
        add     r3, r3, sl
        cmp     r8, #5
        subge   r3, r3, #0x87
        sub     r2, r2, #0x6c
        addge   r2, r2, #0x1b
        b       GameMenuFrame_simpleBagKeyOnCube+0x4dc

/* ---- GameMenuFrame_drawBsBag (the blacksmith's view of the bag) --------------- */

/* +0x490: tab icon, r8 = tab, r7 = 27*tab. displaced: add r2,r2,r7 ; sub r1,r1,#0x51 */
        .global bag10_bs_draw_tab
bag10_bs_draw_tab:
        add     r2, r2, r7
        cmp     r8, #5
        subge   r2, r2, #0x87
        sub     r1, r1, #0x6c
        addge   r1, r1, #0x1b
        b       GameMenuFrame_drawBsBag+0x498

/* +0x530: cursor mark, r3 = current bag, sb = 0x1b. displaced: mla r2,sb,r3,r2 ; sub r1,r1,#0x61 */
        .global bag10_bs_draw_cursor
bag10_bs_draw_cursor:
        cmp     r3, #5
        subge   r3, r3, #5
        mla     r2, sb, r3, r2
        sub     r1, r1, #0x61
        sublt   r1, r1, #0x1b
        b       GameMenuFrame_drawBsBag+0x538

/* ---- NpcUI_drawBg / NpcUI_tableKeyPressed (the shop's "sell" view) ------------
 * The same tab strip also shows a shop's pages, up to five, so the column
 * layout is applied only while merchantState == 4 (selling from the bag).
 * The current bag there is `bagSelCnt`, not storage[1]. */

/* NpcUI_drawBg+0x28c: tab icon, r8 = tab, r7 = 27*tab; `sub r1,#0x51` follows.
 * displaced: add r2,r2,r7 */
        .global bag10_shop_draw_tab
bag10_shop_draw_tab:
        add     r2, r2, r7
        push    {r0}
        ldr     r0, =merchantState
        ldrb    r0, [r0]
        cmp     r0, #4
        pop     {r0}
        bne     NpcUI_drawBg+0x290
        cmp     r8, #5
        subge   r2, r2, #0x87
        sublt   r1, r1, #0x1b
        b       NpcUI_drawBg+0x290

/* NpcUI_drawBg+0x32c: cursor mark, r3 = bagSelCnt, sb = 0x1b; `sub r1,#0x61` follows later.
 * displaced: mla r2,sb,r3,r2 */
        .global bag10_shop_draw_cursor
bag10_shop_draw_cursor:
        push    {r0}
        ldr     r0, =merchantState
        ldrb    r0, [r0]
        cmp     r0, #4
        pop     {r0}
        bne     1f
        cmp     r3, #5
        subge   r3, r3, #5
        sublt   r1, r1, #0x1b
1:      mla     r2, sb, r3, r2
        b       NpcUI_drawBg+0x330

/* NpcUI_tableKeyPressed+0x4c, tab-select mode, r1 = key. displaced: cmn r1,#1 */
        .global bag10_shop_key_column
bag10_shop_key_column:
        ldr     r0, =merchantState
        ldrb    r0, [r0]
        cmp     r0, #4
        bne     9f
        ldr     r0, =bagSelCnt
        ldrb    r2, [r0]
        ldr     r3, =storage10
        ldrsb   r3, [r3, #2]
        cmn     r1, #3
        beq     1f
        cmp     r1, #0x34
        beq     1f
        cmn     r1, #4
        beq     2f
        cmp     r1, #0x36
        beq     2f
        b       9f
1:      cmp     r2, #5
        subge   r2, r2, #5
        b       3f
2:      add     ip, r2, #5
        cmp     ip, r3
        movlt   r2, ip
3:      strb    r2, [r0]
        b       NpcUI_tableKeyPressed+0x28   @ return -1
9:      cmn     r1, #1
        b       NpcUI_tableKeyPressed+0x50

/* NpcUI_tableKeyPressed+0x248: D-pad left. displaced: bne +0x284 */
        .global bag10_shop_dpad_left
bag10_shop_dpad_left:
        beq     NpcUI_tableKeyPressed+0x24c
        ldr     r3, =merchantState
        ldrb    r3, [r3]
        cmp     r3, #4
        bne     NpcUI_tableKeyPressed+0x284
        ldr     r3, =bagSelCnt
        ldrb    r2, [r3]
        cmp     r2, #5
        subge   r2, r2, #5
        strb    r2, [r3]
        b       NpcUI_tableKeyPressed+0x284

/* NpcUI_tableKeyPressed+0x2a0: D-pad right. displaced: bne +0x2c */
        .global bag10_shop_dpad_right
bag10_shop_dpad_right:
        beq     NpcUI_tableKeyPressed+0x2a4
        ldr     r3, =merchantState
        ldrb    r3, [r3]
        cmp     r3, #4
        bne     NpcUI_tableKeyPressed+0x2c
        ldr     r3, =bagSelCnt
        ldrb    r2, [r3]
        ldr     r1, =storage10
        ldrsb   r1, [r1, #2]
        add     r0, r2, #5
        cmp     r0, r1
        movlt   r2, r0
        strb    r2, [r3]
        b       NpcUI_tableKeyPressed+0x2c

/* NpcUI_tableKeyPressed+0x540: touch hit-test of tab r7, r6 = 27*tab.
 * displaced: add r3,r3,r6 ; sub r2,r2,#0x51 */
        .global bag10_shop_touch_tab
bag10_shop_touch_tab:
        add     r3, r3, r6
        sub     r2, r2, #0x51
        ldr     r1, =merchantState
        ldrb    r1, [r1]
        cmp     r1, #4
        bne     NpcUI_tableKeyPressed+0x548
        cmp     r7, #5
        subge   r3, r3, #0x87
        sublt   r2, r2, #0x1b
        b       NpcUI_tableKeyPressed+0x548

/* ---- a new game starts with all ten ------------------------------------------ */

/* MainMenuFrame_ClassSelectionKeyPressed+0x94: displaced strb r2,[r5,#2] (r2 = 3) */
        .global bag10_new_game_a
bag10_new_game_a:
        mov     ip, #10
        strb    ip, [r5, #2]
        b       MainMenuFrame_ClassSelectionKeyPressed+0x98

/* MainMenuFrame_GamePlaySelectKeyPressed+0x160: displaced strb r0,[r1,#2] (r0 = 3) */
        .global bag10_new_game_b
bag10_new_game_b:
        mov     ip, #10
        strb    ip, [r1, #2]
        b       MainMenuFrame_GamePlaySelectKeyPressed+0x164

        .ltorg
