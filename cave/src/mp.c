/* The object pool (`MP_*`), re-stated so the item sub-pool can be bigger.
 *
 * The guest carves `_pool` (0x3c9a0 bytes) into fixed sub-pools by object type
 * and hard-codes each one's base, stride and count in four functions:
 * MP_Initialize, MP_search (MP_getNew is a tail-branch to it), MP_getObj and
 * MP_return. Type 7 — items — has room for 0x76 = 118 objects, which is enough
 * for five bags of twenty plus what is worn, and nowhere near enough for ten.
 *
 * These four are the whole contract, so the cave replaces all four and keeps
 * every number for every other type exactly as the decompile has it; only the
 * item rows point at `item_pool` below. The tests call the guest's originals
 * and these side by side for every non-item type and expect identical answers.
 *
 * Layout facts (all relative to `_pool`), taken from MP_search/MP_getObj:
 *   node = the OBJ record MP_getObj returns; the pool position lives at
 *   node+0x18 { obj ptr, s16 idx, u8 type, u8 free }, and MP_search finds a
 *   free one by the byte at node+0x1f.
 *   For items the node is item+0x9c, so node base = obj base + 0x9c.
 */
#include "guest.h"

#define ITEM_SLOTS 256 /* CS2's mod chose the same; ten bags need 208 at most */
u8 item_pool[ITEM_SLOTS * ITEM_SIZE] __attribute__((aligned(16)));

typedef struct Pos {
    void *obj;
    s16 idx;
    u8 type;
    u8 free;
} Pos;

typedef struct Row {
    u32 obj_base;  /* first object, relative to _pool (0 = item_pool) */
    u32 node_off;  /* node = obj + node_off */
    u32 stride;
    u32 count;
} Row;

/* type -> geometry. Row 0 is unused; type 2 (heroes) has no free-list. */
static const Row rows[0x11] = {
    [1] = {0x1f7e8, 0, 0x188, 0x1e},  /* NPC */
    [2] = {0x0, 0, 0xb90, 3},          /* hero */
    [3] = {0x22b0, 0, 0x738, 0x41},    /* monster */
    [4] = {0x29fe4, 0, 0x58, 0x32},    /* effect */
    [5] = {0x2b114, 0, 0x1b4, 0x32},   /* target effect */
    [6] = {0x3063c, 0, 0x34, 0x168},   /* map object */
    [7] = {0x34f60, 0x9c, ITEM_SIZE, 0x76}, /* item: replaced below */
    [8] = {0x3b6a0, 0, 0x58, 0x32},    /* drop item */
    [0x10] = {0x22604, 0, 0xd0, 0x96}, /* sprite object */
};

static inline u8 *obj_at(int type, int idx)
{
    if (type == 7)
        return item_pool + idx * ITEM_SIZE;
    return _pool + rows[type].obj_base + idx * rows[type].stride;
}

static inline int count_of(int type)
{
    return type == 7 ? ITEM_SLOTS : (int)rows[type].count;
}

/* MP_getObj(pos): the node of the object a position names, or 0. */
void *cave_MP_getObj(const Pos *pos)
{
    int type = pos->type;
    switch (type) {
    case 1: case 2: case 3: case 4: case 5: case 6: case 7: case 8: case 0x10:
        return obj_at(type, pos->idx) + rows[type].node_off;
    default:
        return NULL;
    }
}

/* MP_search(type, out): hand out a free position of this type, or type 0x12
 * ("none") when the pool is exhausted. Same walk as the original: first free
 * node in index order, marked taken on the way out. */
void cave_MP_search(int type, Pos *out)
{
    switch (type) {
    case 2: {
        /* Heroes: one slot, never really returned; the original spins forever
         * if it is taken, which is a bug we keep out of reach by design. */
        u8 *node = _pool + rows[2].node_off;
        if (node[NODE_POS + 7]) {
            node[NODE_POS + 7] = 0;
            out->obj = *(void **)(node + NODE_POS);
            *(u32 *)&out->idx = *(u32 *)(node + NODE_POS + 4);
            return;
        }
        break;
    }
    case 1: case 3: case 4: case 5: case 6: case 7: case 8: case 0x10: {
        int n = count_of(type);
        for (int i = 0; i < n; i++) {
            u8 *node = obj_at(type, i) + rows[type].node_off;
            if (node[NODE_POS + 7]) {
                node[NODE_POS + 7] = 0;
                out->obj = *(void **)(node + NODE_POS);
                *(u32 *)&out->idx = *(u32 *)(node + NODE_POS + 4);
                return;
            }
        }
        break;
    }
    case 0xe: {
        /* Pick-up items: five 0x5c-byte records at 0x3c7e8, free byte at +0xb of
         * the node that starts 4 bytes before the record. */
        for (int i = 0; i < 5; i++) {
            u8 *node = _pool + 0x3c7e4 + i * 0x5c;
            if (node[0xb]) {
                node[0xb] = 0;
                out->obj = *(void **)(node + 4);
                *(u32 *)&out->idx = *(u32 *)(node + 8);
                return;
            }
        }
        break;
    }
    default:
        break;
    }
    MP_POSITION_init(out, 0x12, -1, NULL, 1);
}

/* MP_return(pos): hand an object back to its type's owner. */
void cave_MP_return(const Pos *pos)
{
    int idx = pos->idx;
    switch (pos->type) {
    case 1: NPC_return(obj_at(1, idx)); break;
    case 3: MONSTER_return(obj_at(3, idx)); break;
    case 4: Effect_return(obj_at(4, idx)); break;
    case 5: TargetEffect_return(obj_at(5, idx)); break;
    case 6: MAPOBJ_return(obj_at(6, idx)); break;
    case 7: ITEM_return(obj_at(7, idx)); break;
    case 8: DropItem_return(obj_at(8, idx)); break;
    case 0xe: MP_initPickUpItem(idx); break;
    case 0x10: SprObject_return(obj_at(0x10, idx)); break;
    default: break;
    }
}

/* MP_Initialize(): stamp every object with its position, in the original's
 * order (the heroes first, then monsters, effects, target effects, NPCs, map
 * objects, sprite objects, items, drop items, pick-ups). */
void cave_MP_Initialize(void)
{
    Pos pos;
    for (int i = 0; i < 3; i++) {
        MP_POSITION_init(&pos, 2, i, obj_at(2, i), 1);
        HERO_Set(&pos, i);
    }
    for (int i = 0; i < 0x41; i++) {
        MP_POSITION_init(&pos, 3, i, obj_at(3, i), 1);
        MONSTER_set(obj_at(3, i), &pos);
    }
    for (int i = 0; i < 0x32; i++) {
        MP_POSITION_init(&pos, 4, i, obj_at(4, i), 1);
        Effect_set(obj_at(4, i), &pos);
    }
    for (int i = 0; i < 0x32; i++) {
        MP_POSITION_init(&pos, 5, i, obj_at(5, i), 1);
        TargetEffect_set(obj_at(5, i), &pos);
    }
    for (int i = 0; i < 0x1e; i++) {
        MP_POSITION_init(&pos, 1, i, obj_at(1, i), 1);
        NPC_set(obj_at(1, i), &pos);
    }
    for (int i = 0; i < 0x168; i++) {
        MP_POSITION_init(&pos, 6, i, obj_at(6, i), 1);
        MAPOBJ_set(obj_at(6, i), &pos);
    }
    for (int i = 0; i < 0x96; i++) {
        MP_POSITION_init(&pos, 0x10, i, obj_at(0x10, i), 1);
        SprObject_set(obj_at(0x10, i), &pos);
    }
    for (int i = 0; i < ITEM_SLOTS; i++) {
        MP_POSITION_init(&pos, 7, i, obj_at(7, i), 1);
        ITEM_set(obj_at(7, i), &pos);
    }
    for (int i = 0; i < 0x32; i++) {
        MP_POSITION_init(&pos, 8, i, obj_at(8, i), 1);
        DropItem_set(obj_at(8, i), &pos);
    }
    for (int i = 0; i < 5; i++)
        MP_initPickUpItem(i);
}
