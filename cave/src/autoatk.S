/* Detour for the auto basic attack (feature "autoatk"); the C side is
 * autoatk.c. The world-tick hook needs no asm: GameFrame_Process+0x98 is a
 * `bl Map_objectPreProcess` retargeted straight at autoatk_world_tick, which
 * calls Map_objectPreProcess itself. */
        .syntax unified
        .arm
        .text

/* HERO_Check_Autotarget entry (`push {r4-r11,lr}` displaced into the
 * orig_HERO_Check_Autotarget trampoline). While our press is in flight the
 * target is already chosen: answer 1 without touching it. */
        .global autoatk_autotarget_entry
autoatk_autotarget_entry:
        push    {r0, r1, r2, r3, ip, lr}
        bl      autoatk_forcing
        cmp     r0, #0
        pop     {r0, r1, r2, r3, ip, lr}
        beq     orig_HERO_Check_Autotarget
        mov     r0, #1
        bx      lr
