/* What the cave's own files share with each other (guest.h is what they share
 * with the guest). */
#ifndef CAVE_CAVE_H
#define CAVE_CAVE_H
#include "guest.h"

/* ---- opt.c: the extra options rows and their record ---------------------------
 * One SD record, "CSOPT001", global to the install like the CS2 mod's four
 * separate records were. Loaded lazily on first use, saved on every change. */
struct cs_opt {
    u8 magic;         /* 'C' */
    u8 ver;           /* 1 */
    u8 helm_hidden;   /* 투구외형 OFF: the head slot loads as if empty */
    u8 shield_hidden; /* 방패외형 OFF: the hero's layer-5 parts are not drawn (skin.S reads this byte at +3) */
    u8 xpmul;         /* 경험치배율: 0 = 기본, 1 = X2, 2 = X4 */
    u8 autoatk;       /* 자동기본공격 */
    u8 pad[2];
};
extern struct cs_opt opt;
void opt_load(void);

/* The modal the three save rows raise over the options list (feature
 * "savexfer"): a message with one 확인, or a question with 예/아니오. The host
 * raises it through opt_modal_show and hears the answer through
 * opt_xfer_answer; the cave draws it and consumes every key while it is up.
 * `text` is EUC-KR, `;` breaks a line, at most three lines. */
enum { OPT_MODAL_NONE, OPT_MODAL_MESSAGE, OPT_MODAL_QUESTION };
#define OPT_MODAL_TEXT 156
struct opt_modal {
    u8 kind; /* OPT_MODAL_* */
    u8 sel;  /* question: 0 = 예, 1 = 아니오 */
    u8 pad[2];
    char text[OPT_MODAL_TEXT];
};
extern struct opt_modal opt_modal;
int opt_modal_show(int kind, const char *text);

/* ---- skin.c ------------------------------------------------------------------- */
void skin_smith_reset(void);

#endif
