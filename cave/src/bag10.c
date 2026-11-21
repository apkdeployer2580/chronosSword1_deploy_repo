/* Ten bag pages instead of five.
 *
 * What the guest needs changed, and where each change lives:
 *   - a storage struct with room for ten bag headers: `storage10` here, wired
 *     in by pointing the guest's `_storage` GOT slot at it (hooks.py), with
 *     every `[+0x40]` selected-slot access rewritten to `[+0x7c]`;
 *   - an item pool big enough for 200 items: mp.c;
 *   - the bag count itself: forced to ten on a new game (two immediates) and
 *     after every load (the wrapper below), so old five-bag saves just grow;
 *   - a hero save buffer big enough for ten bags of records: bag10_save_hero_data
 *     below, a re-statement of GameFrame_save_hero_data with 0x2407 in place of
 *     0x1807 (the layout is streamed, so nothing else in the format moves);
 *   - the bag sort, which in the original goes through a 100-item temporary
 *     list and re-creates every item: bag10_sort below sorts the pointers in
 *     place instead, so it holds any number of items and the worn ones stay
 *     the same objects;
 *   - the tab strip drawn as two columns of five, and the keys/touches that
 *     pick a tab: bag10.S, one small detour per site.
 */
#include "guest.h"

/* Room for BAG_MAX bag headers plus the selected-slot byte at BAGSLOT_OFF.
 * 0x80 bytes, 4-aligned; the guest treats it as a byte array. */
u8 storage10[0x80] __attribute__((aligned(4)));

/* ---- loading ------------------------------------------------------------------
 * `Item_loadStorage` reads the bag count from the save and only clears that
 * many bags, so a five-bag save would leave bags 5..9 holding whatever the
 * previous game left there. Clear all ten first, then take the count to ten.
 * Called in place of the `bl Item_loadStorage` in GameFrame_load_hero_data. */
int bag10_load_storage(int buf, int off, int slot)
{
    for (int i = 0; i < BAG_MAX; i++)
        ItemBag_init(storage10, i);
    int r = Item_loadStorage(buf, off, slot);
    storage10[STORAGE_COUNT] = BAG_MAX;
    return r;
}

/* ---- saving -------------------------------------------------------------------
 * GameFrame_save_hero_data, with the buffer grown. Ten bags of twenty 36-byte
 * records need 1 + 64 + 8 + 10*(1 + 20*36) = 7283 bytes; 0x2407 = 9223 leaves
 * the same slack the original had over its 6151. DES runs on len/8 blocks and
 * the last 7 bytes stay plain either way. The loader takes whatever length the
 * record has, so old saves read back unchanged and new ones are simply longer.
 *
 * The body is the decompile of the original, step for step. */
#define HERO_SAVE_LEN 0x2407

static u8 save_buf[HERO_SAVE_LEN];

int bag10_save_hero_data(int slot)
{
    MP_getHero(0);
    if (MC_fsAvailable() < HERO_SAVE_LEN) {
        PickUpItem_addInfo(0, 0x7b, getInCodeString(0x6b), 0, 0);
        return 0;
    }
    char name[100];
    for (int i = 0; i < 100; i++)
        name[i] = 0;
    u8 *buf = save_buf;
    for (int i = 0; i < HERO_SAVE_LEN; i++)
        buf[i] = 0;

    int off = UM_saveVersion(buf, 0);
    off = HERO_save_data(buf, off, slot);
    if (off == 0) {
        PickUpItem_addInfo(0, 0x7b, getInCodeString(0x6c), 0, 0);
        return 0;
    }

    /* learned skills: a count byte, then the two id bytes of each learned one.
     * `_skillData` is { entry **list; s16 count; }, learned flag at entry+0x44. */
    u8 **skills = *(u8 ***)_skillData;
    int nskill = *(s16 *)(_skillData + 4);
    if (nskill < 1) {
        buf[off++] = 0;
    } else {
        u8 n = 0;
        for (int i = 0; i < nskill; i++)
            if ((s8)skills[i][0x44] != 0)
                n++;
        buf[off++] = n;
        for (int i = 0; i < nskill; i++) {
            u8 *sk = skills[i];
            if ((s8)sk[0x44] != 0) {
                buf[off] = sk[0];
                buf[off + 1] = sk[1];
                off += 2;
            }
        }
    }

    /* talents: byte 3 of each, in `*_talDataID` order; `_talData` is reset to
     * that array on the way (both are pointer variables) */
    int ntal = _talentTotalCnt;
    u8 **tal = *(u8 ***)_talDataID;
    *(u8 ***)_talData = tal;
    for (int i = 0; i < ntal; i++)
        buf[off + i] = tal[i][3];
    if (ntal > 0)
        off += ntal;

    off = Item_saveStorage(buf, off);

    /* shortcut slots: 2 pages x 6 slots, 13 bytes each; the slot being dragged
     * (`_copyslot`, page at +0xb, slot at +0xc, signed) is written from its copy */
    for (int page = 0; page < 2; page++) {
        for (int s = 0; s < 6; s++) {
            u8 *src = _shortcutslot + page * 0xc0 + s * 0x20;
            if ((s8)_copyslot[0xb] == page && (s8)_copyslot[0xc] == s)
                src = _copyslot;
            u8 *rec = buf + off;
            rec[0] = page;
            rec[1] = s;
            rec[2] = src[0xd];
            rec[3] = src[9];
            rec[4] = src[7];
            UM_Int64ToByte(*(u32 *)(src + 0x18), *(u32 *)(src + 0x1c), buf, off + 5);
            off += 0xd;
        }
    }

    for (int i = 0; i < 9; i++)
        buf[off + i] = _gameMenuSlotinfo[i];

    MC_knlSprintk(name, "HERODATA%03d", (int)_selectSlotNumber);
    void *copy = MC_knlCalloc(HERO_SAVE_LEN);
    u8 *c = copy;
    for (int i = 0; i < HERO_SAVE_LEN; i++)
        c[i] = buf[i];
    SaveGameBase(name, copy, HERO_SAVE_LEN);
    return 1;
}

/* ---- sorting ------------------------------------------------------------------
 * The order the original produces: worn items first, then by the per-kind
 * rank table that ItemBag_checkAndSetItemSortSlot keeps on its stack, then by
 * item index; equal stacks of a countable kind merged up to 99. The original
 * gets there by copying every item into a 100-slot list, sorting that, and
 * re-creating the items twenty to a bag. This version keeps the same items and
 * just re-links them, so it has no 100-item ceiling and the worn ones — which
 * the game holds by pointer — need no re-equipping afterwards.
 *
 * The rank table is the original's (64 bytes at ItemBag_checkAndSetItemSortSlot,
 * indexed by kind). Its key is `rank`, sorted descending, exactly as the
 * original's `sortY = -rank` ascending; worn items get `1000 - rank`, which the
 * original then drains from the tail — so worn first, in ascending rank. */
static const s8 rank_by_kind[0x40] = {
    /* 61 bytes at .rodata 0x105dec, copied to the stack by that function's prologue */
    0x14, 0x15, 0x16, 0x17, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24,
    0x25, 0x26, 0x27, 0x0a, 0x0b, 0x0c, 0x28, 0x29, 0x07, 0x18, 0x2a, 0x08, 0x2b, 0x03, 0x02, 0x01,
    0x06, 0x05, 0x04, 0x00, 0x10, 0x11, 0x12, 0x13, 0x0e, 0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32,
    0x33, 0x34, 0x35, 0x36, 0x37, 0x0d, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x09, 0x00, 0x00, 0x00,
};

#define SORT_MAX (BAG_MAX * BAG_SLOTS + 8)

static inline LinkedList *bag_of(int i)
{
    return (LinkedList *)(storage10 + STORAGE_BAGS + i * BAG_STRIDE);
}

static int sort_key(const u8 *item)
{
    int kind = item[ITEM_KIND];
    int rank = kind < 0x40 ? rank_by_kind[kind] : 0;
    /* worn items go first; the original's 1000 - rank drained last-first */
    return item[ITEM_EQUIP] == 1 ? 1000 - rank : -rank;
}

void bag10_sort(void)
{
    static u8 *list[SORT_MAX];
    int n = 0;
    storage10[BAGSLOT_OFF] = 0;

    /* collect: bag by bag, slot by slot, so ties keep their old order */
    for (int b = 0; b < BAG_MAX && b < storage10[STORAGE_COUNT]; b++) {
        LinkedList *bag = bag_of(b);
        int cnt = bag->count;
        if (cnt > BAG_SLOTS)
            cnt = BAG_SLOTS;
        for (int s = 0; s < cnt; s++) {
            u8 *it = ITEM_getItemFrom(bag, s);
            if (it == NULL || n >= SORT_MAX)
                break;
            list[n++] = it;
        }
    }

    /* stable insertion sort, descending key; then by index within a kind */
    for (int i = 1; i < n; i++) {
        u8 *it = list[i];
        int k = sort_key(it);
        int j = i;
        while (j > 0) {
            u8 *prev = list[j - 1];
            int pk = sort_key(prev);
            int before = pk < k || (pk == k && prev[ITEM_IDX] > it[ITEM_IDX]);
            if (!before)
                break;
            list[j] = prev;
            j--;
        }
        list[j] = it;
    }

    /* unlink everything; the nodes stay valid, only the lists forget them */
    for (int b = 0; b < BAG_MAX; b++)
        LinkedList_init(bag_of(b));
    for (int i = 0; i < n; i++) {
        u8 *node = list[i] + ITEM_NODE;
        *(u32 *)(node + NODE_PREV) = 0;
        *(u32 *)(node + NODE_NEXT) = 0;
    }

    /* merge stacks of the same countable item, earlier stack first, to 99 */
    for (int i = 0; i < n; i++) {
        u8 *it = list[i];
        if (it == NULL || !Item_isCountable(it[ITEM_KIND]))
            continue;
        for (int j = 0; j < i && it[ITEM_COUNT] > 0; j++) {
            u8 *dst = list[j];
            if (dst == NULL || dst[ITEM_KIND] != it[ITEM_KIND] || dst[ITEM_IDX] != it[ITEM_IDX])
                continue;
            int room = 99 - (s8)dst[ITEM_COUNT];
            if (room <= 0)
                continue;
            int move = (s8)it[ITEM_COUNT];
            if (move > room)
                move = room;
            dst[ITEM_COUNT] = (u8)((s8)dst[ITEM_COUNT] + move);
            it[ITEM_COUNT] = (u8)((s8)it[ITEM_COUNT] - move);
        }
        if ((s8)it[ITEM_COUNT] <= 0) {
            ITEM_return(it);
            list[i] = NULL;
        }
    }

    /* re-link twenty to a bag, in sorted order */
    int b = 0, placed = 0;
    for (int i = 0; i < n; i++) {
        if (list[i] == NULL)
            continue;
        if (placed == BAG_SLOTS) {
            placed = 0;
            if (++b >= BAG_MAX)
                break;
        }
        /* addLastObject links but does not count; addObject would also re-sort
         * by the guest's own key, which is what we are replacing */
        LinkedList_addLastObject(bag_of(b), list[i] + ITEM_NODE);
        bag_of(b)->count++;
        placed++;
    }
    Reset_Shortcut_Slot(1);
}

/* ---- ItemBag_leftSlotCntOnAllBag ----------------------------------------------
 * The original counts free slots in a `char` and returns it sign-extended;
 * with 200 slots that goes negative and "bag full" fires on a bag with room.
 * Same walk, wider counter, capped at 127 for callers that keep it in a byte. */
int bag10_left_slot_count(void)
{
    int free = 0;
    int nbags = (s8)storage10[STORAGE_COUNT];
    for (int b = 0; b < nbags && b < BAG_MAX; b++) {
        LinkedList *bag = bag_of(b);
        for (int s = 0; s < BAG_SLOTS; s++)
            if (ITEM_getItemFrom(bag, s) == NULL)
                free++;
    }
    return free > 127 ? 127 : free;
}
