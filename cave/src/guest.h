/* What the cave knows about the guest: the functions it calls, the globals it
 * reads, and the struct offsets it depends on. Every name here is resolved at
 * link time against payload/libchronosMian.so's own symbol table (see
 * cave/build.py), so a typo is a link error rather than a wrong address.
 *
 * Offsets are Chronos Sword 1's. Where the CS2 mod this is ported from used a
 * different offset it is noted, because that is exactly the kind of thing that
 * silently goes wrong in a port. */
#ifndef CAVE_GUEST_H
#define CAVE_GUEST_H

typedef unsigned char u8;
typedef signed char s8;
typedef unsigned short u16;
typedef signed short s16;
typedef unsigned int u32;
typedef signed int s32;
typedef unsigned long long u64;
typedef signed long long s64;

#ifndef NULL
#define NULL ((void *)0)
#endif

/* the cave's own byte loops (entry.c); clang emits calls to these for struct
 * copies and aggregate initialisation too */
void *memcpy(void *dst, const void *src, u32 n);
void *memset(void *dst, int c, u32 n);

/* ---- item object (0xe0 bytes, lives in the type-7 pool) ---------------------
 * CS2's item is 0x168 bytes and its tail fields sit 0x82 further in
 * (kind 0x118, count 0x119, equip 0x11a, node 0x11c). */
#define ITEM_SIZE 0xe0
#define ITEM_IDX 0x00        /* u8  index within the kind */
#define ITEM_SPRITE 0x01     /* s8  sprite id: HIMG/<layer>/<id/30>, frame id%30 (Load_Sprite_HeroImages) */
#define ITEM_PAL 0x02        /* s16 palette id, -1 for none (PAL/%03d) */
#define ITEM_GRADE 0x04      /* u8  grade, indexes the name-colour table */
#define ITEM_NAME 0x33       /* char[0x19] */
#define ITEM_DESC 0x4c       /* char[0x4b] */
#define ITEM_KIND 0x96       /* u8, 0xff when free */
#define ITEM_COUNT 0x97      /* s8 stack count, 1..99 */
#define ITEM_EQUIP 0x98      /* s8: 1 worn, -1 spare (see CLAUDE.md, equip fix) */
#define ITEM_NODE 0x9c       /* OBJ list node; bags link items through it */
#define ITEM_SORTY 0xa8      /* s32 sort key used by LinkedList_sort (node+0xc) */
#define ITEM_STAMP 0xd0      /* u64 creation time, the save's identity for the item */

/* ---- hero object (MP_getHero(0)); the pet is hero 1 -------------------------
 * CS2's hero keeps the same fields 0x14 lower from 0x274 on (target 0x274,
 * attack wait 0x278) and its HP at 0x10c. */
#define HERO_X 0x8           /* s32 map x */
#define HERO_Y 0xc           /* s32 map y */
#define HERO_STATE 0xc0      /* u8: 0 idle, 2 walking, 4 attacking */
#define HERO_MOTION 0xc1     /* u8 */
#define HERO_DIR 0xc2        /* u8 */
#define HERO_CLASS 0xc6      /* s8 */
#define HERO_HP 0x120        /* s32 */
#define HERO_TARGET 0x288    /* s8 index into _monsterList, -1 none */
#define HERO_ATK_WAIT 0x28c  /* s32, non-zero while a basic attack is still in flight */
#define HERO_EQUIP 0x65c     /* void *[8]: 0 weapon, 1 shield, 2 head, 3 upper, 4 lower, 5 hands, 6 necklace, 7 ring */
#define EQUIP_SLOTS 8
#define EQUIP_DRAWN 5        /* slots 0..4 have a sprite layer; 5..7 are invisible */

/* monster object */
#define MON_X 0x8
#define MON_Y 0xc
#define MONSTER_LIST_MAX 0x46

/* OBJ node (0x44 bytes at item+0x9c): prev, next, then the pool position at +0x18 */
#define NODE_PREV 0x0
#define NODE_NEXT 0x4
#define NODE_POS 0x18        /* MP_POSITION { obj ptr, s16 idx, u8 type, u8 free } */

/* ---- item storage ("_storage") ------------------------------------------------
 * u8 [0] unused, [1] current bag, [2] bag count, [3] pad, then one 12-byte bag
 * header per bag at +4 (LinkedList: head, tail, u8 count), then the selected
 * slot byte. CS1 ships room for 5 bags with the slot byte at +0x40; the cave's
 * replacement has room for 10 with the slot byte at +0x7c, which is also where
 * CS2 keeps it. Every `[+0x40]` in the guest is rewritten to `[+0x7c]` by the
 * hook table, so the guest and this header must agree on BAGSLOT_OFF. */
#define BAG_MAX 10
#define BAG_SLOTS 20
#define STORAGE_CUR 1
#define STORAGE_COUNT 2
#define STORAGE_BAGS 4
#define BAG_STRIDE 0xc
#define BAGSLOT_OFF (STORAGE_BAGS + BAG_MAX * BAG_STRIDE) /* 0x7c */
#define BAG_HEAD 0x0
#define BAG_TAIL 0x4
#define BAG_COUNT 0x8

typedef struct LinkedList {
    void *head;
    void *tail;
    s8 count;
    u8 pad[3];
} LinkedList;

/* ---- guest functions ----------------------------------------------------------
 * Signatures follow the decompile; the guest is soft-float AAPCS like us. */
extern void *MP_getHero(int who);
extern void *MP_getEquip(int who, int slot);
extern void MP_POSITION_init(void *pos, int type, int idx, void *obj, int free);
extern void HERO_Set(void *pos, int idx);
extern void MONSTER_set(void *obj, void *pos);
extern void Effect_set(void *obj, void *pos);
extern void TargetEffect_set(void *obj, void *pos);
extern void NPC_set(void *obj, void *pos);
extern void MAPOBJ_set(void *obj, void *pos);
extern void SprObject_set(void *obj, void *pos);
extern void ITEM_set(void *obj, void *pos);
extern void DropItem_set(void *obj, void *pos);
extern void MP_initPickUpItem(int idx);
extern void NPC_return(void *obj);
extern void MONSTER_return(void *obj);
extern void Effect_return(void *obj);
extern void TargetEffect_return(void *obj);
extern void MAPOBJ_return(void *obj);
extern void ITEM_return(void *obj);
extern void DropItem_return(void *obj);
extern void SprObject_return(void *obj);

extern void *ITEM_getItemFrom(LinkedList *bag, int slot);
extern int Item_isCountable(int kind);
extern void ItemBag_init(void *storage, int bag);
extern void LinkedList_init(LinkedList *l);
extern void LinkedList_release(LinkedList *l);
extern int LinkedList_remove(LinkedList *l, void *node);
extern void LinkedList_addLastObject(LinkedList *l, void *node);
extern void LinkedList_addObject(LinkedList *l, void *node);
extern void Reset_Shortcut_Slot(int a);
extern int Item_loadStorage(int buf, int off, int slot);
extern int Item_saveStorage(void *buf, int off);
extern int HERO_save_data(void *buf, int off, int slot);
extern int UM_saveVersion(void *buf, int off);
extern void UM_Int64ToByte(u32 lo, u32 hi, void *buf, int off);
extern void *MC_knlCalloc(u32 n);
extern void MC_knlFree(void *p);
extern int MC_fsAvailable(void);
extern void MC_knlSprintk(char *dst, const char *fmt, ...);
extern void SaveGameBase(const char *name, void *buf, u32 len);
extern void *getInCodeString(int id);
extern void PickUpItem_addInfo(int a, int b, void *s, int c, int d);

/* storage (see bag10.c for the layout) */
extern void *ITEMSTORAGE_getList(int bag);
extern void *ITEMSTORAGE_getCur(void);
extern int ITEMSTORAGE_getTabMaxCnt(int bag);
extern int ITEMSTORAGE_removeFocusedItem(int bag, int slot, int count);
extern int ITEM_isEquipSetSlot(void *item);
extern int ITEM_addCnt(void *item, int delta);
extern int checkEqipItem(int idx, int cls, void *item);
extern void init_ItemBag(void);

/* item bag UI (the shared panel every bag screen draws through) */
extern void drawItemBag(int a, int b, int c, int d);
extern int ItemBagKey(int type, int a, int b, int c);
extern void drawItemInfo(void *item, int x, int y, int a, int b);
extern void drawItem_Icon_Name(void *item, int x, int y, int mode);

/* drawing */
extern u32 MC_grpGetPixelFromRGB(int r, int g, int b);
extern void UI_RoundRect(void *g, int x, int y, int w, int h, int alpha);
extern void HF_DrawTextOutlineH2(void *g, int x, int y, int w, const char *s, int a, int b, int alpha, u32 colour, int c, int centre, int d);
extern void HF_DrawTextOutLine(void *g, int x, int y, const char *s, int alpha, u32 colour, int a, int b);
extern int Check_Rect(int x, int y, int rx, int ry, int w, int h);
extern void Map_initDrawFlag(void);

/* options screen */
extern void MainMenuOptionListKey(int key);
extern void GameFrame_saveOptionData(void);

/* SD records inside `kickass`: both return the byte count on success */
extern int MX_sdSaveData(const char *name, void *buf, u32 len);
extern int MX_sdLoadData(const char *name, void *buf, u32 len);

/* hero / combat */
extern void HERO_Load_SprImages(int who);
extern int GameFrame_stopProcess(void);
extern int Event_isEvent(void);
extern int PointerList_isEnd(void *list, int idx);
extern int Hero_canAttack(void *monster);
extern int MONSTER_isObject(void *monster);
extern int MONSTER_Check_type_canAttack(void *monster);
extern int HERO_Check_Attack_Range(int who);
extern void HERO_Key_Press(int key, int who);
extern void Map_objectPreProcess(void);
extern void GameFrame_autoSave(void);

/* ---- guest globals -------------------------------------------------------------
 * `_storage` is deliberately NOT declared: the guest reaches it through its GOT
 * slot, which the hook table points at `storage10` below, and cave code must
 * use the same object. */
extern u8 _pool[];
extern s8 _skillData[];
extern s8 _talentTotalCnt;
extern u8 _talData[];
extern u8 _talDataID[];
extern u8 _shortcutslot[];
extern u8 _copyslot[];
extern u8 _gameMenuSlotinfo[];
extern s8 _selectSlotNumber;
extern u8 _lcdG[];           /* [4] graphics context, [0x4c] width, [0x50] height */
extern s32 _menuState;       /* 0 on the main menu, else in-game */
extern u8 _menuListKey;      /* selected option row */
extern s32 _menuPoint;       /* first visible option row */
extern s32 _menuSPoint;      /* selected row's index among the 7 visible, set while drawing */
extern u8 _event[];          /* [0x328] event running, [0xcfb] NPC UI opened by an event */
extern s8 smithState;        /* 5 = blacksmith main menu */
extern s8 npcUI_SelCnt;      /* the smith menu's cursor */
extern u8 _isSelectBag;      /* bag panel: cursor on the tab strip (1) or in the grid (0) */
extern s8 CUR_HERO;
extern void *_monsterList[]; /* PointerList of up to 0x46 monsters */

#define LCD_G (*(void **)(_lcdG + 4))
#define LCD_W (*(s32 *)(_lcdG + 0x4c))
#define LCD_H (*(s32 *)(_lcdG + 0x50))

/* Our replacement storage, sized for BAG_MAX bags. Defined in bag10.c. */
extern u8 storage10[];

/* ---- the host's word ---------------------------------------------------------
 * One byte per feature in `cs_core/src/cave.rs::FEATURES` order, non-zero when
 * that feature's hooks went in. Written by the host at boot, defined in
 * entry.c. */
struct cave_cfg {
    u8 bag10;   /* ten bag pages                       (CS_BAG10) */
    u8 skin;    /* transmog at the blacksmith          (CS_SKIN) */
    u8 options; /* the extra rows in the options menu  (CS_OPTIONS) */
    u8 autoatk; /* auto basic attack                   (CS_AUTOATK) */
    u8 savexfer; /* the three save rows in 환경설정      (CS_SAVEXFER) */
    u8 reserved[11];
};
extern struct cave_cfg cave_cfg;

#endif
