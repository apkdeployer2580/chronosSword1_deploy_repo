/* The EABI division helpers clang calls for `/` and `%`, mapped onto the
 * guest's own libgcc routines (which the host serves natively; see the `Div`
 * patch in cs_core/src/patch.rs). ARMv5TE has no divide instruction, so every
 * division in C becomes one of these calls. */
        .syntax unified
        .arm
        .text

        .global __aeabi_idiv
__aeabi_idiv:
        b       __divsi3

        .global __aeabi_uidiv
__aeabi_uidiv:
        b       __udivsi3

/* {quotient, remainder} in r0, r1 */
        .global __aeabi_idivmod
__aeabi_idivmod:
        push    {r0, r1, lr}
        bl      __divsi3
        pop     {r1, r2, lr}          @ r1 = dividend, r2 = divisor
        mul     r3, r0, r2
        sub     r1, r1, r3
        bx      lr

        .global __aeabi_uidivmod
__aeabi_uidivmod:
        push    {r0, r1, lr}
        bl      __udivsi3
        pop     {r1, r2, lr}
        mul     r3, r0, r2
        sub     r1, r1, r3
        bx      lr
