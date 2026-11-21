"""The hook table: every guest site the cave changes, grouped by feature.

Each entry names a site in the guest (symbol+offset, or a bare image-relative
address), what kind of change it is, and what it becomes:

    kind "b" / "bl"   the word at the site becomes a branch (with link) to the
                      cave symbol `target`. `expect` (optional) is the word that
                      must be there now; `expect_call` (bl only) the guest
                      function the site must currently call; `orig` (optional)
                      asks build.py for a trampoline of that name that runs the
                      displaced first instruction and continues at site+4.
    kind "imm"        rewrite the immediate offset of a load/store to `value`.
    kind "word"       replace the word with `value` (an int, or a cave symbol).
    kind "got"        point the guest's GOT slot for symbol `site` at cave symbol `value`.

build.py verifies every `expect` and refuses to produce a manifest otherwise; the
host verifies every old word again at boot and leaves a whole feature out if any
of its sites has moved. Features:

    bag10    ten bag pages (cave/src/bag10.c, bag10.S, mp.c)
    skin     외형변경 at the blacksmith (skin.c, skin.S)
    options  the extra 환경설정 rows (opt.c, opt.S)
    autoatk  자동기본공격 (autoatk.c, autoatk.S)
    savexfer 세이브 내보내기/불러오기/되돌리기 — the modal over the options list (opt.c, opt.S)
"""

# Every load/store of the selected-slot byte, `_storage[0x40]`, which moves to
# `[0x7c]` when the struct grows to ten bags. Found by Ghidra's reference
# analysis on the guest (data references to _storage+0x40) and cross-checked
# against a scan of every byte-sized `ld*/st* rX,[rY,#0x40]` in those functions:
# the two agree function by function, 112 sites in 23 functions.
BAG10_SLOT_SITES = [
    ("GameMenuFrame_drawBsBag+0x154", 0x64730),
    ("GameMenuFrame_drawBsBag+0x6f8", 0x64cd4),
    ("GameMenuFrame_drawBsBag+0x80c", 0x64de8),
    ("GameMenuFrame_drawBsBag+0x830", 0x64e0c),
    ("GameMenuFrame_drawBsBag+0x870", 0x64e4c),
    ("GameMenuFrame_simpleBagKeyOnCube+0x258", 0x6e440),
    ("GameMenuFrame_simpleBagKeyOnCube+0x28c", 0x6e474),
    ("GameMenuFrame_simpleBagKeyOnCube+0x290", 0x6e478),
    ("GameMenuFrame_simpleBagKeyOnCube+0x2cc", 0x6e4b4),
    ("GameMenuFrame_simpleBagKeyOnCube+0x2f4", 0x6e4dc),
    ("GameMenuFrame_simpleBagKeyOnCube+0x2f8", 0x6e4e0),
    ("GameMenuFrame_simpleBagKeyOnCube+0x388", 0x6e570),
    ("GameMenuFrame_simpleBagKeyOnCube+0x3b0", 0x6e598),
    ("GameMenuFrame_simpleBagKeyOnCube+0x3c4", 0x6e5ac),
    ("GameMenuFrame_simpleBagKeyOnCube+0x3c8", 0x6e5b0),
    ("GameMenuFrame_simpleBagKeyOnCube+0x3f8", 0x6e5e0),
    ("GameMenuFrame_simpleBagKeyOnCube+0x40c", 0x6e5f4),
    ("GameMenuFrame_simpleBagKeyOnCube+0x410", 0x6e5f8),
    ("GameMenuFrame_simpleBagKeyOnCube+0x5d8", 0x6e7c0),
    ("GameMenuFrame_simpleBagKeyOnCube+0x774", 0x6e95c),
    ("GameMenuFrame_simpleBagKeyOnCube+0x784", 0x6e96c),
    ("GameMenuFrame_simpleBagKeyOnCube+0x7ac", 0x6e994),
    ("GameMenuFrame_simpleBagKeyOnCube+0x7e4", 0x6e9cc),
    ("IGM_itemComposeKeyPressed+0x40", 0x7020c),
    ("IGM_itemComposeKeyPressed+0x88", 0x70254),
    ("IGM_itemComposeKeyPressed+0x130", 0x702fc),
    ("IGM_itemComposeKeyPressed+0x154", 0x70320),
    ("IGM_itemComposeKeyPressed+0x204", 0x703d0),
    ("IGM_itemComposeKeyPressed+0x238", 0x70404),
    ("GameMenuFrame_drawItemBag+0x20", 0x723d0),
    ("GameMenuFrame_drawItemBag+0x5f4", 0x729a4),
    ("GameMenuFrame_drawItemBag+0x834", 0x72be4),
    ("GameMenuFrame_drawItemBag+0x95c", 0x72d0c),
    ("GameMenuFrame_drawItemBag+0x9e4", 0x72d94),
    ("GameMenuFrame_drawItemBag+0x1328", 0x736d8),
    ("GameMenuFrame_drawItemBag+0x1644", 0x739f4),
    ("IGM_DrawCube+0x2c4", 0x753a8),
    ("IGM_getPackageItems+0x78", 0x773cc),
    ("IGM_getItemsFromOldBoxDB+0x268", 0x77710),
    ("IGM_getItemsFromLockBoxDB+0x3d4", 0x77c28),
    ("GameMenuFrame_KeyPressed+0x1674", 0x794b4),
    ("GameMenuFrame_KeyPressed+0x2180", 0x79fc0),
    ("GameMenuFrame_KeyPressed+0x239c", 0x7a1dc),
    ("GameMenuFrame_KeyPressed+0x2460", 0x7a2a0),
    ("GameMenuFrame_KeyPressed+0x2dbc", 0x7abfc),
    ("GameMenuFrame_KeyPressed+0x3600", 0x7b440),
    ("GameMenuFrame_KeyPressed+0x492c", 0x7c76c),
    ("GameMenuFrame_KeyPressed+0x58a8", 0x7d6e8),
    ("GameMenuFrame_KeyPressed+0x5954", 0x7d794),
    ("GameMenuFrame_KeyPressed+0x5ee4", 0x7dd24),
    ("GameMenuFrame_KeyPressed+0x5f60", 0x7dda0),
    ("GameMenuFrame_KeyPressed+0x6ac0", 0x7e900),
    ("GameMenuFrame_KeyPressed+0x6ba4", 0x7e9e4),
    ("GameMenuFrame_KeyPressed+0x730c", 0x7f14c),
    ("GameMenuFrame_KeyPressed+0x7474", 0x7f2b4),
    ("GameMenuFrame_KeyPressed+0x8bf8", 0x80a38),
    ("GameMenuFrame_KeyPressed+0x8dc4", 0x80c04),
    ("GameMenuFrame_KeyPressed+0x8e64", 0x80ca4),
    ("GameMenuFrame_KeyPressed+0x9324", 0x81164),
    ("init_ItemBag+0x28", 0xa0be8),
    ("drawItemBag+0x2d0", 0xa1d8c),
    ("drawItemBag+0x744", 0xa2200),
    ("drawItemBag+0xa64", 0xa2520),
    ("drawItemBag+0xa88", 0xa2544),
    ("drawItemBag+0xac8", 0xa2584),
    ("ItemBagKey+0x414", 0xa2c68),
    ("ItemBagKey+0x448", 0xa2c9c),
    ("ItemBagKey+0x65c", 0xa2eb0),
    ("ItemBagKey+0x684", 0xa2ed8),
    ("ItemBagKey+0x688", 0xa2edc),
    ("ItemBagKey+0x6ec", 0xa2f40),
    ("ItemBagKey+0x714", 0xa2f68),
    ("ItemBagKey+0x718", 0xa2f6c),
    ("ItemBagKey+0x8b8", 0xa310c),
    ("ItemBagKey+0x8f4", 0xa3148),
    ("ItemBagKey+0xaa8", 0xa32fc),
    ("ItemBagKey+0xabc", 0xa3310),
    ("ItemBagKey+0xac0", 0xa3314),
    ("ItemBagKey+0xb00", 0xa3354),
    ("ItemBagKey+0xb14", 0xa3368),
    ("ItemBagKey+0xb18", 0xa336c),
    ("ItemBagKey+0xc00", 0xa3454),
    ("ItemBagKey+0xf08", 0xa375c),
    ("MainMenuFrame_ItemBagKeyPressed+0x50", 0xaa2fc),
    ("NpcUI_DrawSmith_depth+0x188", 0xdd0c0),
    ("NpcUI_DrawSmith_depth+0x40c", 0xdd344),
    ("NpcUI_blackSmithKeyPressed+0xa8", 0xde73c),
    ("NpcUI_blackSmithKeyPressed+0x1a4", 0xde838),
    ("NpcUI_blackSmithKeyPressed+0x368", 0xde9fc),
    ("NpcUI_blackSmithKeyPressed+0x444", 0xdead8),
    ("NpcUI_blackSmithKeyPressed+0x538", 0xdebcc),
    ("NpcUI_blackSmithKeyPressed+0x1084", 0xdf718),
    ("NpcUI_blackSmithKeyPressed+0x10dc", 0xdf770),
    ("NpcUI_blackSmithKeyPressed+0x1224", 0xdf8b8),
    ("NpcUI_blackSmithKeyPressed+0x182c", 0xdfec0),
    ("NpcUI_blackSmithKeyPressed+0x1a88", 0xe011c),
    ("NpcUI_blackSmithKeyPressed+0x1ac4", 0xe0158),
    ("NpcUI_blackSmithKeyPressed+0x1b38", 0xe01cc),
    ("NpcUI_blackSmithKeyPressed+0x1c44", 0xe02d8),
    ("NpcUI_merchantSelectKeyPressed+0x478", 0xe1f4c),
    ("NpcUI_merchantSelectKeyPressed+0x480", 0xe1f54),
    ("NpcUI_merchantSelectKeyPressed+0x4b4", 0xe1f88),
    ("NpcUI_merchantSelectKeyPressed+0x4c8", 0xe1f9c),
    ("NpcUI_KeyPressed+0x1b0", 0xe3438),
    ("NpcUI_KeyPressed+0x200", 0xe3488),
    ("ITEMSTORAGE_getCur+0x34", 0xf3f00),
    ("ITEMSTORAGE_getCur+0x4c", 0xf3f18),
    ("ITEMSTORAGE_removeItem+0x80", 0xf4014),
    ("ITEMSTORAGE_removeItem+0x88", 0xf401c),
    ("ITEMSTORAGE_removeCurItem+0x40", 0xf4110),
    ("ITEMSTORAGE_init+0x20", 0xf4150),
    ("ITEMSTORAGE_AddSortList+0x84", 0xf428c),
]

HOOKS = []


def h(feature, site, kind, **kw):
    d = {"feature": feature, "site": site, "kind": kind}
    d.update(kw)
    HOOKS.append(d)


# ---- bag10 ----------------------------------------------------------------------

# The struct itself: the guest's GOT slot for `_storage` points at the cave's
# ten-bag copy, and every selected-slot byte access moves from +0x40 to +0x7c.
h("bag10", "_storage", "got", value="storage10")
for _name, _addr in BAG10_SLOT_SITES:
    h("bag10", f"0x{_addr:x}", "imm", value=0x7c, label=_name)

# Pool, sort, free-slot count, hero save: whole-function replacements.
h("bag10", "MP_Initialize", "b", target="cave_MP_Initialize")
h("bag10", "MP_search", "b", target="cave_MP_search")  # MP_getNew is `b MP_search`
h("bag10", "MP_getObj", "b", target="cave_MP_getObj")
h("bag10", "MP_return", "b", target="cave_MP_return")
h("bag10", "ITEMSTORAGE_AddSortList", "b", target="bag10_sort")
h("bag10", "ItemBag_leftSlotCntOnAllBag", "b", target="bag10_left_slot_count")
h("bag10", "GameFrame_save_hero_data", "b", target="bag10_save_hero_data")

# Load: the call site in GameFrame_load_hero_data, wrapped to clear all ten bags
# first and force the count to ten afterwards.
h("bag10", "GameFrame_load_hero_data+0x1a4", "bl", target="bag10_load_storage", expect_call="Item_loadStorage")

# A new game starts with ten (the two `strb #3` sites).
h("bag10", "MainMenuFrame_ClassSelectionKeyPressed+0x94", "b", target="bag10_new_game_a", expect=0xE5C52002)
h("bag10", "MainMenuFrame_GamePlaySelectKeyPressed+0x160", "b", target="bag10_new_game_b", expect=0xE5C10002)

# The tab strip and its keys, per screen (see bag10.S for the geometry).
h("bag10", "drawItemBag+0x8b8", "b", target="bag10_draw_tab_a", expect=0xE2411051)
h("bag10", "drawItemBag+0x948", "b", target="bag10_draw_cursor_a", expect=0xE0222398)
h("bag10", "drawItemBag+0xa1c", "b", target="bag10_draw_tab_c", expect=0xE2411051)
h("bag10", "ItemBagKey+0x94", "b", target="bag10_key_column", expect=0xE2855010)
h("bag10", "ItemBagKey+0x2f4", "b", target="bag10_touch_tab", expect=0xE0833008)
h("bag10", "ItemBagKey+0x650", "b", target="bag10_dpad_left", expect=0x1A00001B)
h("bag10", "ItemBagKey+0x6e0", "b", target="bag10_dpad_right", expect=0x1AFFFE5C)
h("bag10", "GameMenuFrame_simpleBagKeyOnCube+0x34", "b", target="bag10_cube_key_column", expect=0xE3710001)
h("bag10", "GameMenuFrame_simpleBagKeyOnCube+0x24c", "b", target="bag10_cube_dpad_left", expect=0x1A000015)
h("bag10", "GameMenuFrame_simpleBagKeyOnCube+0x2c0", "b", target="bag10_cube_dpad_right", expect=0x1AFFFF65)
h("bag10", "GameMenuFrame_simpleBagKeyOnCube+0x4d4", "b", target="bag10_cube_touch_tab", expect=0xE083300A)
h("bag10", "GameMenuFrame_drawBsBag+0x490", "b", target="bag10_bs_draw_tab", expect=0xE0822007)
h("bag10", "GameMenuFrame_drawBsBag+0x530", "b", target="bag10_bs_draw_cursor", expect=0xE0222399)
h("bag10", "NpcUI_drawBg+0x28c", "b", target="bag10_shop_draw_tab", expect=0xE0822007)
h("bag10", "NpcUI_drawBg+0x32c", "b", target="bag10_shop_draw_cursor", expect=0xE0222399)
h("bag10", "NpcUI_tableKeyPressed+0x4c", "b", target="bag10_shop_key_column", expect=0xE3710001)
h("bag10", "NpcUI_tableKeyPressed+0x248", "b", target="bag10_shop_dpad_left", expect=0x1A00000D)
h("bag10", "NpcUI_tableKeyPressed+0x2a0", "b", target="bag10_shop_dpad_right", expect=0x1AFFFF61)
h("bag10", "NpcUI_tableKeyPressed+0x540", "b", target="bag10_shop_touch_tab", expect=0xE0833006)

# ---- skin -----------------------------------------------------------------------

# Every place a sprite loader reads the equipped item: five in
# Load_Sprite_HeroImages (layers 0/1/2/4/5 -> slots 2/3/4/0/1), three in
# NPC_getHeroImgbyFolder and five in NPC_LoadImageAllCharacter (NPC bodies that
# mirror the hero's look). All are `bl MP_getEquip`; the wrapper substitutes
# the recorded look and honours 투구외형/방패외형.
for _site in (
    "Load_Sprite_HeroImages+0xd8", "Load_Sprite_HeroImages+0x31c", "Load_Sprite_HeroImages+0x330",
    "Load_Sprite_HeroImages+0x344", "Load_Sprite_HeroImages+0x384",
    "NPC_getHeroImgbyFolder+0x50", "NPC_getHeroImgbyFolder+0x88", "NPC_getHeroImgbyFolder+0xa0",
    "NPC_LoadImageAllCharacter+0x420", "NPC_LoadImageAllCharacter+0x43c", "NPC_LoadImageAllCharacter+0x460",
    "NPC_LoadImageAllCharacter+0x478", "NPC_LoadImageAllCharacter+0x494",
):
    h("skin", _site, "bl", target="skin_getEquip", expect_call="MP_getEquip")

# The blacksmith's main menu: drawn by us whole, keys asked of us first, our
# sub-state reset at the end of NpcUI_setBlackSmith (its `bx lr`), and the bag
# panel's icon alpha (`ldrsb r1,[r8,#0x96]` at +0x334, item in r8, alpha at
# [sp,#0x34]) dimmed for what our pick screens cannot take.
h("skin", "NpcUI_DrawSmith_Main", "b", target="skin_smith_draw", expect=0xE92D4FF0)
h("skin", "NpcUI_blackSmithKeyPressed", "b", target="skin_smith_key_entry", expect=0xE92D4FF0, orig="orig_NpcUI_blackSmithKeyPressed")
h("skin", "NpcUI_setBlackSmith+0x8c", "b", target="skin_smith_reset_tail", expect=0xE12FFF1E)
h("skin", "drawItemBag+0x334", "b", target="skin_dim_detour", expect=0xE1D819D6)
# 방패외형: the part loop of SPR_drawSprite (`ldrb r6,[r4,r5]` at +0x15c, the
# loop's re-entry point) drops the hero's layer-5 parts while the option is off.
h("skin", "SPR_drawSprite+0x15c", "b", target="skin_hide_layer_detour", expect=0xE7D46005)

# ---- options --------------------------------------------------------------------

# drawOption: the row count (`movne r3,#0xa`; the `moveq r3,#9` after it is
# jumped over) and the first instruction after `bl Draw_Arrow` in the row loop
# (`cmp r4,#8`). MainMenuOptionListKey: the last index (`movne r2,#9`, the
# `moveq r2,#8` after it jumped over). MainMenuOptionKey: entry.
h("options", "MainMenuFrame_drawOption+0x164", "b", target="opt_draw_count_detour", expect=0x13A0300A)
h("options", "MainMenuFrame_drawOption+0x240", "b", target="opt_draw_row_detour", expect=0xE3540008)
h("options", "MainMenuOptionListKey+0x18", "b", target="opt_listkey_max_detour", expect=0x13A02009)
h("options", "MainMenuOptionKey", "b", target="opt_key_entry", expect=0xE92D4FF0, orig="orig_MainMenuOptionKey")
# 경험치배율: HERO_add_Exp's amount, at its entry (`push {r4-r10,lr}`).
h("options", "HERO_add_Exp", "b", target="opt_exp_entry", expect=0xE92D47F0, orig="orig_HERO_add_Exp")

# ---- savexfer -------------------------------------------------------------------

# The three save rows are drawn and keyed by the options hooks above; what is
# theirs alone is the modal (a message or a 예/아니오 question) over the
# finished screen, drawn from drawOption's epilogue (`add sp,sp,#0x1a4`). The
# rows themselves are only offered when this feature went in (cave_cfg).
h("savexfer", "MainMenuFrame_drawOption+0x540", "b", target="opt_overlay_detour", expect=0xE28DDF69)

# ---- autoatk --------------------------------------------------------------------

# The world tick: GameFrame_Process's `bl Map_objectPreProcess`, which the
# replacement calls itself. HERO_Check_Autotarget's entry answers 1 while our
# press is in flight.
h("autoatk", "GameFrame_Process+0x98", "bl", target="autoatk_world_tick", expect_call="Map_objectPreProcess")
h("autoatk", "HERO_Check_Autotarget", "b", target="autoatk_autotarget_entry", expect=0xE92D4FF0, orig="orig_HERO_Check_Autotarget")
