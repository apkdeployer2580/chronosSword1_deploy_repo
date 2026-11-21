/* 외형변경 — transmog at the blacksmith (feature "skin").
 *
 * A worn piece of equipment can take the look of another piece of the same
 * kind that the player gives up. Nothing about the worn item changes: what
 * changes is which sprite Load_Sprite_HeroImages loads for its layer. That
 * function reads two bytes of the equipped item — item[1] (sprite id) and
 * item[2..3] (palette) — through MP_getEquip at five call sites, and those
 * five calls (plus the eight in the two NPC loaders that mirror the hero's
 * look) are retargeted to skin_getEquip, which hands back a copy of the item
 * with the two bytes replaced, or NULL when the options hide the slot.
 *
 * Records are keyed by the item's creation stamp (item+0xd0, the same 64-bit
 * value the save uses to re-link worn items), 256 of them in one 4 KB SD
 * record "CSSKIN01", global to the install like the CS2 mod's
 * "cs2_appearance_v1". The stamp survives saves, bag sorts and the item
 * moving between bags; it does not survive the item being sold, so a sold
 * item's record is simply never matched again.
 *
 * The UI lives inside the blacksmith's main menu (smithState 5): the guest's
 * NpcUI_DrawSmith_Main is replaced whole, NpcUI_blackSmithKeyPressed is asked
 * first, and a sub-state of our own (`ss`) says which screen is up. The guest
 * never sees a smithState it does not know. The two bag-picking screens are
 * the guest's own bag panel (drawItemBag / ItemBagKey), the way the smith's
 * 장비제련 pick is; the confirmation and the message are drawn here.
 *
 * Layout of the menu: five rows 0x1a apart from H/2-0x34 (the shipped three
 * start at H/2-0x2f); the fourth and fifth are ours. */
#include "cave.h"

#define SKIN_NAME "CSSKIN01"
#define SKIN_MAGIC 0x314e4b53u /* "SKN1" */
#define SKIN_RECS 256

#define S_SKIN "\xbf\xdc\xc7\xfc\xba\xaf\xb0\xe6"                                   /* 외형변경 */
#define S_UNSKIN "\xbf\xdc\xc7\xfc\xba\xaf\xb0\xe6 \xc1\xa6\xb0\xc5"                 /* 외형변경 제거 */
#define S_PICK_WORN "\xc2\xf8\xbf\xeb \xc0\xe5\xba\xf1 \xbc\xb1\xc5\xc3 (\xb8\xf1\xb0\xc9\xc0\xcc/\xb9\xdd\xc1\xf6 \xc1\xa6\xbf\xdc)" /* 착용 장비 선택 (목걸이/반지 제외) */
#define S_PICK_DONOR "\xbc\xd2\xb8\xf0\xc7\xd2 \xbf\xdc\xc7\xfc \xc0\xe7\xb7\xe1 \xbc\xb1\xc5\xc3" /* 소모할 외형 재료 선택 */
#define S_DONE "\xbf\xcf\xb7\xe1\xb5\xc7\xbe\xfa\xbd\xc0\xb4\xcf\xb4\xd9"            /* 완료되었습니다 */
#define S_BAD_ITEM "\xbc\xb1\xc5\xc3\xc7\xd2 \xbc\xf6 \xbe\xf8\xb4\xc2 \xc0\xe5\xba\xf1\xc0\xd4\xb4\xcf\xb4\xd9" /* 선택할 수 없는 장비입니다 */
#define S_TABLE_FULL "\xbf\xdc\xc7\xfc \xc0\xfa\xc0\xe5 \xb0\xf8\xb0\xa3\xc0\xcc \xba\xce\xc1\xb7\xc7\xd5\xb4\xcf\xb4\xd9" /* 외형 저장 공간이 부족합니다 */
#define S_SAVE_FAIL "\xc0\xfa\xc0\xe5 \xbd\xc7\xc6\xd0: \xba\xaf\xb0\xe6\xc7\xcf\xc1\xf6 \xbe\xca\xbe\xd2\xbd\xc0\xb4\xcf\xb4\xd9" /* 저장 실패: 변경하지 않았습니다 */
#define S_CANCEL "\xc3\xeb\xbc\xd2"                                                 /* 취소 */
#define S_OK "\xc8\xae\xc0\xce"                                                     /* 확인 */
#define S_CONSUMED "\xc0\xe7\xb7\xe1 \xc0\xe5\xba\xf1\xb0\xa1 \xbc\xd2\xb8\xf0\xb5\xcb\xb4\xcf\xb4\xd9" /* 재료 장비가 소모됩니다 */
#define S_NO_SKIN "\xc0\xfb\xbf\xeb\xb5\xc8 \xbf\xdc\xc7\xfc\xc0\xcc \xbe\xf8\xbd\xc0\xb4\xcf\xb4\xd9" /* 적용된 외형이 없습니다 */

/* ---- the record table ---------------------------------------------------------- */

struct skin_rec {
    u32 lo, hi; /* the worn item's stamp */
    u8 sprite;  /* what it now looks like: item[1] and item[2..3] of the donor */
    u8 slot;    /* the save slot that owns it, +1; 0 = written before this existed */
    s16 pal;
    u32 magic;  /* SKIN_MAGIC while in use, anything else = free */
};

static struct skin_rec skin_tab[SKIN_RECS];
static u8 skin_loaded;

static void skin_load(void)
{
    if (skin_loaded)
        return;
    skin_loaded = 1;
    memset(skin_tab, 0, sizeof skin_tab);
    if (MX_sdLoadData(SKIN_NAME, skin_tab, sizeof skin_tab) != (int)sizeof skin_tab)
        memset(skin_tab, 0, sizeof skin_tab);
}

static int skin_save(void)
{
    return MX_sdSaveData(SKIN_NAME, skin_tab, sizeof skin_tab) == (int)sizeof skin_tab;
}

/* The stamp as it looks once it has been through a save.
 *
 * `UM_ByteToInt64` (0xeef24) assembles the two words and then ends with
 * `orr sb, sb, r3, asr #31` — it ORs the *low* word's sign into the high
 * word. So an item whose stamp has bit 31 set (half of them, the low word
 * being milliseconds) comes back off `Item_loadStorage` with a high word of
 * -1 instead of what `Item_saveStorage` wrote. The saved bytes are right; the
 * read-back is not, and the game does not notice because it only ever
 * compares stamps that have both been through the same loader.
 *
 * A look record outlives the session it was made in, so it does notice: keyed
 * on the raw pair, every look applied to an item before that item's first
 * reload would be lost at the next load. Keying on the value the loader would
 * produce fixes that, and normalising is idempotent — a stamp that has
 * already been through a load normalises to itself — so records written
 * before this existed are found too. */
static u32 stamp_norm(u32 lo, u32 hi)
{
    return (lo >> 31) ? 0xffffffffu : hi;
}

/* One table serves the whole install, so a record has to name its save slot
 * as well as its item. The item's stamp is `MC_knlCurrentTime()` at creation
 * and `ITEM_CreateItem` only dedupes against the value it minted last, so it
 * is not unique even inside one character — the four starting pieces come in
 * two identical-stamp pairs, measured. Two characters would have to be made
 * in the same millisecond to collide, but "would have to" is not a guarantee,
 * and a look leaking between save slots is the kind of thing a player finds
 * a week later with no way back. So the slot is part of the key.
 *
 * A record with slot 0 was written before this existed and matches any slot:
 * the alternative is to silently drop looks a player already applied. */
static u8 skin_slot(void)
{
    return (u8)(_selectSlotNumber + 1);
}

static struct skin_rec *skin_find(u32 lo, u32 hi)
{
    u8 slot = skin_slot();
    int i;
    for (i = 0; i < SKIN_RECS; i++)
        if (skin_tab[i].magic == SKIN_MAGIC && skin_tab[i].lo == lo &&
            stamp_norm(skin_tab[i].lo, skin_tab[i].hi) == hi &&
            (skin_tab[i].slot == 0 || skin_tab[i].slot == slot))
            return &skin_tab[i];
    return NULL;
}

static struct skin_rec *skin_free_rec(void)
{
    int i;
    for (i = 0; i < SKIN_RECS; i++)
        if (skin_tab[i].magic != SKIN_MAGIC)
            return &skin_tab[i];
    return NULL;
}

static u32 stamp_lo(const u8 *item) { return *(const u32 *)(item + ITEM_STAMP); }
static u32 stamp_hi(const u8 *item)
{
    return stamp_norm(stamp_lo(item), *(const u32 *)(item + ITEM_STAMP + 4));
}

/* ---- what the sprite loaders see ------------------------------------------------ */

static u8 skin_scratch[ITEM_SIZE];

/* MP_getEquip for the sprite loaders: the worn item, with its look replaced by
 * its record if it has one, or NULL for the head slot when 투구외형 is off —
 * which loads exactly what an empty head slot loads, the bare head. The
 * shield cannot be hidden this way: an empty shield slot loads the class's
 * default shield sprite (HIMG/005/000), not an empty hand, so 방패외형 is
 * done at draw time instead (skin_part_byte). Only the hero (who 0) has
 * skins; the pet's equipment is untouched. */
void *skin_getEquip(int who, int slot)
{
    u8 *item = MP_getEquip(who, slot);
    struct skin_rec *rec;
    if (who != 0 || item == NULL)
        return item;
    opt_load();
    if (slot == 2 && opt.helm_hidden)
        return NULL;
    skin_load();
    rec = skin_find(stamp_lo(item), stamp_hi(item));
    if (rec == NULL)
        return item;
    memcpy(skin_scratch, item, ITEM_SIZE);
    skin_scratch[ITEM_SPRITE] = rec->sprite;
    *(s16 *)(skin_scratch + ITEM_PAL) = rec->pal;
    return skin_scratch;
}

/* From SPR_drawSprite's part loop (skin.S): `b` is the first byte of the part
 * about to be drawn, its low five bits the layer. Layer 5 is the shield; for
 * the hero's own sprite with 방패외형 off it becomes 0x7f, the "no part" marker
 * the loop already skips. The detour only comes here for layer 5 with the
 * option off, so this is not on every sprite's path. */
int skin_part_byte(const u8 *spr, int b)
{
    const u8 *hero = MP_getHero(0);
    if (hero != NULL && spr == hero + 0x40)
        return 0x7f;
    return b;
}

/* ---- eligibility ------------------------------------------------------------------ */

/* Which of the five drawn slots the hero wears `item` in, or -1. */
static int worn_slot_of(const void *item)
{
    int s;
    for (s = 0; s < EQUIP_DRAWN; s++)
        if (MP_getEquip(0, s) == item)
            return s;
    return -1;
}

/* Kinds 7..0x12 are the armour pieces, three kinds (one per class) per body
 * part; a piece may take the look of any piece for the same part. Weapons
 * (0..4) and shields (5, 6) only of their own kind. Everything from 0x13 up
 * is not equipment. */
static int kinds_compatible(u8 worn, u8 donor)
{
    if (worn >= 0x13 || donor >= 0x13)
        return 0;
    if (worn >= 7 && donor >= 7)
        return (worn - 7) / 3 == (donor - 7) / 3;
    return worn == donor;
}

static int can_pick_worn(const u8 *item)
{
    return item != NULL && item[ITEM_KIND] < 0x13 && worn_slot_of(item) >= 0;
}

static int can_pick_remove(const u8 *item)
{
    return can_pick_worn(item) && skin_find(stamp_lo(item), stamp_hi(item)) != NULL;
}

/* ---- the screens ------------------------------------------------------------------- */

enum {
    SS_MENU,         /* the five-row menu */
    SS_PICK_WORN,    /* bag panel: choose what to change */
    SS_PICK_DONOR,   /* bag panel: choose what it will look like */
    SS_CONFIRM,      /* apply? */
    SS_PICK_REMOVE,  /* bag panel: choose what to restore */
    SS_CONFIRM_REMOVE,
    SS_MESSAGE,      /* result + OK */
};

static u8 ss;
static s8 ss_sel;          /* confirm screens: 0 = 확인, 1 = 취소 */
static const char *ss_msg; /* replaces the caption of a pick screen, or the message screen's text */

static u8 *worn_item;
static u32 worn_lo, worn_hi;
static u8 *donor_item;
static u32 donor_lo, donor_hi;

/* Menu rows: the guest's three (getInCodeString 0x2f5..0x2f7) and our two. */
#define MENU_ROWS 5
#define MENU_Y0 (-0x34)
#define MENU_PITCH 0x1a

static int can_pick_donor(const u8 *item)
{
    if (item == NULL || worn_item == NULL || item == worn_item)
        return 0;
    if (ITEM_isEquipSetSlot((void *)item))
        return 0;
    return kinds_compatible(worn_item[ITEM_KIND], item[ITEM_KIND]);
}

/* The alpha drawItemBag draws an item's icon with: on our pick screens an
 * item that cannot be chosen is dimmed, the way the bag dims what the hero
 * cannot equip. */
int skin_dim_alpha(const u8 *item, int alpha)
{
    if (smithState != 5 || item == NULL)
        return alpha;
    switch (ss) {
    case SS_PICK_WORN: return can_pick_worn(item) ? alpha : 0x37;
    case SS_PICK_DONOR: return can_pick_donor(item) ? alpha : 0x37;
    case SS_PICK_REMOVE: return can_pick_remove(item) ? alpha : 0x37;
    default: return alpha;
    }
}

/* From the end of NpcUI_setBlackSmith: a fresh visit starts on the menu. */
void skin_smith_reset(void)
{
    ss = SS_MENU;
    ss_sel = 0;
    ss_msg = NULL;
    worn_item = NULL;
    donor_item = NULL;
}

static void enter_pick(int state)
{
    ss = (u8)state;
    ss_msg = NULL;
    init_ItemBag(); /* cursor to bag 0, tab strip focused — what the guest's own picks start from */
}

static void enter_message(const char *msg)
{
    ss = SS_MESSAGE;
    ss_sel = 0;
    ss_msg = msg;
}

/* Is `item` still that item — same object, same stamp — and somewhere in a
 * bag? Bag count from the storage the guest is using (ITEMSTORAGE_* go
 * through the GOT, so this is right with and without the ten-bag feature). */
static int find_in_bags(const u8 *item, u32 lo, u32 hi, int *bag, int *slot)
{
    int b, i, nbags = cave_cfg.bag10 ? BAG_MAX : 3;
    for (b = 0; b < nbags; b++) {
        int n = ITEMSTORAGE_getTabMaxCnt(b);
        for (i = 0; i < n && i < BAG_SLOTS; i++) {
            const u8 *it = ITEM_getItemFrom(ITEMSTORAGE_getList(b), i);
            if (it == item) {
                if (stamp_lo(it) != lo || stamp_hi(it) != hi)
                    return 0;
                *bag = b;
                *slot = i;
                return 1;
            }
        }
    }
    return 0;
}

static void skin_apply(void)
{
    struct skin_rec *rec, *donor_rec, backup;
    int bag, slot;
    skin_load();
    if (worn_item == NULL || worn_slot_of(worn_item) < 0 || stamp_lo(worn_item) != worn_lo ||
        stamp_hi(worn_item) != worn_hi) {
        enter_message(S_BAD_ITEM);
        return;
    }
    if (!find_in_bags(donor_item, donor_lo, donor_hi, &bag, &slot) || !can_pick_donor(donor_item)) {
        enter_message(S_BAD_ITEM);
        return;
    }
    if (MC_fsAvailable() < (int)sizeof skin_tab + 0x400) {
        enter_message(S_SAVE_FAIL);
        return;
    }
    rec = skin_find(worn_lo, worn_hi);
    if (rec == NULL)
        rec = skin_free_rec();
    if (rec == NULL) {
        enter_message(S_TABLE_FULL);
        return;
    }
    backup = *rec;
    rec->lo = worn_lo;
    rec->hi = worn_hi;
    /* a donor that itself wears a skin passes that look on, not its own */
    donor_rec = skin_find(donor_lo, donor_hi);
    if (donor_rec != NULL) {
        rec->sprite = donor_rec->sprite;
        rec->pal = donor_rec->pal;
    } else {
        rec->sprite = donor_item[ITEM_SPRITE];
        rec->pal = *(const s16 *)(donor_item + ITEM_PAL);
    }
    rec->slot = skin_slot();
    rec->magic = SKIN_MAGIC;
    if (!skin_save()) {
        *rec = backup;
        enter_message(S_SAVE_FAIL);
        return;
    }
    /* the donor is used up; the guest's own removal returns it to the pool */
    ITEMSTORAGE_removeFocusedItem(bag, slot, 1);
    donor_item = NULL;
    HERO_Load_SprImages(0);
    GameFrame_autoSave();
    enter_message(S_DONE);
}

static void skin_remove(void)
{
    struct skin_rec *rec, backup;
    skin_load();
    if (worn_item == NULL || worn_slot_of(worn_item) < 0 || stamp_lo(worn_item) != worn_lo ||
        stamp_hi(worn_item) != worn_hi) {
        enter_message(S_BAD_ITEM);
        return;
    }
    rec = skin_find(worn_lo, worn_hi);
    if (rec == NULL) {
        enter_message(S_NO_SKIN);
        return;
    }
    backup = *rec;
    memset(rec, 0, sizeof *rec);
    if (!skin_save()) {
        *rec = backup;
        enter_message(S_SAVE_FAIL);
        return;
    }
    HERO_Load_SprImages(0);
    enter_message(S_DONE);
}

/* A row of the menu was chosen. */
static void menu_select(int row)
{
    switch (row) {
    case 0: smithState = 6; break;    /* 장비제련 */
    case 1: smithState = 0x12; break; /* 오브부여 */
    case 2: smithState = 0x11; break; /* 오브제거 */
    case 3:
        worn_item = NULL;
        enter_pick(SS_PICK_WORN);
        break;
    default:
        worn_item = NULL;
        enter_pick(SS_PICK_REMOVE);
        break;
    }
}

/* Leave the smith the way the guest's own menu does. */
static void menu_back(void)
{
    Map_initDrawFlag();
    _event[0xcfb] = 0;
}

/* One step back from a pick screen: to the menu, or from the donor pick to
 * the worn pick. */
static void pick_back(void)
{
    ss_msg = NULL;
    _isSelectBag = 0;
    if (ss == SS_PICK_DONOR)
        enter_pick(SS_PICK_WORN);
    else
        ss = SS_MENU;
}

/* The item under the bag cursor was chosen with 'e'. */
static void pick_item(void)
{
    u8 *item = ITEMSTORAGE_getCur();
    if (item == NULL)
        return;
    skin_load();
    switch (ss) {
    case SS_PICK_WORN:
        if (!can_pick_worn(item)) {
            ss_msg = S_BAD_ITEM;
            return;
        }
        worn_item = item;
        worn_lo = stamp_lo(item);
        worn_hi = stamp_hi(item);
        enter_pick(SS_PICK_DONOR);
        return;
    case SS_PICK_DONOR:
        if (!can_pick_donor(item)) {
            ss_msg = S_BAD_ITEM;
            return;
        }
        donor_item = item;
        donor_lo = stamp_lo(item);
        donor_hi = stamp_hi(item);
        ss = SS_CONFIRM;
        ss_sel = 0;
        ss_msg = NULL;
        return;
    default: /* SS_PICK_REMOVE */
        if (!can_pick_remove(item)) {
            ss_msg = can_pick_worn(item) ? S_NO_SKIN : S_BAD_ITEM;
            return;
        }
        worn_item = item;
        worn_lo = stamp_lo(item);
        worn_hi = stamp_hi(item);
        ss = SS_CONFIRM_REMOVE;
        ss_sel = 0;
        ss_msg = NULL;
        return;
    }
}

/* The confirm/message screens' buttons: 확인 at the left, 취소 at the right
 * (message screen: 확인 only, centred). */
#define BTN_Y (0x54)
#define BTN_W 0x48
static int btn_x(int which, int count)
{
    int w = LCD_W;
    if (count == 1)
        return w / 2 - BTN_W / 2;
    return which == 0 ? w / 2 - BTN_W - 4 : w / 2 + 4;
}

static void confirm_act(int which)
{
    if (ss == SS_MESSAGE) {
        ss = SS_MENU;
        ss_msg = NULL;
        return;
    }
    if (which != 0) { /* 취소: back to the pick it came from */
        ss = ss == SS_CONFIRM ? SS_PICK_DONOR : SS_PICK_REMOVE;
        ss_msg = NULL;
        return;
    }
    if (ss == SS_CONFIRM)
        skin_apply();
    else
        skin_remove();
}

/* ---- drawing ------------------------------------------------------------------------ */

/* Centred on the screen; the width is the wrap width, wide enough that no
 * caption of ours breaks into two lines. */
static void draw_centred(int y, const char *s, u32 colour)
{
    HF_DrawTextOutlineH2(LCD_G, LCD_W / 2, y, 0xd2, s, 0, 0, 100, colour, 0, 1, 1);
}

static void draw_button(int x, int y, const char *s, int selected)
{
    u32 colour = selected ? MC_grpGetPixelFromRGB(0xff, 0xff, 0xff) : MC_grpGetPixelFromRGB(0x74, 0x74, 0x71);
    UI_RoundRect(LCD_G, x, y - 2, BTN_W, 0x10, 0xff);
    HF_DrawTextOutlineH2(LCD_G, x + BTN_W / 2, y, 100, s, 0, 0, 100, colour, 0, 1, 1);
}

/* Replaces NpcUI_DrawSmith_Main, which NpcUI_Render calls for smithState 5. */
void skin_smith_draw(void)
{
    int w = LCD_W, h = LCD_H, i;
    u32 grey = MC_grpGetPixelFromRGB(0x74, 0x74, 0x71);
    u32 white = MC_grpGetPixelFromRGB(0xff, 0xff, 0xff);
    u32 tan = MC_grpGetPixelFromRGB(0xc6, 0x9c, 0x6e);
    switch (ss) {
    case SS_MENU:
        for (i = 0; i < MENU_ROWS; i++) {
            const char *s = i < 3 ? (const char *)getInCodeString(0x2f5 + i) : i == 3 ? S_SKIN : S_UNSKIN;
            int y = h / 2 + MENU_Y0 + i * MENU_PITCH;
            UI_RoundRect(LCD_G, w / 2 - 0x21, y, 0x48, 0x10, 0xff);
            draw_centred(y + 2, s, i == npcUI_SelCnt ? white : grey);
        }
        return;
    case SS_PICK_WORN:
    case SS_PICK_DONOR:
    case SS_PICK_REMOVE:
        draw_centred(h / 2 - 0x69, ss_msg ? ss_msg : ss == SS_PICK_DONOR ? S_PICK_DONOR : S_PICK_WORN,
                     ss_msg ? white : tan);
        drawItemBag(0, -1, 1, 0);
        return;
    case SS_CONFIRM:
    case SS_CONFIRM_REMOVE:
        UI_RoundRect(LCD_G, w / 2 - 0x58, h / 2 - 0x66, 0xb0, 0xc8, 0xff);
        draw_centred(h / 2 - 0x60, ss == SS_CONFIRM ? S_SKIN : S_UNSKIN, white);
        if (worn_item != NULL)
            drawItemInfo(worn_item, w / 2 - 0x53, h / 2 - 0x4e, 1, 0);
        if (ss == SS_CONFIRM && donor_item != NULL) {
            UI_RoundRect(LCD_G, w / 2 - 0x50, h / 2 + 0x24, 0xa0, 0x1e, 0xff);
            drawItem_Icon_Name(donor_item, w / 2 - 0x4d, h / 2 + 0x26, 2);
            draw_centred(h / 2 + 0x44, S_CONSUMED, tan);
        }
        draw_button(btn_x(0, 2), h / 2 + BTN_Y, S_OK, ss_sel == 0);
        draw_button(btn_x(1, 2), h / 2 + BTN_Y, S_CANCEL, ss_sel != 0);
        return;
    default: /* SS_MESSAGE */
        UI_RoundRect(LCD_G, w / 2 - 0x58, h / 2 - 0x32, 0xb0, 0x55, 0xff);
        draw_centred(h / 2 - 0x14, ss_msg ? ss_msg : S_DONE, white);
        draw_button(btn_x(0, 1), h / 2 + 0x10, S_OK, 1);
        return;
    }
}

/* ---- keys ------------------------------------------------------------------------------ */

/* The guest's state-5 handling, with five rows. Touch: a row selects it, a
 * second tap on the selected row runs it; the bottom-right button runs it;
 * the top-right close leaves; the D-pad's up/down move. Keys: UP/'2', DOWN/'8',
 * OK/'5', CLEAR. */
static void menu_key(int type, int a, int b)
{
    int up = 0, down = 0, go = 0, back = 0, i, w = LCD_W, h = LCD_H;
    if (type == 0x13) {
        for (i = 0; i < MENU_ROWS; i++) {
            if (Check_Rect(a, b, w / 2 - 0x21, h / 2 + MENU_Y0 + i * MENU_PITCH, 0x48, 0x10)) {
                if (i == npcUI_SelCnt)
                    go = 1;
                else
                    npcUI_SelCnt = (s8)i;
                break;
            }
        }
        if (Check_Rect(a, b, 0x1a, h - 0x53, 0x27, 0x17))
            up = 1;
        if (Check_Rect(a, b, 0x1a, h - 0x17, 0x27, 0x17))
            down = 1;
        if (Check_Rect(a, b, w - 0x37, h - 0x3f, 0x37, 0x3f))
            go = 1;
        if (Check_Rect(a, b, w - 0x30, 0, 0x30, 0x36))
            back = 1;
    } else if (type == 2) {
        switch (a) {
        case -1: case '2': up = 1; break;
        case -2: case '8': down = 1; break;
        case -5: case '5': go = 1; break;
        case -0x10: back = 1; break;
        default: return;
        }
    } else {
        return;
    }
    if (up && --npcUI_SelCnt < 0)
        npcUI_SelCnt = MENU_ROWS - 1;
    if (down && ++npcUI_SelCnt >= MENU_ROWS)
        npcUI_SelCnt = 0;
    if (go)
        menu_select(npcUI_SelCnt);
    if (back)
        menu_back();
}

static void pick_key(int type, int a, int b)
{
    int r;
    /* the top-right close and CLEAR step back, before the bag panel sees them */
    if ((type == 0x13 && b < 0x36 && a >= LCD_W - 0x30) || (type == 2 && a == -0x10)) {
        pick_back();
        return;
    }
    r = ItemBagKey(type, a, b, 0);
    if (r == 'd') {
        /* the guest's picks: first out of the grid onto the tab strip, then out */
        if (_isSelectBag)
            pick_back();
        else {
            _isSelectBag = 1;
            init_ItemBag();
        }
    } else if (r == 'e') {
        pick_item();
    }
}

static void confirm_key(int type, int a, int b)
{
    int nbtn = ss == SS_MESSAGE ? 1 : 2, w = LCD_W, h = LCD_H, y;
    if (type == 0x13) {
        y = ss == SS_MESSAGE ? h / 2 + 0x10 : h / 2 + BTN_Y;
        if (Check_Rect(a, b, btn_x(0, nbtn), y - 2, BTN_W, 0x10)) {
            confirm_act(0);
            return;
        }
        if (nbtn == 2 && Check_Rect(a, b, btn_x(1, nbtn), y - 2, BTN_W, 0x10)) {
            confirm_act(1);
            return;
        }
        if (Check_Rect(a, b, w - 0x37, h - 0x3f, 0x37, 0x3f))
            confirm_act(ss_sel);
        else if (Check_Rect(a, b, w - 0x30, 0, 0x30, 0x36))
            confirm_act(1);
        else if (nbtn == 2 && Check_Rect(a, b, 0, h - 0x3c, 0x1a, 0x25))
            ss_sel = 0;
        else if (nbtn == 2 && Check_Rect(a, b, 0x41, h - 0x3c, 0x1a, 0x25))
            ss_sel = 1;
        return;
    }
    if (type != 2)
        return;
    switch (a) {
    case -3: case '4': if (nbtn == 2) ss_sel = 0; break;
    case -4: case '6': if (nbtn == 2) ss_sel = 1; break;
    case -5: case '5': confirm_act(ss_sel); break;
    case -0x10: confirm_act(1); break;
    default: break;
    }
}

/* Asked first by NpcUI_blackSmithKeyPressed. Everything in smithState 5 is
 * ours; any other state is the guest's. Returns 1 when handled. */
int skin_smith_key(int type, int a, int b)
{
    if (smithState != 5)
        return 0;
    switch (ss) {
    case SS_MENU:
        menu_key(type, a, b);
        break;
    case SS_PICK_WORN:
    case SS_PICK_DONOR:
    case SS_PICK_REMOVE:
        pick_key(type, a, b);
        break;
    default:
        confirm_key(type, a, b);
        break;
    }
    return 1;
}
