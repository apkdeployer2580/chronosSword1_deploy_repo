/* The extra rows of 환경설정 (feature "options"), and the modal the save rows
 * raise over them (feature "savexfer").
 *
 * The shipped screen is a fixed list: 10 rows under the title menu (the
 * tenth is 데이터 관리), 9 in game, drawn by MainMenuFrame_drawOption from an array
 * of ten labels and driven by MainMenuOptionKey / MainMenuOptionListKey. The
 * Chronos Sword 2 mod grew it by five rows; this does the same with the rows
 * that make sense for Chronos Sword 1:
 *
 *     투구외형        ON/OFF      only with the skin feature
 *     방패외형        ON/OFF      only with the skin feature
 *     경험치배율      기본/X2/X4
 *     자동기본공격    ON/OFF      only with the auto-attack feature
 *     세이브 내보내기             only with the savexfer feature; a press, no value
 *     세이브 불러오기             "
 *     세이브 되돌리기             "
 *
 * and takes one away: 데이터 관리 (row 9 of the title menu's list) is gone.
 * It was the upload half of a server backup whose download half is
 * unreachable in this build, so pressing it could only ever delete the local
 * save (CLAUDE.md, 「데이터 관리는 단방향 삭제 버튼이다」). Both lists now have
 * nine shipped rows, and row 9 — the index the guest still treats specially
 * in three places — is the first row of ours on either menu. Why that is safe
 * is below, at opt_base.
 *
 * Four hooks (see cave/hooks.py, opt.S):
 *
 *  * drawOption's row count (`movne r3,#10 / moveq r3,#9`) and
 *    MainMenuOptionListKey's last index (`movne r2,#9 / moveq r2,#8`) become
 *    calls to opt_row_count / opt_max_index, so the guest's own scrolling,
 *    cursor and scroll bar cover the new rows.
 *  * drawOption's row loop, right after Draw_Arrow, asks opt_draw_row first;
 *    for a row of ours it draws label and value and skips the guest's label
 *    lookup (which would index past its ten-entry array).
 *  * MainMenuOptionKey's entry asks opt_key first. It takes every event that
 *    touches a row of ours and nothing else, so the shipped rows keep the
 *    shipped code — including the guest's `_menuListKey == 9` special cases,
 *    which are exactly the ones a row of ours now sits under.
 *
 * `_menuListKey` is the selected row, `_menuPoint` the first of the seven
 * visible ones, `_menuSPoint` the selected row's place among them. The screen
 * exists twice — under the title menu and in the in-game menu — and both come
 * through the same three functions. */
#include "cave.h"

#define OPT_NAME "CSOPT001"
#define OPT_MAGIC 'C'
#define OPT_VER 1

#define S_HELM "\xc5\xf5\xb1\xb8\xbf\xdc\xc7\xfc"                  /* 투구외형 */
#define S_SHIELD "\xb9\xe6\xc6\xd0\xbf\xdc\xc7\xfc"                /* 방패외형 */
#define S_XPMUL "\xb0\xe6\xc7\xe8\xc4\xa1\xb9\xe8\xc0\xb2"         /* 경험치배율 */
#define S_AUTOATK "\xc0\xda\xb5\xbf\xb1\xe2\xba\xbb\xb0\xf8\xb0\xdd" /* 자동기본공격 */
#define S_BASIC "\xb1\xe2\xba\xbb"                                 /* 기본 */
#define S_EXPORT "\xbc\xbc\xc0\xcc\xba\xea \xb3\xbb\xba\xb8\xb3\xbb\xb1\xe2" /* 세이브 내보내기 */
#define S_IMPORT "\xbc\xbc\xc0\xcc\xba\xea \xba\xd2\xb7\xaf\xbf\xc0\xb1\xe2" /* 세이브 불러오기 */
#define S_RESTORE "\xbc\xbc\xc0\xcc\xba\xea \xb5\xc7\xb5\xb9\xb8\xae\xb1\xe2" /* 세이브 되돌리기 */
#define S_YES "\xbf\xb9"                                           /* 예 */
#define S_NO "\xbe\xc6\xb4\xcf\xbf\xc0"                            /* 아니오 */
#define S_OK "\xc8\xae\xc0\xce"                                    /* 확인 */
#define S_UNAVAILABLE "\xbb\xe7\xbf\xeb\xc7\xd2 \xbc\xf6 \xbe\xf8\xbd\xc0\xb4\xcf\xb4\xd9." /* 사용할 수 없습니다. */

struct cs_opt opt;
static u8 opt_loaded;

void opt_load(void)
{
    struct cs_opt t;
    if (opt_loaded)
        return;
    opt_loaded = 1;
    memset(&t, 0, sizeof t);
    if (MX_sdLoadData(OPT_NAME, &t, sizeof t) == (int)sizeof t && t.magic == OPT_MAGIC && t.ver == OPT_VER) {
        opt = t;
        opt.helm_hidden = opt.helm_hidden ? 1 : 0;
        opt.shield_hidden = opt.shield_hidden ? 1 : 0;
        if (opt.xpmul > 2)
            opt.xpmul = 0;
        opt.autoatk = opt.autoatk ? 1 : 0;
    } else {
        memset(&opt, 0, sizeof opt);
    }
    opt.magic = OPT_MAGIC;
    opt.ver = OPT_VER;
}

static void opt_save(void)
{
    MX_sdSaveData(OPT_NAME, &opt, sizeof opt);
}

/* ---- the rows ------------------------------------------------------------------ */

enum { ROW_HELM, ROW_SHIELD, ROW_XPMUL, ROW_AUTOATK, ROW_EXPORT, ROW_IMPORT, ROW_RESTORE, ROW_KINDS };

/* The save rows are presses, not values: they have no value column and
 * stepping them left does nothing. */
static int row_is_action(int kind)
{
    return kind >= ROW_EXPORT;
}

/* Which rows exist, in screen order: a row is only offered when the feature
 * that reads it went in. */
static int opt_rows(u8 *rows)
{
    int n = 0;
    if (cave_cfg.skin) {
        rows[n++] = ROW_HELM;
        rows[n++] = ROW_SHIELD;
    }
    rows[n++] = ROW_XPMUL;
    if (cave_cfg.autoatk)
        rows[n++] = ROW_AUTOATK;
    if (cave_cfg.savexfer) {
        rows[n++] = ROW_EXPORT;
        rows[n++] = ROW_IMPORT;
        rows[n++] = ROW_RESTORE;
    }
    return n;
}

/* The shipped row count. The guest's own test is `_menuState == 0 ? 9 : 10`
 * (`_menuState` is the title menu's sub-state: 4 while its 환경설정 is up,
 * 0 once a game is running and the same screen is reached from the in-game
 * menu), and the tenth row under the title menu is 데이터 관리. That row is
 * retired, so both lists have nine, and ours start at 9 on either menu.
 *
 * Row 9 is the index the guest still special-cases, in MainMenuOptionKey
 * alone (there is nothing else: drawOption only draws its label, ListKey only
 * clamps to the last index we hand it):
 *
 *   +0x294  the key arm's jump table — ←/→/OK on row 9 returns 0x64, the
 *           screen code for 데이터 관리;
 *   +0xcd8  OK (-5) checks row 9 before taking the → arm;
 *   +0x390  the touch arm has a row-9 block of its own: the list rows select,
 *           the OK button and the row's own label return 0x64.
 *
 * Every one of those is behind an event opt_key takes first when the selected
 * row is ours — the four keys, a tap on any visible row, the OK button, the
 * D-pad's left and right — so with row 9 ours the guest never reaches them.
 * What it still gets while row 9 is selected is scrolling (the up/down keys,
 * the scroll arrows, the D-pad's up and down) and the back button, none of
 * which looks at the row. cave.rs proves that by sweeping every key and
 * every screen point through MainMenuOptionKey with row 9 selected. */
static int opt_base(void)
{
    return 9;
}

/* A game is loaded and the hero has sprites to reload: the in-game menu's
 * case. From the title menu there is no hero yet. */
static int opt_in_game(void)
{
    return _menuState == 0;
}

/* Replaces `iVar5 = 9 / 10` in drawOption: rows in the list. */
int opt_row_count(void)
{
    u8 rows[ROW_KINDS];
    return opt_base() + opt_rows(rows);
}

/* Replaces `bVar2 = 8 / 9` in MainMenuOptionListKey: the last row index. */
int opt_max_index(void)
{
    return opt_row_count() - 1;
}

static const char *opt_label(int kind)
{
    switch (kind) {
    case ROW_HELM: return S_HELM;
    case ROW_SHIELD: return S_SHIELD;
    case ROW_XPMUL: return S_XPMUL;
    case ROW_AUTOATK: return S_AUTOATK;
    case ROW_EXPORT: return S_EXPORT;
    case ROW_IMPORT: return S_IMPORT;
    default: return S_RESTORE;
    }
}

static const char *opt_value(int kind)
{
    switch (kind) {
    case ROW_HELM: return opt.helm_hidden ? "OFF" : "ON";
    case ROW_SHIELD: return opt.shield_hidden ? "OFF" : "ON";
    case ROW_XPMUL: return opt.xpmul == 1 ? "X2" : opt.xpmul == 2 ? "X4" : S_BASIC;
    case ROW_AUTOATK: return opt.autoatk ? "ON" : "OFF";
    default: return NULL;
    }
}

/* ---- the save rows' seam to the host ------------------------------------------ */

/* The host hooks these two entries (cs_core::savexfer): the first with the
 * row that was pressed (0 export, 1 import, 2 restore), the second with the
 * answer to the question it raised. The bodies are what happens when the host
 * is not there — a row that says so, and an answer that goes nowhere. Not
 * inlined, so each is a real call the host can sit on — and each **stores
 * its argument** before anything else: a body that ignored it let clang drop
 * the argument at the call site (the first build passed whatever was in r0,
 * and the host saw "row 4"), which a hook at the entry cannot tell from a
 * real value. The volatile store is what makes r0 mean something there. */
void opt_xfer_action(int action) __attribute__((noinline));
void opt_xfer_answer(int yes) __attribute__((noinline));
volatile int opt_xfer_last_action;
volatile int opt_xfer_last_answer;

void opt_xfer_action(int action)
{
    opt_xfer_last_action = action;
    opt_modal_show(OPT_MODAL_MESSAGE, S_UNAVAILABLE);
}

void opt_xfer_answer(int yes)
{
    opt_xfer_last_answer = yes;
}

/* ---- the modal ------------------------------------------------------------------ */

struct opt_modal opt_modal;

/* The modal belongs to the options screen and to a row of ours: leaving the
 * screen zeroes `_menuListKey` (both back paths do), so a selection below our
 * rows means the screen was left under the modal and it must not come back
 * with it. Checked wherever the modal is read. */
static int modal_live(void)
{
    if (opt_modal.kind == OPT_MODAL_NONE)
        return 0;
    if ((s8)_menuListKey < opt_base()) {
        opt_modal.kind = OPT_MODAL_NONE;
        return 0;
    }
    return 1;
}

/* Raise a modal. Refused — and says so with 0 — when the options screen is not
 * showing a row of ours, which is where a late answer from the phone's file
 * screen could otherwise land. */
int opt_modal_show(int kind, const char *text)
{
    int i;
    if ((s8)_menuListKey < opt_base())
        return 0;
    opt_modal.kind = (u8)kind;
    opt_modal.sel = 0;
    for (i = 0; i < OPT_MODAL_TEXT - 1 && text[i]; i++)
        opt_modal.text[i] = text[i];
    opt_modal.text[i] = 0;
    return 1;
}

/* The modal's answer: a message closes, a question closes and tells the host. */
static void modal_act(int yes)
{
    int kind = opt_modal.kind;
    opt_modal.kind = OPT_MODAL_NONE;
    if (kind == OPT_MODAL_QUESTION)
        opt_xfer_answer(yes);
}

/* The box is the guest's own popup box (drawPopWindow: w/2-0x58, h/2-0x32,
 * 0xb0 x 0x64); the buttons sit where its OK sits (h/2+0x1e), two of them
 * for a question. */
#define BOX_X (-0x58)
#define BOX_Y (-0x32)
#define BOX_W 0xb0
#define BOX_H 0x64
#define BTN_Y 0x1e
#define BTN_W 0x40
#define BTN_H 0x10
#define LINE_Y0 (-0x28)
#define LINE_PITCH 0xe
#define LINES_MAX 3

static int btn_x(int which, int count)
{
    int w = LCD_W;
    if (count == 1)
        return w / 2 - BTN_W / 2;
    return which == 0 ? w / 2 - BTN_W - 4 : w / 2 + 4;
}

static int modal_key(int type, int a, int b)
{
    int two = opt_modal.kind == OPT_MODAL_QUESTION;
    int w = LCD_W, h = LCD_H, by = h / 2 + BTN_Y - 2;
    if (type == 2) {
        switch (a) {
        case -3: case '4': if (two) opt_modal.sel = 0; break;
        case -4: case '6': if (two) opt_modal.sel = 1; break;
        case -5: case '5': modal_act(two ? opt_modal.sel == 0 : 1); break;
        default: break;
        }
        return 1;
    }
    if (type != 0x13)
        return 1;
    if (two) {
        if (Check_Rect(a, b, btn_x(0, 2), by, BTN_W, BTN_H)) {
            modal_act(1);
            return 1;
        }
        if (Check_Rect(a, b, btn_x(1, 2), by, BTN_W, BTN_H)) {
            modal_act(0);
            return 1;
        }
    } else if (Check_Rect(a, b, btn_x(0, 1), by, BTN_W, BTN_H)) {
        modal_act(1);
        return 1;
    }
    /* the screen's own OK button, and the D-pad's left/right for a question */
    if (Check_Rect(a, b, w - 0x37, h - 0x3f, 0x37, 0x3f))
        modal_act(two ? opt_modal.sel == 0 : 1);
    else if (two && Check_Rect(a, b, 0, h - 0x3c, 0x1a, 0x25))
        opt_modal.sel = 0;
    else if (two && Check_Rect(a, b, 0x41, h - 0x3c, 0x1a, 0x25))
        opt_modal.sel = 1;
    return 1;
}

static void draw_centred(int y, const char *s, u32 colour)
{
    HF_DrawTextOutlineH2(LCD_G, LCD_W / 2, y, BOX_W - 8, s, 0, 0, 100, colour, 0, 1, 1);
}

static void draw_button(int x, int y, const char *s, int selected)
{
    u32 colour = selected ? MC_grpGetPixelFromRGB(0xff, 0xff, 0xff) : MC_grpGetPixelFromRGB(0x74, 0x74, 0x71);
    UI_RoundRect(LCD_G, x, y - 2, BTN_W, BTN_H, 0xff);
    HF_DrawTextOutlineH2(LCD_G, x + BTN_W / 2, y, BTN_W, s, 0, 0, 100, colour, 0, 1, 1);
}

/* Called from drawOption's epilogue (opt.S), i.e. over the finished screen. */
void opt_draw_overlay(void)
{
    char line[OPT_MODAL_TEXT];
    int w, h, y, n, i, j;
    u32 white;
    if (!modal_live())
        return;
    w = LCD_W;
    h = LCD_H;
    white = MC_grpGetPixelFromRGB(0xff, 0xff, 0xff);
    UI_RoundRect(LCD_G, w / 2 + BOX_X, h / 2 + BOX_Y, BOX_W, BOX_H, 0xff);
    /* the lines: `;` breaks, at most LINES_MAX of them. `;` itself is a
     * zero-width colour escape to the font, so it must never reach it. */
    y = h / 2 + LINE_Y0;
    for (i = 0, n = 0; n < LINES_MAX && opt_modal.text[i]; n++) {
        for (j = 0; opt_modal.text[i] && opt_modal.text[i] != ';' && j < OPT_MODAL_TEXT - 1; i++, j++)
            line[j] = opt_modal.text[i];
        line[j] = 0;
        if (opt_modal.text[i] == ';')
            i++;
        draw_centred(y, line, white);
        y += LINE_PITCH;
    }
    if (opt_modal.kind == OPT_MODAL_QUESTION) {
        draw_button(btn_x(0, 2), h / 2 + BTN_Y, S_YES, opt_modal.sel == 0);
        draw_button(btn_x(1, 2), h / 2 + BTN_Y, S_NO, opt_modal.sel == 1);
    } else {
        draw_button(btn_x(0, 1), h / 2 + BTN_Y, S_OK, 1);
    }
}

/* ---- the rows' behaviour --------------------------------------------------------- */

/* Step a row's value; `dir` is -1 / +1. Saved at once, like the guest saves
 * OPTIONDATA on every change. The hero's sprites are reloaded for the
 * visibility rows so the change shows immediately — only in game, where a
 * hero exists to reload. A save row is a press: stepping it right (which is
 * also what OK and a tap on the selected row come in as) presses it, left is
 * nothing. */
static void opt_cycle(int kind, int dir)
{
    if (row_is_action(kind)) {
        if (dir > 0)
            opt_xfer_action(kind - ROW_EXPORT);
        return;
    }
    switch (kind) {
    case ROW_HELM:
        opt.helm_hidden ^= 1;
        break;
    case ROW_SHIELD:
        opt.shield_hidden ^= 1;
        break;
    case ROW_XPMUL:
        opt.xpmul = (u8)((opt.xpmul + 3 + dir) % 3);
        break;
    default:
        opt.autoatk ^= 1;
        break;
    }
    opt_save();
    if ((kind == ROW_HELM || kind == ROW_SHIELD) && opt_in_game() && cave_cfg.skin)
        HERO_Load_SprImages(0);
}

/* Called from drawOption's loop for every visible row, with the colour the
 * guest chose for it (bright when selected). Returns 1 when the row is ours
 * and has been drawn. Geometry is the guest's own: labels at W/2-0x1d, values
 * at W/2+0x14, rows 0x1a apart from H/2-0x54. */
int opt_draw_row(int row, u32 colour)
{
    u8 rows[ROW_KINDS];
    int n = opt_rows(rows);
    int i = row - opt_base();
    int y;
    const char *value;
    if (i < 0 || i >= n)
        return 0;
    opt_load();
    y = (row - _menuPoint) * 0x1a + LCD_H / 2 - 0x54;
    HF_DrawTextOutLine(LCD_G, LCD_W / 2 - 0x1d, y, opt_label(rows[i]), 100, colour, 0, 1);
    value = opt_value(rows[i]);
    if (value)
        HF_DrawTextOutLine(LCD_G, LCD_W / 2 + 0x14, y, value, 100, colour, 0, 1);
    return 1;
}

/* Select `row` from a tap, scrolling by one when the tapped row was the last
 * visible one and there is more below — the same as moving down onto it. */
static void opt_select(int row, int max)
{
    _menuListKey = (u8)row;
    if (row - _menuPoint == 6 && row < max)
        _menuPoint++;
}

/* Called from MainMenuOptionKey's entry with its arguments (2 = key press
 * with the key in `a`; 0x13 = touch at `a`,`b`). Returns 1 when the event was
 * ours and the guest must not see it. */
int opt_key(int type, int a, int b)
{
    u8 rows[ROW_KINDS];
    int n = opt_rows(rows);
    int base = opt_base();
    int max = base + n - 1;
    int sel = (s8)_menuListKey;
    int r, w, h;
    if (n == 0)
        return 0;
    opt_load();
    /* While a modal is up every event is its own, so nothing underneath moves. */
    if (modal_live())
        return modal_key(type, a, b);
    if (type == 2) {
        if (sel < base || sel > max)
            return 0;
        switch (a) {
        case -1: case '2': case -2: case '8':
            MainMenuOptionListKey(a);
            return 1;
        case -3: case '4':
            opt_cycle(rows[sel - base], -1);
            return 1;
        case -4: case '6': case -5: case '5':
            opt_cycle(rows[sel - base], 1);
            return 1;
        default:
            return 0;
        }
    }
    if (type != 0x13)
        return 0;
    w = LCD_W;
    h = LCD_H;
    /* A tap on a visible row: ours, or — while one of ours is selected — any
     * row, so the guest's own row loop never runs with a row of ours
     * selected. Tapping the selected row again steps its value, as the
     * guest's rows do. */
    for (r = _menuPoint; r <= _menuPoint + 6 && r <= max; r++) {
        int ry = (r - _menuPoint) * 0x1a + h / 2 - 0x54;
        if (r < base && sel < base)
            continue;
        if (Check_Rect(a, b, w / 2 - 0x48, ry, 0xc, 0x10) || Check_Rect(a, b, w / 2 - 0x3c, ry - 2, 0x6e, 0x10) ||
            Check_Rect(a, b, w / 2 + 0x32, ry, 0x14, 0x10)) {
            if (r < base)
                opt_select(r, max);
            else if (sel == r)
                opt_cycle(rows[r - base], 1);
            else
                opt_select(r, max);
            return 1;
        }
    }
    if (sel < base || sel > max)
        return 0;
    /* One of ours is selected: the D-pad's left/right and the OK button step
     * it. Up/down, the scroll arrows and everything else go to the guest,
     * whose handling of them does not depend on the row. */
    if (Check_Rect(a, b, 0, h - 0x3c, 0x1a, 0x25)) {
        opt_cycle(rows[sel - base], -1);
        return 1;
    }
    if (Check_Rect(a, b, 0x41, h - 0x3c, 0x1a, 0x25) || Check_Rect(a, b, w - 0x37, h - 0x3f, 0x37, 0x3f)) {
        opt_cycle(rows[sel - base], 1);
        return 1;
    }
    return 0;
}

/* HERO_add_Exp's second argument, scaled by 경험치배율. Stacks with whatever
 * the host already did to it (the cash-shop growth potions) — both are
 * multipliers the player chose. */
int opt_exp_scale(int amount)
{
    s64 v;
    int m;
    opt_load();
    m = opt.xpmul == 1 ? 2 : opt.xpmul == 2 ? 4 : 1;
    if (amount <= 0 || m == 1)
        return amount;
    v = (s64)amount * m;
    if (v > 0x7fffffff)
        v = 0x7fffffff;
    return (int)v;
}
