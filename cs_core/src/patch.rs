//! Host implementations of guest functions.
//!
//! The guest rasterises every sprite itself, so when a scene is slow it is slow
//! inside one identifiable guest function. Profiling the intro found a single
//! one holding **67.8% of all guest instructions**:
//! `internalGrpCopyFrameBuffer`, the blitter behind `MC_grpDrawImage` and
//! `MC_grpCopyFrameBuffer`.
//!
//! It is slow for a structural reason, not a subtle one: it re-evaluates twelve
//! bounds and clip comparisons *and* recomputes both row base addresses **for
//! every single pixel**. Roughly a hundred guest instructions to move one
//! 16-bit value. Hoisting all of that out of the inner loop is exactly what a
//! host implementation gets for free.
//!
//! The rule this follows — and the reason it is safe — is that the host path
//! either reproduces the original bit for bit or **declines**, leaving the
//! interpreter to run the shipped code. It declines when the context carries a
//! custom per-pixel operator (a guest function pointer we would have to call
//! back into anyway), which is the one case where the host has no advantage.

use crate::mem::GuestMem;
use crate::pad::{pos_for_key, PadPos};
use crate::Vm;
use arm32_cpu::{reg, Memory, Mode};

/// `MC_GrpFrameBuffer`, as `MC_grpCreateOffScreenFrameBuffer` builds it
/// (`MC_knlAlloc(0x14)`).
#[derive(Clone, Copy)]
struct FrameBuffer {
    w: i32,
    h: i32,
    /// Row stride in *bytes* — `width << 1` at 16bpp, `width << 2` at 32bpp.
    pitch: i32,
    bpp: i32,
    pixels: u32,
}

impl FrameBuffer {
    fn read(mem: &mut GuestMem, p: u32) -> Self {
        Self {
            w: mem.r32(p) as i32,
            h: mem.r32(p + 4) as i32,
            pitch: mem.r32(p + 8) as i32,
            bpp: mem.r32(p + 0xc) as i32,
            pixels: mem.r32(p + 0x10),
        }
    }
}

/// The fields of `MC_GrpContext` this blitter reads. Offsets confirmed against
/// `MC_grpInitContext`, which seeds `clip = (0, 0, 0x7fffffff, 0x7fffffff)`,
/// `trans = MC_grpGetPixelFromRGB(0, 0, 0xff)` and `alpha = 0xff`.
#[derive(Clone, Copy)]
struct Ctx {
    clip_l: i32,
    clip_t: i32,
    clip_r: i32,
    clip_b: i32,
    trans: u32,
    alpha: i32,
    custom_op: u32,
}

impl Ctx {
    fn read(mem: &mut GuestMem, p: u32) -> Self {
        Self {
            clip_l: mem.r32(p + 4) as i32,
            clip_t: mem.r32(p + 8) as i32,
            clip_r: mem.r32(p + 0xc) as i32,
            clip_b: mem.r32(p + 0x10) as i32,
            trans: mem.r32(p + 0x1c),
            alpha: mem.r32(p + 0x20) as i32,
            custom_op: mem.r32(p + 0x2c),
        }
    }
}

/// `_midas_funcGrpPixelOpProc`, transcribed.
///
/// The guest ignores its `bpp` argument and always does RGB565 arithmetic —
/// including on a 32bpp surface, where the result is nonsense. That is the
/// shipped behaviour and this reproduces it rather than improving on it: the
/// screen is 16bpp, so the 32bpp path is unreachable in practice, and a "fix"
/// here could only ever create a difference from the original.
#[inline(always)]
fn blend565(dst: u32, src: u32, a: u32) -> u32 {
    let ia = 255 - a;
    let g = ((src & 0x7e0).wrapping_mul(a)).wrapping_add((dst & 0x7e0).wrapping_mul(ia)) >> 8;
    let r = ((src & 0xf800).wrapping_mul(a)).wrapping_add((dst & 0xf800).wrapping_mul(ia)) >> 8;
    let b = ((src & 0x1f).wrapping_mul(a)).wrapping_add((dst & 0x1f).wrapping_mul(ia));
    (g & 0x7e0) | (r & 0xf800) | ((b << 19) >> 27)
}

/// The `Ex` blend pipeline, shared by `DM_DrawRotate565Ex` and
/// `DM_DrawScreen565Ex`.
///
/// The two functions carry **byte-identical copies** of it — same level table
/// indexing, same `level == 0x10` grayscale, same `alpha <= 0xfe` weighting with
/// the same asymmetric weights, same 565 recomposition. Sharing one host copy is
/// not tidiness: this half is the part that was already A/B'd pixel for pixel
/// through the rotate blitter, so the second port inherits that evidence instead
/// of needing its own.
#[derive(Clone)]
struct Blend565 {
    /// One 32x32 block per channel value, already selected by `level`.
    tbl: [u8; 1024],
    /// `level == 0x10`: desaturate instead of looking the table up.
    gray: bool,
    /// `alpha <= 0xfe`: mix the blend result back against the destination.
    weighted: bool,
    a5: i32,
    a5p1: i32,
    a6p1: i32,
    ia5: i32,
    ia5p1: i32,
    ia6p1: i32,
}

impl Blend565 {
    /// Per-pixel alpha weights, exactly as both functions compute them at entry
    /// (`DM_DrawRotate565Ex+0xfc`, `DM_DrawScreen565Ex+0x54`): 5-bit for red and
    /// blue, 6-bit for green, and blue uses the *un-incremented* weight. That
    /// last one is a shipped asymmetry, not a transcription slip.
    fn new(alpha: i32, level: u32) -> Self {
        let af = alpha & 0xf8;
        let a5 = af >> 3;
        let a5p1 = a5 + 1;
        let a6p1 = (af >> 2) + 1;
        Self {
            tbl: [0; 1024],
            gray: level == 0x10,
            weighted: alpha <= 0xfe,
            a5,
            a5p1,
            a6p1,
            ia5: 32 - a5,
            ia5p1: 32 - a5p1,
            ia6p1: 64 - a6p1,
        }
    }

    /// Copy in the `level`'th 1 KB block of `_DM_blendingTable565`. Fails —
    /// and the caller then declines — if the table is not mapped.
    fn load(&mut self, mem: &mut GuestMem, table: u32, level: u32) -> bool {
        if self.gray {
            return true;
        }
        match mem.slice(table.wrapping_add(level.wrapping_mul(1024)), 1024) {
            Some(b) => {
                self.tbl.copy_from_slice(b);
                true
            }
            None => false,
        }
    }

    /// One source pixel over one destination pixel, both RGB565.
    #[inline(always)]
    fn apply(&self, s: u32, d: u32) -> u16 {
        let (sr, sg5, sb) = ((s >> 11) as i32, ((s & 0x7e0) >> 6) as i32, (s & 0x1f) as i32);
        let (dr, dg5, db) = ((d >> 11) as i32, ((d & 0x7e0) >> 6) as i32, (d & 0x1f) as i32);
        let (mut pr, mut pg6, mut pb);
        if self.gray {
            // level == 0x10: desaturate to (6r + 4g + 2b) / 16.
            let gray = (6 * sr + 4 * sg5 + 2 * sb) >> 4;
            pr = gray;
            pg6 = gray * 2;
            pb = gray;
        } else {
            // out_ch = table[(level*32 + src_ch)*32 + dst_ch]; green is looked
            // up in 5 bits and doubled back to six on the way out.
            let lut = |s_ch: i32, d_ch: i32| self.tbl[(d_ch + (s_ch << 5)) as usize] as i32;
            pr = lut(sr, dr);
            pg6 = lut(sg5, dg5) * 2;
            pb = lut(sb, db);
        }
        if self.weighted {
            pr = (pr * self.a5p1 + dr * self.ia5p1) >> 5;
            pg6 = (pg6 * self.a6p1 + (dg5 * 2) * self.ia6p1) >> 6;
            pb = (pb * self.a5 + db * self.ia5) >> 5;
        }
        // Red is not masked: a 5-bit mean cannot overflow into bit 16.
        (((pr << 11) | ((pg6 << 5) & 0x7e0) | (pb & 0x1f)) & 0xffff) as u16
    }
}

/// Which guest function a patched address stands for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Patch {
    /// `internalGrpCopyFrameBuffer` — the blitter behind `MC_grpDrawImage`.
    Blit,
    /// `MC_grpFillRect`.
    FillRect,
    /// `MIDASKernelManager::getAssetSizeFromVFS` — the archive directory scan.
    VfsLookup,
    /// `DM_DrawRotate565Ex` — the paletted rotate/scale/blend blitter.
    RotBlitEx,
    /// `DM_DrawRotate565` — the same walk without the blend pipeline.
    RotBlit,
    /// `DM_DrawScreen565Ex` — the unscaled paletted blitter with the blend
    /// pipeline. See `host_draw_screen`.
    ScreenBlitEx,
    /// `DM_DrawScreen565` — the same walk, `strh` of the palette entry.
    ScreenBlit,
    /// `DM_DrawImageRotateEx` — the arbitrary-angle scaler behind the boss-kill
    /// zoom, which blends through `DM_ProcPixelBlending` once per pixel.
    /// See `host_image_rotate`.
    ImageRotateEx,
    /// `startDes` — the DES driver behind every save and every monster load.
    /// See `des.rs`.
    StartDes,
    /// `DM_FillRectAlphaRGB` — the alpha-fill behind in-game dialogue boxes
    /// and HUD panels (34% of an in-game frame).
    FillAlpha,
    /// `DM_BoxBlurLCD565` — the frosted-glass pass behind every menu, shop and
    /// dialogue background. See `host_box_blur`.
    BoxBlur,
    /// `DM_RadialBlurLcd565` — the zoom-streak effect a dying boss leaves on
    /// the screen, and the most expensive thing in the game. See
    /// `host_radial_blur`.
    RadialBlur,
    /// `Map_drawMiddleObject+0x1a4` — the `add r3, r3, #4` that steps that
    /// effect's animation counter, replaced by one derived from the clock so
    /// the zoom takes seconds instead of five frames. See `host_boss_zoom`.
    BossZoom,
    /// `GameFrame_Process` — the world tick, held to the five steps it took
    /// while the shipped zoom was on screen, so the whole scene slows with the
    /// zoom instead of running on underneath it. See `host_zoom_world`.
    ZoomWorld,
    /// `Map_drawMiddleObject+0x210` — the `mov r2, #0x14` a fresh zoom is armed
    /// with, answered lower so the effect builds from a clean screen instead of
    /// starting half smeared. Also where the effect's centre is fixed. See
    /// `host_zoom_arm`.
    ZoomArm,
    /// One of the two `strb r3, [r7, #0x899]` that mark a boss as killed: note
    /// where that boss is standing, so the zoom can be centred on it. Declines —
    /// the guest sets its own flag. See `host_zoom_kill`.
    ZoomKill {
        rn: u8,
    },
    /// `__divsi3` / `__udivsi3`. The guest has no divide instruction, so every
    /// `/` in it is a call into a 55-instruction shift loop.
    Div {
        signed: bool,
    },
    /// `_midas_funcFntDrawString` — the whole bitmap-font rasteriser. See
    /// `font.rs`.
    FntDrawString,
    /// The `_midas_funcFntDrawPixel*` leaves. Kept as separate patches so
    /// `_midas_funcFntGroup_Draw` (the other caller of PreRGB) and a declined
    /// DrawString still get the fast pixel.
    FntPixel(FntPixel),
    /// `HF_DrawTextOutLine`, hooked for one call site only: the cash-shop
    /// price line. See `host_cash_price`.
    CashPrice,
    /// `DM_DrawImgRotateEx`, hooked for the menu D-pad's call sites only.
    /// See `host_menu_arrow`.
    MenuArrow,
    /// `Check_Rect`, hooked for the menu D-pad's four hit boxes only, so they
    /// move with the sprites. See `host_menu_arrow_hit`.
    MenuArrowHit,
    /// `getInCodeString` — swap 데이터 관리's carrier-fee wording for the
    /// `getInCodeString` — the one entry hook on the string table. Two
    /// unrelated jobs share it because a patch address can only carry one kind:
    /// 데이터 관리's fee wording and the 「내 ID」 tile's label. Each half is
    /// gated on its own knob. See `host_in_code_string`.
    InCodeString,
    /// `MainMenuFrame_KeyPressed+0xa68` — the instruction that commits the main
    /// menu to a tile. See `host_my_id_key`.
    MyIdKey,
    /// `MainMenuFrame_drawMainMenu+0xb0` — the grey-tile test. See
    /// `host_my_id_tile`. Carries 공용창고's tile as well, and is where
    /// `vault_screen` is cleared.
    MyIdTile,
    /// `MainMenuFrame_KeyPressed+0xa34` — the `beq` that sends a press on 편지
    /// to its network-fee question instead of the tile commit. Skipped while
    /// the tile is retired. See `host_letter_tile_branch`.
    LetterTileBranch,
    /// `MainMenuFrame_KeyPressed+0x2760` — the slot list's OK, answered with
    /// the chest's own load instead of the letter's. See `host_vault_slot_ok`.
    VaultSlotOk,
    /// `MainMenuFrame_ItemBagKeyPressed` — the bag grid's keys. Declines unless
    /// the chest owns the letter frame. See `host_vault_bag_key`.
    VaultBagKey,
    /// `draw_CommonItembag` — the bag grid's drawing, re-issued with the chest's
    /// storage under it and the two panel words over it. See
    /// `host_vault_bag_draw`.
    VaultBagDraw,
    /// `MainMenuFrame_drawMenuSlot` — the shared three-slot list, given a
    /// heading while it is the chest's. See `host_vault_slot_draw`.
    VaultSlotDraw,
    /// `IGM_DrawEquip` — the 장비 tab, re-issued and then given five cells
    /// showing the 외형 each piece is wearing. See `host_equip_skins`.
    EquipSkins,
    /// `getEqipNum` — no equipment slot stands behind a look cell. Declines on
    /// the eight gear cells. See `host_equip_slot_num`.
    EquipSlotNum,
    /// `MP_getEquip` — a slot outside the eight is nothing worn, instead of
    /// whatever `hero + slot*4` lands on. See `host_equip_worn`.
    EquipWorn,
    /// The three `ldr Rd,[Rn,#0x65c]` in `IGM_DrawEquip`: the equipment the
    /// info panel is about, answered with the look's donor while a look cell
    /// is selected. See `host_equip_panel_item`.
    EquipPanelItem,
    /// `GameMenuFrame_KeyPressed+0x4fa4` — hit-test the look cells in front of
    /// the gear ones, and decline. See `host_equip_touch`.
    EquipTouch,
    /// `GameMenuFrame_KeyPressed+0x3e8c` — OK on a look cell, which must not
    /// open the equip picker for a slot that does not exist. See
    /// `host_equip_ok_empty`.
    EquipOkEmpty,
    /// The four arrow arms of the 장비 tab's cursor, which walk a 4x2 grid the
    /// look cells made into eight columns and five. See `host_equip_step`.
    EquipStep,
    /// `MapClip` — the four clip globals, clamped. Two loads and four stores
    /// against thirty guest instructions, once per drawn tile row. See
    /// `host_map_clip`.
    MapClip,
    /// `Map_getTileIndex` — one byte out of a layer's tile array. See
    /// `host_map_tile_index`.
    MapTileIndex,
    /// `DM_CalcAnchorGbm` — the anchor correction every blitter does first,
    /// including the two this host already serves. See `host_calc_anchor`.
    CalcAnchor,
    /// `DM_RestorePalette` — a sprite's palette put back from its resource
    /// blob. See `host_restore_palette`.
    RestorePalette,
    /// `DM_SetChagePalBlendEx` — a sprite's palette blended toward a colour,
    /// entry by entry. See `host_pal_blend`.
    PalBlend,
    /// `MC_grpPutPixel` — one clipped pixel, and the 565 lerp it reaches for
    /// when the context is not opaque. See `host_put_pixel`.
    PutPixel,
    /// `_midas_funcFntInvalidate` — the four bitmap bases a font's glyph tables
    /// start at. See `host_fnt_invalidate`.
    FntInvalidate,
    /// `onCloseIME_MainFrame` — take the typed ID before the guest files it as
    /// a pet name. Declines unless our own keyboard is up. See
    /// `host_id_input_done`.
    IdInputDone,
    /// `PopWindowKey` — turn a touch on the 「내 ID」 popup's text into "change
    /// it". Declines unless that popup is the one on screen. See
    /// `host_my_id_popup_touch`.
    MyIdPopupTouch,
    /// `getSKTPhoneNumber` — answer with the identity **verbatim**, instead of
    /// letting the guest rebuild it and prepend a `'0'`. See
    /// `host_skt_phone_number`.
    SktPhoneNumber,
    /// `InputInit` — widen the letter's recipient line from a phone keypad to
    /// an ID. Declines on every other mode and on the letter body. See
    /// `host_letter_recipient`.
    LetterRecipient,
    /// `PocErrorCode` — say why a letter bounced, instead of `<ERROR:9401>`.
    /// Declines on every code that is not ours. See `host_send_error`.
    SendError,
    /// `MainMenuLetterSendKey+0x5dc` — take a copy of the recipient the instant
    /// before the guest erases it, so re-opening the field can put it back. See
    /// `host_letter_keep_recipient`.
    LetterKeepTo,
    /// `NpcUI_merchantSelectKeyPressed` — collapse the cash shop's two-press
    /// confirm into one. See `host_cash_skip_confirm`.
    CashSkipConfirm,
    /// `GameMenuFrame_KeyPressed+0x32ec` — the one instruction where the
    /// top-level game menu consumes RIGHT. See `host_bag_right`.
    BagRight,
    /// `GameMenuFrame_KeyPressed+0x4958` — using a second 오토루팅 when the
    /// first is already applied. Consumes the copy the original leaves stuck in
    /// the bag, then declines. See `host_autoloot_dup`.
    AutoLootDup,
    /// `EVENT_checkCanSkip` — the per-opcode table that decides whether the
    /// cutscene SKIP button is drawn. See `host_event_can_skip`.
    EventCanSkip,
    /// Two points inside `Event_Key` where the same opcode set turns SKIP's
    /// input away. See `host_event_skip_key`.
    EventSkipKey,
    /// `HERO_Key_DirectKeyPress` — finish the teardown its cast-abort block
    /// leaves half done. See `host_cast_cancel`.
    CastCancel,
    /// `HERO_draw_casting_guage` — unwedge its file-static. See
    /// `host_cast_gauge`.
    CastGauge,
    /// `EVENT_Tick` — rescue a cutscene skip whose progress byte is never
    /// advanced because the renderer that owns it is not running. See
    /// `host_skip_watchdog`.
    SkipWatchdog,
    /// `ITEM_setEquip`, hooked for the one call in `Item_loadStorage` that
    /// rebuilds the equip slots from the save: refuse to hand a slot to a bag
    /// item that says it is not equipped. See `host_equip_restore`.
    EquipRestore,
    /// `GameFrame_loadOptionData` — force the in-game touch control scheme.
    /// See `host_pad_mode`.
    PadMode,
    /// `GameFrame_saveOptionData` — log what the guest is about to write, and
    /// who asked. Diagnostic only; always declines. See `host_opt_save_log`.
    OptSaveLog,
    /// `NpcUI_setMerchant` — the shop-list builder, carrying two jobs because
    /// an address can carry one patch kind: append the 비약 to the cash shop's
    /// list after the guest has parsed the file, and build 「레시피2」's list
    /// outright. See `host_cash_potion_list` and `host_recipe2_load`.
    CashPotionList,
    /// `NpcUI_setBoughtItemOnCASH` — turn a 비약 purchase into a timed buff
    /// instead of an item. See `host_cash_potion_buy`.
    CashPotionBuy,
    /// `NpcUI_DrawMerChant_Main+0x1c` — the category menu's label-loop bound,
    /// answered one higher so a fifth row is drawn. See `host_recipe2_rows`.
    Recipe2Rows,
    /// `NpcUI_merchantKeyPressed+0x4b8` — the cursor's upward wrap target.
    Recipe2WrapUp,
    /// `NpcUI_merchantKeyPressed+0x4e0` — the cursor's downward wrap test,
    /// answered with flags for one row more.
    Recipe2WrapDown,
    /// `NpcUI_merchantKeyPressed+0x9f4` — the touch hit-test loop's bound.
    Recipe2HitRows,
    /// `NpcUI_merchantKeyPressed+0xa94` — the `merchantState` store, which for
    /// the fifth row writes the recipe shop's state instead of a sixth one the
    /// key dispatcher has no table entry for. See `host_recipe2_state`.
    Recipe2State,
    /// `Pet_Process+0x2f0` — the 전투타입 byte the pet AI dispatches on,
    /// answered with one of the shipped four when it holds 스마트. See
    /// `host_pet_smart_pick`.
    PetSmartPick,
    /// `Pet_move_process+0x28` and `+0xac` — the same byte, told the same
    /// story. See `host_pet_smart_move`.
    PetSmartMove,
    /// `HERO_Check_Autotarget+0xe8` — the same byte again, read for the pet
    /// from `HERO_End_Process` as well as from its own arm. See
    /// `host_pet_smart_target`.
    PetSmartTarget,
    /// `Hero_canAttack+0x28` — the `HP <= 0` refusal that makes an uncovered
    /// treasure chest unhittable. See `host_chest_break`.
    ChestBreak,
    /// Diagnostic only (`CS_BOX_DEBUG`): report why a treasure object is or is
    /// not attackable. Installed on `Hero_canAttack` and
    /// `MONSTER_Check_type_canAttack`, logs once per changed state and always
    /// declines, so the guest runs exactly as it would without it.
    BoxDebug,
    /// Diagnostic only (`CS_BOX_DEBUG`): the two `cmp r0, #0` that decide, in
    /// `GameFrame_PressTouchKeypad`, whether the OK press was consumed by an
    /// event or by a pick-up before the attack is ever reached.
    OkDebug,
    /// `GameMenuCharPetKey+0x680` / `+0x6a4` — the two clamps that held the
    /// 전투타입 row to four values.
    PetSmartWrapUp,
    PetSmartWrapDown,
    /// `draw_pet_info+0x4c`, `+0x328` and `+0x3e4` — the description
    /// dispatch, whose default arm writes no text, and the marquee's two reads
    /// of the same byte (the text pointer and the scroll width).
    /// See `host_pet_smart_desc`.
    PetSmartDesc,
    /// The tail of the pet-name commit inside `MainFrame_KeyPressed` — save the
    /// pet file once a rename bought from the shop has been confirmed. See
    /// `host_pet_rename_save` and [`pet_rename_site`].
    PetRenameSave,
    /// `HERO_add_Exp` — multiply the award while a growth 비약 runs, and decay
    /// a kill's award past the level gap instead of the shipped zero. See
    /// `host_exp_mul`.
    ExpMul,
    /// The level-gap gate in `MONSTER_dropExp` (`cmp r2, #5; bgt exit`): a kill
    /// more than five levels below the hero pays nothing at all. Bypassed with
    /// the gap remembered, so `host_exp_mul` can decay the award instead. See
    /// `host_exp_gap_gate`.
    ExpGapGate,
    /// One `ldrb` of an item's level requirement, with the registers decoded
    /// from the instruction it stands in for. See `host_item_level` and
    /// [`ITEM_LEVEL_SITES`].
    ItemLevel {
        rd: u8,
        rn: u8,
    },
    /// One `ldrsb` of an item's network flag (`item[8]`), answered as -1 so
    /// the seven equip gates treat network equipment like everything else.
    /// See `host_net_item_flag` and [`NET_ITEM_SITES`].
    NetItemFlag {
        rd: u8,
        rn: u8,
    },
    /// `MONSTER_loadCsv`, hooked for the one call that follows
    /// `MONSTER_loadMonsterStatus`: add a network item to the freshly loaded
    /// monster's drop table. See `host_net_item_drop` and `netitem.rs`.
    NetItemDrop,
    /// `HF_DrawTextOutlineH2`, hooked for the item panel's 요구레벨 line only:
    /// rule the requirement through and put the lowered one after it. See
    /// `host_item_info_level`.
    ItemInfoLevel,
    /// `IGM_DrawMenu` and `GameFrame_drawGameUI` — draw the running buffs
    /// beside the HP/MP block, and the gamepad legend along the bottom of the
    /// world screen while a pad is connected. See `host_hud`.
    Hud,
    /// A return from `__knlGR_getScreenFrameBuffer` into the keyboard layout:
    /// answer with a narrower surface so it lays itself out smaller. See
    /// `host_ime_surface` and [`IME_SURFACE_SITES`].
    ImeSurface,
    /// `imeLocal_button_init` — lay the keyboard out for a narrower surface until
    /// it fits, then centre what it produced. See `host_ime_layout`.
    ImeLayout,
    /// `imeLocal_onHandleEventKey` — log which key the keyboard decided was
    /// pressed, and let it run. Diagnostic only; `CS_IME_DEBUG=1`. See
    /// `host_ime_key_log`.
    ImeKeyLog,
    /// The `beq` that turns a backspace on an empty name into a cancel: send it
    /// to the guest's own do-nothing exit instead. See `host_ime_backspace`.
    ImeBackspace,
    /// `MainFrame_KeyPressed` — while the naming confirm popup is up, turn a tap
    /// on the OK button drawn behind it into the popup's own OK key. See
    /// `host_name_ok`.
    NameOk,
    /// One of the four MIDAS drawing entries — the leaves under every rounded
    /// rectangle, and almost all of the pet-naming keyboard's frame. See
    /// `host_midas_line`.
    MidasLine(MidasOp),
    /// `GameFrame_loadAgreeData` — answer "the AGREE record is already there",
    /// which is what keeps the SMS 수신동의 screen off the boot path. See
    /// `host_sms_agreed`.
    SmsAgreed,
    /// `MainMenuOptionKey`'s two `mov r0, #0x64` — the return value that means
    /// 데이터 관리. Replaced by a popup saying the feature is unavailable. See
    /// `host_data_manage_blocked`.
    DataManageBlocked,
    /// The cave's `opt_xfer_action` — one of 환경설정's three save rows was
    /// pressed. See `savexfer::Vm::host_xfer_action`.
    XferAction,
    /// The cave's `opt_xfer_answer` — the save rows' 예/아니오 question was
    /// answered. See `savexfer::Vm::host_xfer_answer`.
    XferAnswer,
    /// `MC_knlGetSystemProperty` — answer `"PHONENUMBER"` with this install's
    /// identity so the private server can tell players apart. See
    /// `host_phone_number` and `identity.rs`.
    PhoneNumber,
    /// The wait loop inside `UM_Sleep` (`+0x24`, the `bl MC_knlCurrentTime` the
    /// loop re-enters). `UM_Sleep` is a **busy-wait**: it re-reads the clock
    /// until the asked-for milliseconds have passed, and the interpreter renders
    /// that as tens of thousands of instructions per millisecond at full CPU —
    /// which on a phone is heat, not sleep. The hook sleeps the host for what is
    /// still left and then declines, so the guest's own loop wakes, sees the
    /// time gone, and exits through its own code. See `host_um_sleep`.
    SleepLoop,
    /// Not an implementation: log the call and let the guest run it. Installed
    /// from `CS_WATCH=Sym1,Sym2`, so any guest function can be given an
    /// argument trace without a rebuild. The only practical way to see what a
    /// 2.7 KB game function is being asked to do.
    Watch,
}

/// Which MIDAS drawing entry a [`Patch::MidasLine`] stands for. All four are the
/// same solid-colour rectangle; they differ only in where the extents come from.
/// See `Vm::host_midas_line`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MidasOp {
    /// `(g, x, y, colour)`.
    Pixel,
    /// `(g, x, y, len, colour)` — `len + 1` pixels along x.
    LineH,
    /// `(g, x, y, len, colour)` — `len + 1` pixels along y.
    LineV,
    /// `(g, x, y, lenX, lenY, colour)`.
    FillRect,
}

/// Which of the five pixel-drawing leaves a patch stands for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FntPixel {
    /// `_midas_funcFntDrawPixel` — one pixel, color decomposed first.
    Plain,
    /// `_midas_funcFntDrawPixelW` — three pixels at x-1, x, x+1.
    W,
    /// `_midas_funcFntDrawPixelH` — two pixels at y+1, y-1.
    H,
    /// `_midas_funcFntDrawPixel2` — two pixels at x, x+1.
    Two,
    /// `_midas_funcFntDrawPixelPreRGB` — one pixel from the accumulators.
    PreRgb,
}

/// Guest symbols this module can stand in for.
pub const PATCHED: [(&str, Patch); 24] = [
    ("_midas_drawPixel", Patch::MidasLine(MidasOp::Pixel)),
    ("_midas_drawLineH", Patch::MidasLine(MidasOp::LineH)),
    ("_midas_drawLineV", Patch::MidasLine(MidasOp::LineV)),
    ("_midas_fillRect", Patch::MidasLine(MidasOp::FillRect)),
    ("_Z26internalGrpCopyFrameBufferiiiiiiiiP14_MC_GrpContexth", Patch::Blit),
    ("MC_grpFillRect", Patch::FillRect),
    ("DM_FillRectAlphaRGB", Patch::FillAlpha),
    ("DM_BoxBlurLCD565", Patch::BoxBlur),
    ("DM_RadialBlurLcd565", Patch::RadialBlur),
    ("__divsi3", Patch::Div { signed: true }),
    ("__udivsi3", Patch::Div { signed: false }),
    ("_ZN18MIDASKernelManager19getAssetSizeFromVFSEPKcPm", Patch::VfsLookup),
    ("DM_DrawRotate565Ex", Patch::RotBlitEx),
    ("DM_DrawRotate565", Patch::RotBlit),
    ("DM_DrawScreen565Ex", Patch::ScreenBlitEx),
    ("DM_DrawScreen565", Patch::ScreenBlit),
    ("DM_DrawImageRotateEx", Patch::ImageRotateEx),
    ("startDes", Patch::StartDes),
    ("_midas_funcFntDrawString", Patch::FntDrawString),
    ("_midas_funcFntDrawPixel", Patch::FntPixel(FntPixel::Plain)),
    ("_midas_funcFntDrawPixelW", Patch::FntPixel(FntPixel::W)),
    ("_midas_funcFntDrawPixelH", Patch::FntPixel(FntPixel::H)),
    ("_midas_funcFntDrawPixel2", Patch::FntPixel(FntPixel::Two)),
    ("_midas_funcFntDrawPixelPreRGB", Patch::FntPixel(FntPixel::PreRgb)),
];

/// Where [`Patch::SleepLoop`] sits inside `UM_Sleep`: the `bl
/// MC_knlCurrentTime` at `+0x24` that every iteration of the wait loop
/// re-enters (the fall-through from the start-of-sleep `strd` lands there too).
pub const UM_SLEEP_LOOP: u32 = 0x24;

/// Is `UM_Sleep` still the busy-wait this hook was written against?
///
/// Four words are checked, each one something the hook actually relies on:
/// the two `mov`s that park the asked-for duration in `r7:r6` (which is where
/// [`crate::Vm::host_um_sleep`] reads it back out at the loop), the loop's own
/// `ldr r3, [r4, r5]` / `ldrd r2, r3, [r3]` pair (the start-of-sleep timestamp
/// the hook subtracts to know what is still left), and that the `bl` being
/// hooked really calls `MC_knlCurrentTime` — the clock, and nothing else.
pub fn um_sleep_site(base: u32, current_time: u32, mem: &mut GuestMem) -> Option<u32> {
    if !host_sleep() {
        return None;
    }
    // mov r7, r0 / mov r6, r1 — the 64-bit duration, low word first.
    if mem.r32(base + 4) != 0xe1a0_7000 || mem.r32(base + 8) != 0xe1a0_6001 {
        return None;
    }
    // ldr r3, [r4, r5] / ldrd r2, r3, [r3] — where the loop keeps its start.
    if mem.r32(base + 0x28) != 0xe794_3005 || mem.r32(base + 0x2c) != 0xe1c3_20d0 {
        return None;
    }
    let at = base + UM_SLEEP_LOOP;
    let word = mem.r32(at);
    if word >> 24 != 0xeb {
        return None;
    }
    let off = ((word << 8) as i32) >> 6; // 24-bit signed offset, in words
    (at.wrapping_add(8).wrapping_add(off as u32) == current_time).then_some(at)
}

/// Longest single host sleep, in ms. Anything the guest asks for beyond this
/// falls back to its own loop — still correct, just warm — so a garbage
/// duration can never park the frame thread past Android's 5 s ANR threshold.
/// The two calls this binary actually makes ask for 50 ms (the hit-stop in
/// `TargetEffect_draw`) and 1000 ms (a debug screen); both fit.
pub const SLEEP_HOST_CAP_MS: i64 = 4_000;

/// Installed only when [`crate::net::free_cash`] is on, because unlike the rest
/// of this module these *change* what the guest draws and does.
pub const PATCHED_FREE_CASH: [(&str, Patch); 2] = [
    ("HF_DrawTextOutLine", Patch::CashPrice),
    ("NpcUI_merchantSelectKeyPressed", Patch::CashSkipConfirm),
];

/// The string table. Installed when **either** of its two consumers is on, and
/// each half checks its own knob at run time — one address, one patch kind.
pub const PATCHED_IN_CODE_STRING: (&str, Patch) = ("getInCodeString", Patch::InCodeString);

/// The IME's close callback. Installed with the tile, and declines on every
/// call that is not our own keyboard closing — the pet-naming and letter paths
/// go through here too.
pub const PATCHED_ID_INPUT: (&str, Patch) = ("onCloseIME_MainFrame", Patch::IdInputDone);

/// The popup's key/touch handler, so a touch on the address can reopen the
/// keyboard. Declines on every popup that is not ours.
pub const PATCHED_ID_POPUP: (&str, Patch) = ("PopWindowKey", Patch::MyIdPopupTouch);

/// Installed with `CS_ID_AS_PHONE`: stop the guest reshaping the identity on
/// the way to the handshake. See [`skt_phone_site`].
pub const PATCHED_SKT_PHONE: (&str, Patch) = ("getSKTPhoneNumber", Patch::SktPhoneNumber);

/// The letter's recipient input. Installed with `CS_ID_RECIPIENT`, and refuses
/// if the one call site it was written against has moved.
pub const PATCHED_LETTER_INPUT: (&str, Patch) = ("InputInit", Patch::LetterRecipient);

/// The first of the eight stores that erase the recipient. Installed with the
/// recipient input, and declines into the guest's own store either way.
pub const PATCHED_LETTER_KEEP: (&str, Patch) = ("MainMenuLetterSendKey", Patch::LetterKeepTo);

/// Where to install [`Patch::LetterKeepTo`].
pub fn letter_keep_site(send_key: u32, mem: &mut GuestMem) -> Option<u32> {
    let at = send_key.wrapping_add(LETTER_CLEAR_FIRST);
    (mem.r32(at) == LETTER_CLEAR_FIRST_WORD).then_some(at)
}

/// The network error popup, so a refused letter says why. Rides the same knob
/// as the recipient input — the refusal only happens when the server is
/// checking IDs, which is when IDs are what the field takes.
pub const PATCHED_SEND_ERROR: (&str, Patch) = ("PocErrorCode", Patch::SendError);

/// Is the letter's `InputInit(0)` still where and what it was?
pub fn letter_input_site(send_key: u32, mem: &mut GuestMem) -> bool {
    let at = send_key.wrapping_add(LETTER_INPUT_CALL);
    mem.r32(at.wrapping_sub(4)) == LETTER_INPUT_CALL_PREV && mem.r32(at) >> 24 == 0xeb
}

/// Where to install [`Patch::SktPhoneNumber`], or `None` if IDs are not being
/// reported or the guest is not the binary this was written against.
pub fn skt_phone_site(base: u32, mem: &mut GuestMem) -> Option<u32> {
    if !id_as_phone() {
        return None;
    }
    (mem.r32(base) == SKT_PHONE_WORD && mem.r32(base + 4) == SKT_PHONE_SUB_WORD).then_some(base)
}

/// The three 비약 (`potions.rs`). Entry hooks, all three declining unless
/// [`crate::potions::enabled`].
///
/// `NpcUI_setBoughtItemOnCASH` is hooked at its entry and **not** pinned to a
/// caller: it has two, `MTX_Result_CB+0xac` (the JNI payment this build
/// actually uses) and `netLibCB+0x190` (the socket charge reply). Either one
/// arriving means the item was paid for.
pub const PATCHED_POTIONS: [(&str, Patch); 4] = [
    ("NpcUI_setMerchant", Patch::CashPotionList),
    ("NpcUI_setBoughtItemOnCASH", Patch::CashPotionBuy),
    ("HERO_add_Exp", Patch::ExpMul),
    ("HF_DrawTextOutlineH2", Patch::ItemInfoLevel),
];

/// Where the parse in `NpcUI_setMerchant` puts each field. Read off the loop at
/// `+0x120`, and cross-checked against the array bounds: the description stride
/// is 0xff and the last one ends at `0x1260 + 99*0xff + 0xff = 0x75fc`, exactly
/// where the grade colours begin, whose hundredth byte ends at 0x765c — just
/// inside `_merchant`'s 0x7660. The strides prove each other.
const MERCH_COUNT: u32 = 0x00;
const MERCH_ID: u32 = 0x02;
const MERCH_SUB: u32 = 0x66;
const MERCH_NAME: u32 = 0xca;
const MERCH_NAME_STRIDE: u32 = 0x28;
const MERCH_PRICE: u32 = 0x106c;
const MERCH_QTY: u32 = 0x11fc;
const MERCH_DESC: u32 = 0x1260;
const MERCH_DESC_STRIDE: u32 = 0xff;
const MERCH_GRADE: u32 = 0x75fc;
/// Rows the array holds, which is what the strides above work out to.
const MERCH_SLOTS: i32 = 100;

/// Shipped cash-shop rows to take off the shelf, by `(id, sub)`.
///
/// 확장가방 (`id 0x3e`, the only row in the file that uses that id — which is
/// also the id `NpcUI_merchantSelectKeyPressed`'s first pass special-cases)
/// buys a sixth bag page. The cave's `bag10` hands out ten and re-forces ten
/// after **every** load (`bag10_load_storage`), so with it installed the item
/// is a thousand cash for nothing at all. Off the shelf it cannot be chosen,
/// which is the same argument 작명의 비약 is gated by.
///
/// `CS_CASH_BAGSLOT=1` puts it back.
const MERCH_DROP: [(u8, u8); 1] = [(0x3e, 0)];

pub fn cash_bagslot() -> bool {
    knob("CS_CASH_BAGSLOT", 0) != 0
}

/// Cells per shop page — four columns by five rows. The shipped file has
/// twelve entries, so three more still fit on page one and `NPCUI_BAG_MAX`
/// stays 1; refusing to inject past this is what keeps the paging UI out of
/// the picture entirely.
const MERCH_PAGE: u32 = 20;

/// The id the shop's draw loop stops on. `NpcUI_DrawMerChant_Item` skips a row
/// only for this value.
const MERCH_END: u8 = 0xff;

/// `merchantState` while the cash shop is open. Together with `r0 == 2` it
/// picks the cash shop exactly: the town shops open with `setMerchant(0/1/3)`
/// and states 1/2/4.
const MERCH_STATE_CASH: u8 = 3;

/// `netSocket` fields the purchase reads, and the three globals its tail
/// writes. `errIdx = 6` is `getInCodeString(6)` = 구입하였습니다, which is what
/// the guest's own tail sets; 0 is what its own bag-full path
/// (`NpcUI_setBoughtItemOnCASH+0x2c`) writes, and `NpcUI_DrawErrorMsg` draws
/// nothing for it.
const SOCK_SLOT: u32 = 0xb4;
const SOCK_FLAG: u32 = 0x18;
const ERR_BOUGHT: u8 = 6;
const ERR_NONE: u8 = 0;

// ---- 「레시피2」 -----------------------------------------------------------
//
// A fifth row in the town merchant's category menu, selling the ten recipes
// the shipped 「레시피」 row does not. Why it has to be a new row rather than
// ten more entries on the old one is in `recipe2.rs`: `_merchant` is exactly
// 100 slots and the shipped recipe file is exactly 100 rows.
//
// The menu itself is four numbers and one string, in two functions:
//
//   NpcUI_DrawMerChant_Main+0x1c   mov ip,#0x2e8   the last label id, +1'd
//                                                  two instructions later into
//                                                  the loop bound
//   NpcUI_merchantKeyPressed+0x4b8 movne r2,#3     up  wraps to the last row
//                          +0x4e0  cmp r1,#0x3<<24 down wraps past the last
//                          +0x9f4  cmp sb,#4       the touch hit-test loop
//                          +0xa94  strb r2,[r3]    merchantState = cursor + 1
//
// The labels are `getInCodeString(0x2e5 + i)` — 아이템 / 레시피 / 유료상점 /
// 판매 — cached into a four-word array at `sp+0x40` that the tail re-reads to
// redraw the selected one in white. **The fifth word lands at `sp+0x50`**,
// which is inside the function's own `sub sp,sp,#0x54` frame and is written
// and read by nothing else; that is what makes one more row free.
//
// The one thing that is *not* free is the state. `merchantState` is
// `cursor + 1`, and `NpcUI_merchantKeyPressed+0x60` dispatches it through
// `cmp r3,#4 / addls pc,pc,r3,lsl #2` — a five-entry jump table with no room
// for a sixth. So the fifth row does not get a state of its own: the store is
// hooked and writes **2**, the recipe shop's own state. Everything downstream
// — the item grid, the info panel, the gold purchase, the back button — is
// then the recipe shop's shipped code, running against a list the host built
// instead of one `NpcUI_setMerchant` parsed out of `NPC/MERCHANT_1`.
//
// `CS_RECIPE2=0` leaves the menu with its shipped four rows and installs none
// of this.

/// Rows the shipped category menu has, and therefore the index of the one this
/// adds. `merchantState` would be `RECIPE2_ROW + 1`; see [`RECIPE2_STATE`].
const RECIPE2_ROW: u32 = 4;
/// Rows once the fifth is added.
const RECIPE2_ROWS: u32 = 5;
/// Highest cursor value, which is what an upward wrap lands on.
const RECIPE2_LAST_ROW: u32 = RECIPE2_ROWS - 1;

/// What the guest would write into `merchantState` for the new row, and what
/// is written instead. 2 is the recipe shop — the state whose every downstream
/// arm is exactly what this row wants.
const RECIPE2_STATE_RAW: u8 = 5;
const RECIPE2_STATE: u8 = 2;

/// The label. `getInCodeString(0x2e9)` is 아이템갯수: to every other caller, so
/// the swap is gated on the return address of the draw loop's own `bl`.
const RECIPE2_LABEL_ID: u32 = 0x2e9;
/// 레시피2, EUC-KR. Never a `str` — see the CLAUDE.md rule.
const RECIPE2_LABEL: &[u8] = b"\xb7\xb9\xbd\xc3\xc7\xc7\x32";

/// Grade colour for every injected row. The shipped recipe file spreads 0..4
/// over its hundred rows with no rule this could follow, so one value is
/// picked rather than a taxonomy invented; 2 is what the first recipes use.
const RECIPE2_GRADE: u8 = 2;
/// Quantity. Every shipped recipe row is 1.
const RECIPE2_QTY: u8 = 1;

/// `NpcUI_DrawMerChant_Main` offsets and the words at them.
const R2_DRAW_BOUND: u32 = 0x1c; // mov  ip, #0x2e8
const R2_DRAW_BOUND_WORD: u32 = 0xe3a0cfba;
const R2_DRAW_FIRST: u32 = 0x24; // mov  r6, #0x2e4
const R2_DRAW_FIRST_WORD: u32 = 0xe3a06fb9;
const R2_DRAW_INC: u32 = 0x28; // add  ip, ip, #1   (bound = 0x2e9)
const R2_DRAW_INC_WORD: u32 = 0xe28cc001;
/// `bl getInCodeString`; the instruction after it is the label swap's caller.
const R2_DRAW_CALL: u32 = 0x58;
const R2_DRAW_CACHE: u32 = 0x80; // str  r0, [ip, r8]   (sp+0x40 + 4i)
const R2_DRAW_CACHE_WORD: u32 = 0xe78c0008;
const R2_DRAW_LOOP: u32 = 0x130; // cmp  r6, r2
const R2_DRAW_LOOP_WORD: u32 = 0xe1560002;
const R2_DRAW_SEL: u32 = 0x14c; // add  r2, sp, #0x50   (one past the cache)
const R2_DRAW_SEL_WORD: u32 = 0xe28d2050;

/// `NpcUI_merchantKeyPressed` offsets and the words at them.
const R2_KEY_UP: u32 = 0x4b8; // movne r2, #3
const R2_KEY_UP_WORD: u32 = 0x13a02003;
const R2_KEY_UP_TEST: u32 = 0x4b0; // tst  r2, #0x80   (sets the NE this reads)
const R2_KEY_UP_TEST_WORD: u32 = 0xe3120080;
const R2_KEY_DOWN: u32 = 0x4e0; // cmp  r1, #0x3000000
const R2_KEY_DOWN_WORD: u32 = 0xe3510403;
const R2_KEY_DOWN_SHIFT: u32 = 0x4dc; // lsl  r1, r2, #0x18
const R2_KEY_DOWN_SHIFT_WORD: u32 = 0xe1a01c02;
const R2_KEY_DOWN_WRAP: u32 = 0x4e8; // movgt r2, #0
const R2_KEY_DOWN_WRAP_WORD: u32 = 0xc3a02000;
const R2_KEY_HIT: u32 = 0x9f4; // cmp  sb, #4
const R2_KEY_HIT_WORD: u32 = 0xe3590004;
const R2_KEY_HIT_STEP: u32 = 0x9f8; // add  sl, sl, #0x1a   (the row pitch)
const R2_KEY_HIT_STEP_WORD: u32 = 0xe28aa01a;
const R2_KEY_STATE: u32 = 0xa94; // strb r2, [r3]   (merchantState)
const R2_KEY_STATE_WORD: u32 = 0xe5c32000;
const R2_KEY_LOAD: u32 = 0xa98; // bl   NpcUI_setMerchant

/// `CS_RECIPE2=0` is the shipped four-row menu. On by default: the ten recipes
/// it sells have names, descriptions and item templates in the game's own data
/// and no other way at all to reach a player.
pub fn recipe2() -> bool {
    knob("CS_RECIPE2", 1) != 0
}

/// Gold per recipe. The shipped recipe shop charges 500 (32 rows), 800 (66)
/// and 1200 (2), so 1000 sits inside its own range and above all but two of
/// it — these ten are the late ones. A knob because it is taste, not a fact
/// read off the binary: the item templates price nine of the ten at **0**,
/// which is exactly the thing that had to be decided.
pub fn recipe2_price() -> i32 {
    knob("CS_RECIPE2_PRICE", 1000)
}

/// Where the five hooks go, or `None` if any word has drifted.
///
/// Every checked word is load-bearing in a way one of them alone is not. The
/// draw hook lets the label loop run one more time, which is only safe while
/// the cache store, the loop bound and the selected-row re-read are the ones
/// this was written against — otherwise a fifth pointer lands somewhere
/// unknown. On the key side the two `cmp`s are answered with **flags**, so the
/// instructions that consume those flags are checked as well.
pub fn recipe2_sites(
    draw: u32,
    key: u32,
    set_merchant: u32,
    in_code_string: u32,
    mem: &mut GuestMem,
) -> Option<Vec<(u32, Patch)>> {
    let words: [(u32, u32, u32); 14] = [
        (draw, R2_DRAW_BOUND, R2_DRAW_BOUND_WORD),
        (draw, R2_DRAW_FIRST, R2_DRAW_FIRST_WORD),
        (draw, R2_DRAW_INC, R2_DRAW_INC_WORD),
        (draw, R2_DRAW_CACHE, R2_DRAW_CACHE_WORD),
        (draw, R2_DRAW_LOOP, R2_DRAW_LOOP_WORD),
        (draw, R2_DRAW_SEL, R2_DRAW_SEL_WORD),
        (key, R2_KEY_UP, R2_KEY_UP_WORD),
        (key, R2_KEY_UP_TEST, R2_KEY_UP_TEST_WORD),
        (key, R2_KEY_DOWN, R2_KEY_DOWN_WORD),
        (key, R2_KEY_DOWN_SHIFT, R2_KEY_DOWN_SHIFT_WORD),
        (key, R2_KEY_DOWN_WRAP, R2_KEY_DOWN_WRAP_WORD),
        (key, R2_KEY_HIT, R2_KEY_HIT_WORD),
        (key, R2_KEY_HIT_STEP, R2_KEY_HIT_STEP_WORD),
        (key, R2_KEY_STATE, R2_KEY_STATE_WORD),
    ];
    let ok = words.iter().all(|&(base, off, want)| mem.r32(base.wrapping_add(off)) == want)
        // And the two `bl`s that say what the surrounding code is *for*: the
        // label the draw loop asks for, and the loader the menu calls.
        && branch_target(draw.wrapping_add(R2_DRAW_CALL), mem, true) == Some(in_code_string)
        && branch_target(key.wrapping_add(R2_KEY_LOAD), mem, true) == Some(set_merchant);
    ok.then(|| {
        vec![
            (draw.wrapping_add(R2_DRAW_BOUND), Patch::Recipe2Rows),
            (key.wrapping_add(R2_KEY_UP), Patch::Recipe2WrapUp),
            (key.wrapping_add(R2_KEY_DOWN), Patch::Recipe2WrapDown),
            (key.wrapping_add(R2_KEY_HIT), Patch::Recipe2HitRows),
            (key.wrapping_add(R2_KEY_STATE), Patch::Recipe2State),
        ]
    })
}

/// The return address the label swap requires — the instruction after the draw
/// loop's `bl getInCodeString`.
pub fn recipe2_label_lr(draw: u32) -> u32 {
    draw.wrapping_add(R2_DRAW_CALL).wrapping_add(4)
}

// ---- 펫 「스마트모드」 ------------------------------------------------------
//
// A fifth 전투타입 on the pet command menu: hunt treasure when there is any,
// fight otherwise. The shipped four are 단독 전투 / 협력 전투 / 보물탐색 /
// 전투안함, all of them one byte — `pet[0x656]`, written by
// `Pet_set_condition` and saved by `GameFrame_savePetData`.
//
// **보물탐색 is already in CS1**, which is what makes this small: the arm that
// does it is shipped, and so is the arm that fights. What is missing is a
// value that chooses between them, and `Pet_Process+0x300` is a five-entry
// jump table with no fifth case:
//
//   +0x2f0  ldrb  r3, [r5, #0x656]
//   +0x2fc  cmp   r3, #3
//   +0x300  addls pc, pc, r3, lsl #2      0 단독 / 1 협력 / 2 보물 / 3 안함
//
// So the fifth value never reaches the table: the `ldrb` is hooked and answers
// **one of the shipped four**, picked fresh each tick. Every arm, every state
// transition and every attack is then the guest's own code — this adds no pet
// AI, it only chooses which shipped AI runs this tick.
//
// The choice is: treasure when `HERO_Check_Treasure` turns up something
// `MONSTER_isObject` agrees is an object, and combat otherwise.
//
// **Busy ticks keep the arm that started the action.** Each arm opens with
// `ldrb r3,[r5,#0xc0]` and jumps away when the pet is mid-action, and the
// three continuations are not the same: 보물탐색's (`+0x564`) is a plain
// "carry on", while 협력's (`+0x55c`) and 단독's (`+0x5ac`) first test
// `state == 4` and continue the attack. Answering combat for every busy tick
// was tried first and is wrong in a way the probe showed at once: the treasure
// arm uncovers a chest by hitting it (state 4), the combat continuation then
// took over that state with the chest still in `pet[0x288]`, and the pet stood
// on the chest "attacking" it for the rest of the run. Keeping the last answer
// lets each arm finish what it began; the next idle tick decides afresh.
//
// Three other places read the byte at runtime and two of them have to be told
// the same story:
//
// | | |
// |---|---|
// | `Pet_Process+0x9c` | `v == 3` → 전투안함. 4 is not 3, so it is already right. |
// | `Pet_move_process+0x28` | `v > 3` **unsigned** → `Pet_moveDiagonal(0,0)`, i.e. stop. Must be told the decision or the pet never moves. |
// | `Pet_move_process+0xac` | `v == 2` → the approach a treasure hunter makes. Must match the decision. |
// | `HERO_Check_Autotarget+0xe8` | `v == 2` → accept objects as targets. First thought to be reachable only from the 단독 arm; it is not — `HERO_End_Process` asks it for the pet every tick, and left at 4 the pet froze in state 1 beside the chest it had just uncovered. Told the decision. |
//
// The menu side is two clamps, a label and a description. All four of the
// shipped labels and descriptions are `getInCodeString` ids — 0x12f + value
// and 0x121 + value — so the fifth of each is served from the string hook,
// gated on the call site's return address.

/// The fifth 전투타입, and the shipped one whose arm it borrows for treasure.
const PET_SMART: u8 = 4;
const PET_TREASURE: u8 = 2;

/// Fields of a pet (a `hero` at index `CUR_HERO + 1`).
const PET_STATE: u32 = 0xc0;
const PET_TARGET: u32 = 0x288;
const PET_COND: u32 = 0x656;

/// `_monsterList` is 0x11c bytes of pointers, which is what bounds the index
/// `HERO_Check_Treasure` leaves in `pet[0x288]`.
const PET_LIST_SLOTS: u32 = 0x11c / 4;

/// `Pet_Process` — the byte the jump table dispatches on.
const PET_PICK: u32 = 0x2f0;
const PET_PICK_WORD: u32 = 0xe5d5_3656; // ldrb  r3, [r5, #0x656]
const PET_PICK_CMP: u32 = 0x2fc;
const PET_PICK_CMP_WORD: u32 = 0xe353_0003; // cmp   r3, #3
const PET_PICK_TABLE: u32 = 0x300;
const PET_PICK_TABLE_WORD: u32 = 0x908f_f103; // addls pc, pc, r3, lsl #2

/// `Pet_move_process` — the two reads that have to hear the same answer.
const PET_MOVE_GATE: u32 = 0x28;
const PET_MOVE_GATE_WORD: u32 = 0xe5d7_3656; // ldrb  r3, [r7, #0x656]
const PET_MOVE_STOP: u32 = 0x34;
const PET_MOVE_STOP_WORD: u32 = 0xe353_0403; // cmp   r3, #0x3000000  (v > 3 stops)
const PET_MOVE_TREASURE: u32 = 0xac;
const PET_MOVE_TREASURE_WORD: u32 = 0xe5d7_3656;
const PET_MOVE_IS_TREASURE: u32 = 0xb0;
const PET_MOVE_IS_TREASURE_WORD: u32 = 0xe353_0002; // cmp   r3, #2

/// `HERO_Check_Autotarget` — `v == 2` accepts objects as targets. Not only
/// reached from 단독 전투's arm: `HERO_End_Process` runs for the pet as it does
/// for the hero (both are `Map_objectEndProcess` calls) and asks it too, which
/// is what settles the pet's state after a chest is uncovered.
const PET_TARGET_GATE: u32 = 0xe8;
const PET_TARGET_GATE_WORD: u32 = 0xe5d5_3656; // ldrb  r3, [r5, #0x656]
const PET_TARGET_IS_TREASURE: u32 = 0xec;
const PET_TARGET_IS_TREASURE_WORD: u32 = 0xe353_0002; // cmp   r3, #2

/// `GameMenuCharPetKey` — the two clamps that hold the row to four values.
const PET_MENU_UP: u32 = 0x680;
const PET_MENU_UP_WORD: u32 = 0xd3a0_2003; // movle r2, #3       (← wraps here)
const PET_MENU_DOWN: u32 = 0x6a4;
const PET_MENU_DOWN_WORD: u32 = 0xe351_0402; // cmp   r1, #0x2000000 (→ wraps past)
/// Both arms end in the same call, which is what says the row is this one.
const PET_MENU_UP_SET: u32 = 0x694;
const PET_MENU_DOWN_SET: u32 = 0x6c4;

/// `draw_char_pet` — the value label is `getInCodeString(0x12f + value)`, and
/// this is the instruction after that call.
const PET_LABEL_LR: u32 = 0x730;
const PET_LABEL_CALL: u32 = 0x72c;
const PET_LABEL_ID: u32 = 0x12f + PET_SMART as u32;

/// `draw_pet_info` — the description is a five-arm jump table on the same
/// byte, whose default arm writes no text at all. The pick is answered 0 so
/// the first arm runs, and its `getInCodeString(0x121)` is swapped for ours.
const PET_DESC_PICK: u32 = 0x4c;
const PET_DESC_PICK_WORD: u32 = 0xe1d3_30d1; // ldrsb r3, [r3, #1]
/// The same read again at `+0x3e4`, this time indexing the per-value text
/// buffers (`sp+0x28 + value*0xc8`) for the marquee. The arm above wrote
/// buffer 0, so this must say 0 as well or the marquee shows an empty slot.
const PET_DESC_SHOW: u32 = 0x3e4;
const PET_DESC_SHOW_WORD: u32 = 0xe1d3_30d1;
/// ...and a third time at `+0x328`, into `lr`, for the pointer the draw call
/// is handed. Three reads, one byte, one answer.
const PET_DESC_DRAW: u32 = 0x328;
const PET_DESC_DRAW_WORD: u32 = 0xe1d3_e0d1; // ldrsb lr, [r3, #1]
const PET_DESC_CMP: u32 = 0x50;
const PET_DESC_CMP_WORD: u32 = 0xe353_0003; // cmp   r3, #3
const PET_DESC_TABLE: u32 = 0x54;
const PET_DESC_TABLE_WORD: u32 = 0x908f_f103;
const PET_DESC_CALL: u32 = 0x3fc;
const PET_DESC_LR: u32 = 0x400;
const PET_DESC_ID: u32 = 0x121;

/// 스마트, EUC-KR.
const PET_SMART_LABEL: &[u8] = b"\xbd\xba\xb8\xb6\xc6\xae";
/// 보물이 보이면 먼저 줍고, 없으면 스스로 교전합니다. The leading `r` is the
/// colour escape every shipped description starts with. **No `%`** — the
/// description is handed to `MC_knlSprintk` as the format.
const PET_SMART_DESC: &[u8] = b"\x72\xba\xb8\xb9\xb0\xc0\xcc\x20\xba\xb8\xc0\xcc\xb8\xe9\x20\xb8\xd5\xc0\xfa\x20\xc1\xdd\xb0\xed\x2c\x20\xbe\xf8\xc0\xb8\xb8\xe9\x20\xbd\xba\xbd\xba\xb7\xce\x20\xb1\xb3\xc0\xfc\xc7\xd5\xb4\xcf\xb4\xd9\x2e";

/// A treasure chest the pet has uncovered can be broken by the hero.
///
/// Shipped, it cannot: `MONSTER_Clone` gives every object `HP = 0`, and the
/// first thing `Hero_canAttack` does is refuse anything whose HP is not above
/// zero — so the chest is never a target and the attack never leaves the
/// ground. Nothing else about it is in the way: `MONSTER_isDie` reads the
/// *state* byte (an uncovered chest is 0, not the 5 or 7 that mean dead),
/// `CHAR_getVisible` is already true once the pet has uncovered it, and the
/// object arm right after returns 1. This lets that one comparison through.
///
/// Confirmed against the shipped builds: the chest is unbreakable in
/// versionCode 82 and 84 as well, so this is the game's own defect, not a
/// regression from any of the host patches.
pub fn chest_break() -> bool {
    knob("CS_CHEST_BREAK", 1) != 0
}

/// `Hero_canAttack+0x28`, the `cmp r3, #0` that guards the `ble` refusal.
const CHEST_HP_CMP: u32 = 0x28;

/// The four words the hook stands on: the monster reaching `r5`, the HP load
/// it compares, the compare itself, and the `ble` that acts on it. If any of
/// them moved, the register this reads is not the monster any more.
const CHEST_SITE: [(u32, u32); 4] = [
    (0x14, 0xe1a0_5000), // mov  r5, r0
    (0x20, 0xe595_3120), // ldr  r3, [r5, #0x120]
    (0x28, 0xe353_0000), // cmp  r3, #0
    (0x2c, 0xda00_0002), // ble  +0x8
];

/// Where to install [`Patch::ChestBreak`], if the site still looks right.
pub fn chest_break_site(can_attack: u32, mem: &mut GuestMem) -> Option<u32> {
    CHEST_SITE
        .iter()
        .all(|&(off, word)| mem.r32(can_attack.wrapping_add(off)) == word)
        .then(|| can_attack.wrapping_add(CHEST_HP_CMP))
}

/// Diagnostic: log why a treasure object is refused as an attack target.
///
/// Off by default. It only reads guest memory and always declines, so a build
/// with it on behaves identically — it just talks.
pub fn box_debug() -> bool {
    knob("CS_BOX_DEBUG", 0) != 0
}

/// `CS_PET_SMART=0` leaves the pet menu with its shipped four 전투타입.
pub fn pet_smart() -> bool {
    knob("CS_PET_SMART", 1) != 0
}

/// Which shipped arm 스마트 falls back to when there is nothing to pick up:
/// 0 = 단독 전투 (the pet picks its own fights), 1 = 협력 전투 (it only
/// fights what the hero is fighting). 단독 by default — a mode called 스마트
/// that waits to be told what to attack would be the smaller half of what was
/// asked for. Clamped, because any other value would land on 보물탐색 or
/// 전투안함 and the mode would never fight at all.
pub fn pet_smart_combat() -> u8 {
    knob("CS_PET_SMART_COMBAT", 0).clamp(0, 1) as u8
}

/// Where the five hooks go, or `None` if any word has drifted.
///
/// Both jump tables are checked as well as the byte reads: this feature is
/// *about* those tables having four arms, and a build that grew a fifth would
/// want none of this.
#[allow(clippy::too_many_arguments)]
pub fn pet_smart_sites(
    process: u32,
    move_process: u32,
    autotarget: u32,
    menu: u32,
    info: u32,
    label: u32,
    set_condition: u32,
    in_code_string: u32,
    mem: &mut GuestMem,
) -> Option<Vec<(u32, Patch)>> {
    let words: [(u32, u32, u32); 15] = [
        (autotarget, PET_TARGET_GATE, PET_TARGET_GATE_WORD),
        (autotarget, PET_TARGET_IS_TREASURE, PET_TARGET_IS_TREASURE_WORD),
        (process, PET_PICK, PET_PICK_WORD),
        (process, PET_PICK_CMP, PET_PICK_CMP_WORD),
        (process, PET_PICK_TABLE, PET_PICK_TABLE_WORD),
        (move_process, PET_MOVE_GATE, PET_MOVE_GATE_WORD),
        (move_process, PET_MOVE_STOP, PET_MOVE_STOP_WORD),
        (move_process, PET_MOVE_TREASURE, PET_MOVE_TREASURE_WORD),
        (move_process, PET_MOVE_IS_TREASURE, PET_MOVE_IS_TREASURE_WORD),
        (menu, PET_MENU_UP, PET_MENU_UP_WORD),
        (menu, PET_MENU_DOWN, PET_MENU_DOWN_WORD),
        (info, PET_DESC_PICK, PET_DESC_PICK_WORD),
        (info, PET_DESC_SHOW, PET_DESC_SHOW_WORD),
        (info, PET_DESC_DRAW, PET_DESC_DRAW_WORD),
        (info, PET_DESC_CMP, PET_DESC_CMP_WORD),
    ];
    let ok = words.iter().all(|&(base, off, want)| mem.r32(base.wrapping_add(off)) == want)
        && mem.r32(info.wrapping_add(PET_DESC_TABLE)) == PET_DESC_TABLE_WORD
        // The two menu arms are the ones that set this row, and the two
        // `getInCodeString` calls are the ones whose text is swapped.
        && branch_target(menu.wrapping_add(PET_MENU_UP_SET), mem, true) == Some(set_condition)
        && branch_target(menu.wrapping_add(PET_MENU_DOWN_SET), mem, true) == Some(set_condition)
        && branch_target(info.wrapping_add(PET_DESC_CALL), mem, true) == Some(in_code_string)
        && branch_target(label.wrapping_add(PET_LABEL_CALL), mem, true) == Some(in_code_string);
    ok.then(|| {
        vec![
            (process.wrapping_add(PET_PICK), Patch::PetSmartPick),
            (move_process.wrapping_add(PET_MOVE_GATE), Patch::PetSmartMove),
            (move_process.wrapping_add(PET_MOVE_TREASURE), Patch::PetSmartMove),
            (autotarget.wrapping_add(PET_TARGET_GATE), Patch::PetSmartTarget),
            (menu.wrapping_add(PET_MENU_UP), Patch::PetSmartWrapUp),
            (menu.wrapping_add(PET_MENU_DOWN), Patch::PetSmartWrapDown),
            (info.wrapping_add(PET_DESC_PICK), Patch::PetSmartDesc),
            (info.wrapping_add(PET_DESC_SHOW), Patch::PetSmartDesc),
            (info.wrapping_add(PET_DESC_DRAW), Patch::PetSmartDesc),
        ]
    })
}

/// The return address the value-label swap requires.
pub fn pet_label_lr(draw_char_pet: u32) -> u32 {
    draw_char_pet.wrapping_add(PET_LABEL_LR)
}

/// ...and the one the description swap requires.
pub fn pet_desc_lr(draw_pet_info: u32) -> u32 {
    draw_pet_info.wrapping_add(PET_DESC_LR)
}

// ---- 펫 작명 -------------------------------------------------------------
//
// The naming screen the story shows once, reopened on demand. Every part of it
// is the guest's: the on-screen Hangul keyboard, the five-character check, the
// confirm popup and the write into the pet. All the host does is call
// `InputInit(1)` and, afterwards, make sure the result reaches the disk.
//
// It works outside a cutscene because the keyboard is not part of any game
// frame. `MX_imeCall` sets `_midas_internalImeInstance[0x40]`, and that one byte
// switches two places that sit *above* the frame loop:
//
//   * `CUSTOM_grpFlushLcd+0x20` paints `imeLocal_QWERTY_Paint` over the LCD
//     before the flush, and the flush is `timerCB+0x60` — so the keyboard draws
//     every frame whatever screen is up.
//   * `clsidAuthHandle+0x104` routes events to `imeLocal_onHandleEvent` instead
//     of `onHandleEvent`, and our touches already arrive there
//     (`Java_..._nativeTouchHandleEvent` -> `handleCletEvent` ->
//     `clsidAuthHandle`). So it is modal for free, and needs no new plumbing.
//
// `imeLocal_button_init` lays the keys out from `__knlGR_getScreenFrameBuffer`,
// so 400x240 and 533x240 both fit without being told.

/// `InputInit(mode)`: 1 is the pet-name mode — a 14-byte limit and the default
/// name as the initial string. 0 is the letter/mail composer.
const IME_MODE_PET: u32 = 1;

// ---- 키보드 크기 ---------------------------------------------------------
//
// The shipped keyboard does not fit this screen. Measured at 533x240, which is
// what the device actually runs: the panel is **293 px tall on a 240 px screen**,
// bottom-anchored, so its top 53 px are off-screen — and the input line lives in
// those 53 px. The player never sees what they are typing. That is the whole of
// the "backspace doesn't work" report: `imeCore_MakeHangul_revertIng` edits the
// jamo of the composing syllable first, and the edit was happening somewhere the
// player could not see.
//
// **The keyboard's height is a function of the surface's WIDTH, not its height.**
// `imeLocal_button_init` fits ten keys across, derives the key height from the
// key width, and stacks four rows — so a wider screen gives a *taller* keyboard.
// Measured, one run per point:
//
//   surface w  |  213  266  319  399  479  533
//   panel h    |  129  154  178  219  260  293
//
// and the height it is handed changes nothing but where the bottom lands
// (`panel.y + panel.h == h`, always). That is why the previous attempt failed:
// it shrank the *height*, which only slid the same 293 px block further up.
//
// So the seam is `__knlGR_getScreenFrameBuffer`, and only at **one** of its seven
// callers. Hand `imeLocal_button_init` a **narrower** surface and it lays itself
// out smaller, all forty-odd metrics and the key table together, by its own
// arithmetic. Everything else keeps seeing the real screen:
//
//   * `imeLocal_QWERTY_Paint` never reads `w`/`h` — it only passes the pointer on
//     to `imeLocal_DrawRoundRect`/`DrawButton`/`DrawString`.
//   * both hit tests read `w`/`h` only inside their `ime[0x296] == ±90` branches,
//     and this keyboard is never rotated.
//
// Narrowing the *base pointer* as well would be a mistake, and it is worth
// recording why, because it looks like the obvious way to centre the result.
// **Glyphs do not go through the context at all.** `_midas_funcFntDrawPixelPreRGB`
// writes to the globals — `mla r5, *_midas_lcdWidth, y, x` into `*_midas_lcd` —
// so text lands at raw screen coordinates whatever framebuffer the caller passed
// (`font.rs` transcribes this faithfully; it is not a host bug). Move the window's
// origin and every key label scatters across the screen while its rounded
// rectangle stays put — which is exactly what a 60%-wide centred window produced.
// So the window shares the screen's origin, and the centring is done afterwards
// by translating the metrics the layout produced.
//
// `CS_IME_H` is the percentage of the screen height the keyboard may occupy;
// `CS_IME_H=0` installs nothing and leaves the shipped, overflowing layout.

/// The one IME return address that gets a narrowed surface: the layout's own
/// `bl __knlGR_getScreenFrameBuffer` plus four. The `bl` is decoded at install to
/// make sure it still calls that function.
const IME_SURFACE_SITES: [(&str, u32); 1] = [("imeLocal_button_init", 0x18)];

/// The key table `imeLocal_button_init` fills: `(x, y, w, h)` as signed
/// halfwords at `ime + 0x2d8 + i*0x14`, with the key code a word at `+0xc`.
///
/// Read off the row loop at `imeLocal_button_init+0x47c`, which walks a pointer
/// from `ime + 0x2e0` by 0x14 and writes `[r3-8] = x`, `[r3-6] = y`,
/// `[r3-4] = w`, `[r3-2] = h` — ten keys a row, with `y`, `w` and `h` constant
/// across the row and only `x` advancing. The field order is confirmed from the
/// other end by `imeLocal_QWERTY_Paint+0x608`, which loads `[r4]`, `[r4,#-4]`,
/// `[r4,#-2]`, `[r4,#2]` off `ime + 0x2dc + i*0x14` as `w, x, y, h` for the
/// backspace icon.
///
/// **The coordinates are plain top-left screen pixels**, not far-edge relative:
/// `imeLocal_onHandleEventTouch+0xbc..+0x110` compares the touch against
/// `x - 2 .. x + w + 2` and `y - 2 .. y + h + slop` directly. The `w - x` /
/// `h - y` forms next to them belong to the `ime[0x296] == ±0x5a` rotated
/// layouts, which this keyboard never selects.
const IME_KEYS: u32 = 0x2d8;
const IME_KEY_STRIDE: u32 = 0x14;

/// How many records there are — the same count the hit test walks, and it depends
/// on the keyboard mode (`ime[0x48]`).
const IME_KEYS_QWERTY: u32 = 0x1f;
const IME_KEYS_NUMERIC: u32 = 0x12;
const IME_MODE: u32 = 0x48;

/// Everything `imeLocal_QWERTY_Paint` positions itself from that is not a key, as
/// `(what, x, y, w, h)` halfword offsets into the instance.
///
/// Read off its two `imeLocal_DrawRoundRect` calls — the keyboard panel at
/// `+0x13c` and the input line at `+0x18c`. `imeLocal_DrawRoundRect(g, x, y, w,
/// h, radius, ...)`: the first three are registers and `h` is the first stack
/// word, confirmed by its own prologue (`push` of 9 + `sub sp,#0x3c` puts the
/// caller's first stack word at `[sp,#0x60]`, which it uses as the height).
///
/// The x of both, and of every key, is what [`Vm::host_ime_layout`] translates to
/// centre the narrowed keyboard; the y and the two sizes are left exactly as the
/// guest computed them.
const IME_BOXES: [(&str, u32, u32, u32, u32); 2] =
    [("panel", 0x2c4, 0x2c6, 0x2c8, 0x2ca), ("input", 0x56c, 0x56e, 0x570, 0x572)];

/// Percentage of the screen height the keyboard may occupy, or 0 for the shipped
/// layout. The width that produces it is searched for at run time, since the only
/// honest source for "how tall is a keyboard this wide" is the layout itself.
///
/// The floor of 25 is there so a value that leaves the keys untappable cannot be
/// set; 100 still helps, because the shipped layout overflows a 240 px screen.
pub fn ime_height_pct() -> i32 {
    match knob("CS_IME_H", 65) {
        0 => 0,
        v => v.clamp(25, 100),
    }
}

/// `CS_IME_DEBUG=1` logs every key the keyboard resolves. Off costs nothing —
/// the hook is not installed.
pub fn ime_debug() -> bool {
    knob("CS_IME_DEBUG", 0) != 0
}

/// Hooked at entry when [`ime_debug`] is on.
pub const PATCHED_IME_LOG: [(&str, Patch); 1] = [("imeLocal_onHandleEventKey", Patch::ImeKeyLog)];

/// Installed together with the sub-window: the window gives the drawing area and
/// this makes the keys fit it. Neither is much use alone.
pub const PATCHED_IME_SIZE: [(&str, Patch); 1] = [("imeLocal_button_init", Patch::ImeLayout)];

pub const IME_SURFACE_SITE_COUNT: usize = IME_SURFACE_SITES.len();

/// The symbols [`ime_surface_sites`] wants resolved, in its own order — so the
/// caller can look them up before lending out `mem`.
pub const IME_SURFACE_NAMES: [&str; IME_SURFACE_SITE_COUNT] = [IME_SURFACE_SITES[0].0];

/// Resolve [`IME_SURFACE_SITES`], or empty when there is nothing to shrink or the
/// guest is not the binary this was written against.
pub fn ime_surface_sites(bases: &[Option<u32>], get_fb: u32, mem: &mut GuestMem) -> Vec<u32> {
    if ime_height_pct() == 0 {
        return Vec::new();
    }
    IME_SURFACE_SITES
        .iter()
        .zip(bases)
        .filter_map(|(&(_, off), &base)| {
            let at = base?.wrapping_add(off);
            (branch_target(at.wrapping_sub(4), mem, true) == Some(get_fb)).then_some(at)
        })
        .collect()
}

/// Property 2 of `MX_imeSetProperty` — the initial string. It `strcpy`s into
/// both `ime+0x74` and `ime+0x174` and sets the length, so passing guest memory
/// is safe and nothing retains the pointer.
const IME_PROP_TEXT: u32 = 2;

// ---- 빈 칸에서의 백스페이스 ------------------------------------------------
//
// Reported: empty the name completely, press `←` once more, and the naming
// screen closes with the **old** name in the confirm popup.
//
// That is the shipped code doing what it was written to do. `←` on an empty
// buffer is the keyboard's **cancel**:
//
//   +0x36c  ldr  r1, [r3, #0x64]        ; the composing jamo, if any
//   +0x37c  bne  +0x454                 ; composing -> revert a jamo instead
//   +0x380  ldrb r2, [r3, #0x174]       ; the committed text
//   +0x388  beq  +0x59c                 ; nothing left to delete -> cancel
//   +0x5a4  mvn  r5, #6                 ; r5 = -7, the cancel marker
//   +0x13c  ... cmn r5, #7 -> imeLocal_Release
//           strcpy(ime + 0x174, ime + 0x74)   ; put the initial string back
//           r0 = 1                            ; 1 = cancelled, 0 = 완료
//           blx  ime[0x28c]                   ; the close callback
//
// The restore is why the *old* name comes back, and the reason it is treated as
// a commit is that `onCloseIME_MainFrame+0x8` is `cmp r0, #1 / bls` — it accepts
// **both** 0 and 1 and never looks at which. So the cancel flag exists and is
// thrown away one function later. (This also settles a claim recorded earlier in
// `CLAUDE.md`: `-5` is *not* the only way out of the overlay.)
//
// Suppressing the cancel needs no new code path, because the guest already has
// two "backspace did nothing" exits — `imeCore_MakeHangul_revertIng` returning
// non-zero (`+0x3a0`) and a non-positive length (`+0x3ac`) both branch to
// `+0x13c` with `r5` still the key code, where `cmn r5, #7` fails and it simply
// returns. So the empty case is pointed at **that** exit instead of the cancel,
// and `←` becomes what the player expects: it deletes, and when there is nothing
// to delete it does nothing.

/// `imeLocal_onHandleEventKey`: the `beq` that sends an empty buffer to the
/// cancel, and the no-op exit it is redirected to.
const IME_BS_TEST: u32 = 0x388;
const IME_BS_NOTHING: u32 = 0x13c;

/// The four words checked at install, as `(offset, word)`. The `beq`'s own
/// encoding pins its target: `0x0a000083` is `beq pc + 8 + 0x83*4` = `+0x59c`,
/// the cancel block, so a rebuilt binary that moved either end fails here.
const IME_BS_SITE: [(u32, u32); 4] = [
    (0x380, 0xe5d3_2174),          // ldrb r2, [r3, #0x174]
    (0x384, 0xe352_0000),          // cmp  r2, #0
    (IME_BS_TEST, 0x0a00_0083),    // beq  +0x59c
    (IME_BS_NOTHING, 0xe084_7007), // add  r7, r4, r7   — the no-op exit
];

// ---- 확인창의 죽은 버튼 ----------------------------------------------------
//
// Reported: on the 이 이름으로 결정 하시겠습니까? popup, the bottom-left arrows
// and the bottom-right OK button do nothing.
//
// They are not the popup's. `NameTitle_drawSysMsg`'s `nameTitleIdx == 0x75` block
// draws a rounded panel, the question, the name and 예/아니오 — nothing else — and
// `MainFrame_KeyPressed+0x11c` hit-tests exactly two rects,
// `(w/2 - 0x35 + i*0x3a, h/2 + 7, 0x30, 0x10)`. The chrome the player is tapping
// belongs to the **cash shop underneath**: measured with `CS_WATCH=Check_Rect`, a
// tap at the OK button is `Check_Rect(x, y, w - 0x37, h - 0x3f)` with `0x37 x
// 0x3f` from `NpcUI_tableKeyPressed+0x66c`. `MainFrame_KeyPressed` runs first in
// `timerCB`, so while the popup is up that button reaches nobody.
//
// The popup's key path already has everything needed: its jump table sends `-5`
// to the activate block and `-4`/`-3` to a plain `eor r2, r2, #1` on
// `nameTitleSel`. So a tap on the OK button is rewritten into key `-5`, and the
// guest does the rest.
//
// **The arrows are deliberately left dead.** The 0x75 render block never reads
// `nameTitleSel` — the selection is invisible — so an arrow would silently change
// what OK does with nothing on screen to show it. For the same reason the OK tap
// pins `nameTitleSel` to 예 first rather than activating whatever the two-tap
// idiom last left there.
//
// Gated on `pet_rename_pending`, so this is our shop-opened naming only; the
// story's own naming scene has no chrome behind it and is untouched.

/// `nameTitleIdx` when the confirm popup is up.
const NAME_CONFIRM: i32 = 0x75;
/// `nameTitleSel` for 예.
const NAME_SEL_YES: u8 = 0;
/// `mainFrame`: the event kind, the touch x (which doubles as the key code) and
/// the touch y — all words, read at `MainFrame_KeyPressed+0x18`, `+0x14c` and
/// `+0x15c`.
const MF_EVENT: u32 = 0x5c;
const MF_X: u32 = 0x38;
const MF_Y: u32 = 0x40;
/// The two event kinds the 0x75 branch distinguishes.
const MF_KEY: u32 = 2;
const MF_TOUCH: u32 = 0x13;
/// The popup's activate key.
const NAME_KEY_OK: u32 = -5i32 as u32;
/// The standard bottom-right OK button, as its size — the rect is that size
/// flush into the far corner, `(w - bw, h - bh, bw, bh)`.
const OK_BUTTON: (i32, i32) = (0x37, 0x3f);

/// Words checked at install: the event-kind load, the `cmp r2, #0x75` that picks
/// the popup, and the two kind tests inside it. Without all four the rewrite
/// would be feeding a branch that no longer exists.
const NAME_OK_SITE: [(u32, u32); 4] = [
    (0x18, 0xe593_005c),                       // ldr r0, [r3, #0x5c]
    (0x48, 0xe352_0000 | NAME_CONFIRM as u32), // cmp r2, #0x75
    (0x11c, 0xe350_0000 | MF_KEY),             // cmp r0, #2      (key)
    (0x124, 0xe350_0000 | MF_TOUCH),           // cmp r0, #0x13 (touch)
];

/// `CS_NAME_OK=0` leaves the OK button dead.
pub fn name_ok_button() -> bool {
    knob("CS_NAME_OK", 1) != 0
}

/// Resolve the hook, or nothing when the knob is off or the branch moved.
pub fn name_ok_site(base: Option<u32>, mem: &mut GuestMem) -> Option<u32> {
    if !name_ok_button() {
        return None;
    }
    let base = base?;
    NAME_OK_SITE.iter().all(|&(off, word)| mem.r32(base + off) == word).then_some(base)
}

/// `CS_NAME_BACKSPACE=0` restores the shipped cancel.
pub fn ime_backspace_floor() -> bool {
    knob("CS_NAME_BACKSPACE", 1) != 0
}

/// Resolve the hook, or nothing when the knob is off or the block moved.
pub fn ime_backspace_site(base: Option<u32>, mem: &mut GuestMem) -> Option<u32> {
    if !ime_backspace_floor() {
        return None;
    }
    let base = base?;
    IME_BS_SITE.iter().all(|&(off, word)| mem.r32(base + off) == word).then_some(base + IME_BS_TEST)
}

/// `hero[0x62c]` on **hero 0** — "has a pet", set by `Event_getPet+0x7c`.
/// `GameFrame_saveGame+0x1b4` reads the same byte and resets the name to the
/// default when it is clear, which is the other half of why an unowned pet must
/// not be renamed: the write would simply be undone at the next save.
const HERO_HAS_PET: u32 = 0x62c;

/// `hero[0xc6]` — the class. `MainMenuFrame_KeyPressed+0x2790` writes it out of
/// `_saveSlot` before it loads the slot's files, and `Load_TalentData` takes
/// the same value.
const HERO_CLASS: u32 = 0xc6;

/// `item[0x98]` — which equipment slot this item is worn in, `-1` for one that
/// is only in the bag. `ITEM_isEquipSetSlot` is the `>= 0` test;
/// `Item_saveStorage` writes the byte and `Item_loadStorage` reads it back,
/// which is how [`Vm::host_equip_restore`] tells a worn item from a spare.
const ITEM_EQUIPPED: u32 = 0x98;

/// The pet is hero 1, and its name is 0x28 bytes at `+0x62d`. Both numbers are
/// the guest's own: `GameFrame_savePetData+0xa8` copies exactly that range out
/// of `MP_getHero(1)`, and the commit below writes into it.
const PET_HERO: u32 = 1;
const PET_NAME: u32 = 0x62d;

/// `_mLetterState`. `onCloseIME_MainFrame` branches on it **before** it looks at
/// anything else, and 2 and 5 send the typed string to the mail box instead of
/// the pet. Neither can be live outside the main menu's letter screen, but the
/// check costs one byte read and the alternative is a rename that silently
/// overwrites a letter.
const LETTER_STATE_RECIPIENT: i8 = 2;
const LETTER_STATE_BODY: i8 = 5;

/// The pet-name commit inside `MainFrame_KeyPressed`, as offsets from its entry.
///
/// The whole block is reached from the `nameTitleIdx == 0x75` confirm popup
/// answered with OK, and it is the **only** place in the image that writes the
/// pet's name from the IME buffer:
///
/// ```text
/// +0x384  ldr r8, [pc, #0x2d8]        ; the popup's yes/no cursor
/// +0x39c  bl  MP_getHero              ; (1) — the pet
/// +0x3b0  bl  MX_imeEnd               ; releases the overlay
/// +0x3b8  add r0, sl, #0x620          ; \ pet + 0x62d
/// +0x3bc  add r0, r0, #0xd            ; /
/// +0x3c8  bl  MC_knlSprintk           ; name <- mainFrame + 0xf3
/// +0x3f0  b   MainFrame_KeyPressed+0xc8
/// ```
///
/// [`PET_COMMIT_END`] is the last of them because by then the name is written
/// *and* `nameTitleIdx` is back to -1 — nothing is left half done, so the hook
/// can save and let the `b` run.
const PET_COMMIT_CURSOR: u32 = 0x384;
const PET_COMMIT_GET_HERO: u32 = 0x39c;
const PET_COMMIT_IME_END: u32 = 0x3b0;
const PET_COMMIT_NAME_HI: u32 = 0x3b8;
const PET_COMMIT_NAME_LO: u32 = 0x3bc;
const PET_COMMIT_SPRINTK: u32 = 0x3c8;
const PET_COMMIT_END: u32 = 0x3f0;

/// Two more offsets in the same block, which pin *what* is being written where:
/// `mov r0, #1` is the argument to `MP_getHero` (the pet is hero 1) and
/// `add r5, r5, #0xf3` is the source, `mainFrame + 0xf3` — the buffer
/// `onCloseIME_MainFrame` left the typed name in.
const PET_COMMIT_HERO_ARG: u32 = 0x398;
const PET_COMMIT_SRC: u32 = 0x3c0;

/// The five non-branch words, checked at install. The two `add` at
/// [`PET_COMMIT_NAME_HI`]/`_LO` are what pin the field offset to 0x62d.
const PET_COMMIT_WORDS: [(u32, u32); 5] = [
    (PET_COMMIT_CURSOR, 0xe59f_82d8),
    (PET_COMMIT_HERO_ARG, 0xe3a0_0001),
    (PET_COMMIT_NAME_HI, 0xe28a_0e62),
    (PET_COMMIT_NAME_LO, 0xe280_000d),
    (PET_COMMIT_SRC, 0xe285_50f3),
];

/// Where the `b` at [`PET_COMMIT_END`] goes — the function's shared epilogue.
const PET_COMMIT_EXIT: u32 = 0xc8;

/// Decode the target of `b` (`link = false`) or `bl` (`link = true`) at `at`, or
/// `None` if the word there is not that branch.
fn branch_target(at: u32, mem: &mut GuestMem, link: bool) -> Option<u32> {
    let word = mem.r32(at);
    (word >> 24 == if link { 0xeb } else { 0xea })
        .then(|| at.wrapping_add(8).wrapping_add((((word << 8) as i32 >> 8) as u32) << 2))
}

/// Where to install [`Patch::PetRenameSave`], or `None` if this is not the
/// binary it was written against.
///
/// Nine words, four of them checked by meaning rather than encoding: the three
/// `bl` must still reach `MP_getHero`, `MX_imeEnd` and `MC_knlSprintk`, and the
/// tail `b` must still go to the epilogue. A mid-function hook on a block this
/// specific has no other way to notice it has drifted, and getting it wrong
/// would mean writing the pet file at an arbitrary moment.
pub fn pet_rename_site(
    base: u32,
    ime_end: u32,
    sprintk: u32,
    get_hero: u32,
    mem: &mut GuestMem,
) -> Option<u32> {
    let at = |off: u32| base.wrapping_add(off);
    if PET_COMMIT_WORDS.iter().any(|&(off, word)| mem.r32(at(off)) != word) {
        return None;
    }
    let calls = [
        (PET_COMMIT_GET_HERO, get_hero),
        (PET_COMMIT_IME_END, ime_end),
        (PET_COMMIT_SPRINTK, sprintk),
    ];
    if calls.iter().any(|&(off, to)| branch_target(at(off), mem, true) != Some(to)) {
        return None;
    }
    (branch_target(at(PET_COMMIT_END), mem, false) == Some(at(PET_COMMIT_EXIT)))
        .then(|| at(PET_COMMIT_END))
}

/// The eight places an item's level requirement (`item[7]`) is loaded to be
/// *enforced*.
///
/// Six are the near-identical `Check_*` gates; two are hand-inlined copies
/// inside `GameMenuFrame_KeyPressed` (`+0xa28c` equips a pet item, class 0x17;
/// `+0x2de0` equips class 0x1a from the bag, and this one is missing from the
/// analysis document — found by scanning `.text` for every byte load at
/// offset 7 rather than by following the six symbols).
///
/// The tooltip's own load (`drawItemInfo+0xc78`, the one `ldrsb` of the set) is
/// deliberately **not** here: what it prints is the requirement the player is
/// told about, and that is handled at the draw instead, so the original number
/// can be shown struck through beside the new one. See `host_item_info_level`.
///
/// Patching the single load rather than wrapping the function is what makes
/// this uniform: each site is one `ldrb Rd, [Rn, #7]`, the whole instruction is
/// reproduced by the host, and no item memory is ever written — which also
/// settles whether `item` is an instance or a shared template, since it no
/// longer matters. The expected instruction word is checked at install; a site
/// that does not match is dropped, and the registers are decoded from that same
/// word rather than kept in a second table.
const ITEM_LEVEL_SITES: [(&str, u32, u32); 8] = [
    ("Check_Hand", 0x28, 0xe5d5_6007),
    ("Check_LowerBody", 0x28, 0xe5d5_6007),
    ("Check_UpperBody", 0x28, 0xe5d5_6007),
    ("Check_Head", 0x28, 0xe5d5_6007),
    ("Check_LeftHand", 0x2c, 0xe5d4_7007),
    ("Check_RightHand", 0x2c, 0xe5d5_8007),
    ("GameMenuFrame_KeyPressed", 0x2de0, 0xe5d6_8007),
    ("GameMenuFrame_KeyPressed", 0xa28c, 0xe5d7_8007),
];

/// How many of them there should be, for the install-time warning.
pub const ITEM_LEVEL_SITE_COUNT: usize = ITEM_LEVEL_SITES.len();

/// Resolve [`ITEM_LEVEL_SITES`] against the loaded guest, decoding `Rd`/`Rn`
/// out of each verified instruction word.
pub fn item_level_sites(
    symbols: &std::collections::HashMap<String, u32>,
    mem: &mut GuestMem,
) -> Vec<(u32, Patch)> {
    if !crate::potions::enabled() || !crate::potions::level_enabled() {
        return Vec::new();
    }
    ITEM_LEVEL_SITES
        .iter()
        .filter_map(|&(sym, off, word)| {
            let at = symbols.get(sym)?.wrapping_add(off);
            if mem.r32(at) != word {
                log::warn!("CS_POTION_LEVEL: {sym}+{off:#x} is not the expected instruction");
                return None;
            }
            let patch =
                Patch::ItemLevel { rd: ((word >> 12) & 0xf) as u8, rn: ((word >> 16) & 0xf) as u8 };
            Some((at, patch))
        })
        .collect()
}

/// 네트워크 장비의 장착을 막는 일곱 지점 — `item[8]`(부호 있는 네트워크
/// 플래그, 일반 -1 / 네트워크 0..4)의 `ldrsb` 전부. 자세한 배경은
/// `netitem.rs` 의 모듈 주석.
///
/// 여섯 `Check_*` 는 요구레벨 검사(`ITEM_LEVEL_SITES`) 바로 다음 명령이고,
/// 일곱째는 목걸이(0x16)·반지(0x17) 장착 액션이 쓰는 손으로 인라인된
/// 사본이다 (`GameMenuFrame_KeyPressed+0x9f74`; 목걸이 레벨 검사 블록
/// `+0xa288` 과 반지 블록 `+0xa274` 가 둘 다 여기로 합류한다). 일곱 다
/// `cmp rX,#0` + `bge`(Check_* 는 -1 반환) / `blt`(KeyPressed 는 통과)가
/// 뒤따른다 — 플래그가 0 이상이면 "장착 할 수 없는 아이템입니다" 다.
///
/// `checkEqipItem`(가방 아이콘의 회색 처리)은 같은 `Check_*` 를 부르므로
/// 따로 손댈 것이 없고, 목걸이·반지는 슬롯 종류만 보므로 원래 회색이
/// 아니다.
///
/// 레벨 게이트와 같은 방식으로 명령 하나만 대신 실행한다: 아이템 메모리에
/// 아무것도 쓰지 않으므로 세이브에는 진짜 플래그가 남고, `CS_NET_EQUIP=0`
/// 이면 출고 동작 그대로다.
const NET_ITEM_SITES: [(&str, u32, u32); 7] = [
    ("Check_Hand", 0x4c, 0xe1d5_30d8),
    ("Check_LowerBody", 0x4c, 0xe1d5_30d8),
    ("Check_UpperBody", 0x4c, 0xe1d5_30d8),
    ("Check_Head", 0x4c, 0xe1d5_30d8),
    ("Check_LeftHand", 0x50, 0xe1d4_30d8),
    ("Check_RightHand", 0x50, 0xe1d5_30d8),
    ("GameMenuFrame_KeyPressed", 0x9f74, 0xe1d7_20d8),
];

/// How many of them there should be, for the install-time warning.
pub const NET_ITEM_SITE_COUNT: usize = NET_ITEM_SITES.len();

/// Resolve [`NET_ITEM_SITES`], decoding `Rd`/`Rn` from each verified word —
/// the same discipline as [`item_level_sites`].
pub fn net_item_sites(
    symbols: &std::collections::HashMap<String, u32>,
    mem: &mut GuestMem,
) -> Vec<(u32, Patch)> {
    if !net_equip() {
        return Vec::new();
    }
    NET_ITEM_SITES
        .iter()
        .filter_map(|&(sym, off, word)| {
            let at = symbols.get(sym)?.wrapping_add(off);
            if mem.r32(at) != word {
                log::warn!("CS_NET_EQUIP: {sym}+{off:#x} is not the expected instruction");
                return None;
            }
            let patch = Patch::NetItemFlag {
                rd: ((word >> 12) & 0xf) as u8,
                rn: ((word >> 16) & 0xf) as u8,
            };
            Some((at, patch))
        })
        .collect()
}

/// `CS_NET_EQUIP=0` puts the network-equipment equip block back.
pub fn net_equip() -> bool {
    knob("CS_NET_EQUIP", 1) != 0
}

/// `CS_NET_DROP=0` keeps monster drop tables exactly as the files say.
pub fn net_drop() -> bool {
    knob("CS_NET_DROP", 1) != 0
}

/// Injected entry's drop rate, in the game's own units: a u32 compared
/// against `UM_Rand(0, 0xffff)`, so 655 ≈ 1%. The hero's drop-rate bonus
/// (`hero+0x72c`) boosts it exactly as it boosts the shipped entries.
/// Shipped equipment entries range 327..32767. Clamped so a huge value
/// cannot push the later shipped slots' windows past the rand range.
pub fn net_drop_rate() -> i32 {
    knob("CS_NET_DROP_RATE", 655).clamp(0, 0xffff)
}

/// Where [`Patch::NetItemDrop`] hooks: `MONSTER_loadCsv`, gated to the
/// `r1 = 0` call in `MONSTER_dataLoad` — the first thing that runs after
/// `MONSTER_loadMonsterStatus` has parsed the decrypted record into the
/// monster, with `r0` still the monster. An entry hook that always declines,
/// so the guest's own `loadCsv` runs untouched.
pub const PATCHED_NET_DROP: [(&str, Patch); 1] = [("MONSTER_loadCsv", Patch::NetItemDrop)];

/// Return address of that first `bl MONSTER_loadCsv`, as an offset into
/// `MONSTER_dataLoad` (0xd0c90 − 0xd0bd4). The second call (`r1 = 1`,
/// returns to `+0xc8`) and any other caller fail the LR test.
pub const MON_LOADCSV_RET: u32 = 0xbc;

/// The monster's drop table, as `MONSTER_loadMonsterStatus+0x3b8` lays it
/// out: twelve slots of kind (`u16`, bit 15 set = empty), item index
/// (`u16`) and rate (`u32`). `MONSTER_searchDropItemWithRate` walks slots
/// 2..=10 as one cumulative window over `UM_Rand(0, 0xffff)`; slots 0 and 1
/// are `MONSTER_dropIndexItem`'s separately-rolled drops and slot 11 is
/// unreachable (the walk gives up at index 0xb), so only 2..=10 are touched.
const MON_DROP_KIND: u32 = 0x192;
const MON_DROP_IDX: u32 = 0x1aa;
const MON_DROP_RATE: u32 = 0x1c4;
const MON_DROP_POOL: std::ops::RangeInclusive<u32> = 2..=10;

/// Return address of the `bl HF_DrawTextOutlineH2` that draws
/// `getInCodeString(0x157)` = `r요구레벨 &%d`, as an offset into `drawItemInfo`.
///
/// `drawItemInfo` draws a dozen lines through the same renderer, so the site is
/// the whole guard — and it is also the recursion guard, since the re-issued
/// draw enters with `LR = RETURN_MAGIC`.
const ITEM_INFO_LEVEL_RET: u32 = 0xce0;

/// Where `HF_DrawTextOutlineH2`'s arguments sit, counted from the entry SP.
///
/// `HF_DrawTextOutLine` is a two-hop shim onto this: it reorders its registers
/// into `HF_drawTextWithOutLine(g, str, x, y, len, c1, c2, flags)`, which calls
/// `H2(g, x, y, 0x3e8, [0]=str, [4]=0, [8]=0, [0xc]=len, [0x10]=c1, [0x14]=c2,
/// [0x18]=flags, [0x1c]=1)`. The item panel calls H2 directly with the same
/// `c1 = -1, c2 = 0, flags = 0` the cash price line uses.
const H2_STR: u32 = 0x00;
const H2_LEN: u32 = 0x0c;
/// Only carried through — the whole block is copied to the re-issued call — but
/// named so the map above is complete and the tests can set it.
#[allow(dead_code)]
const H2_C1: u32 = 0x10;
const H2_C2: u32 = 0x14;
const H2_FLAGS: u32 = 0x18;
const H2_ARGS: u32 = 0x20;

/// Where the buff lines go: just right of the HP/MP block, which occupies
/// x 2..67 and y 3..24 (`Draw_PlayInfo`'s own `DM_FillRectRGB` calls).
///
/// The step is 14 and not the 10 `HF_getHeightEng` reports: that is the Latin
/// glyph height, while a Korean syllable is twelve rows and the outline adds
/// one above and one below. At 9 and again at 12 the next line's outline ate
/// the bottom stroke of 간 — 시간 came out as 시가 in the probe dump, which is
/// the sort of thing only a screenshot tells you.
const HUD_X: u32 = 0x48;
const HUD_Y: u32 = 2;
const HUD_STEP: u32 = 14;

/// `HF_DrawTextOutLine(g, x, y, s, [sp] len, [sp+4] c1, [sp+8] c2, [sp+0xc]
/// flags)`, with the colours the cash shop's own price line passes — outlined
/// text over a blurred background is exactly the case it was chosen for.
const HUD_C1: u32 = u32::MAX;
const HUD_C2: u32 = 0;
const HUD_FLAGS: u32 = 0;
/// One guest buffer serves every HUD line — buff, item panel, gamepad legend —
/// so it is sized for the longest of them (the pad legend's 78 bytes) plus the
/// NUL, rounded up.
const HUD_SCRATCH: usize = 96;

/// Return address of the one `bl GameFrame_drawGameUI` that draws the world's
/// UI, as an offset into `GameFrame_Render`. The other caller,
/// `MainMenuOptionKey+0x234`, is the options screen's preview and must not grow
/// a buff line.
const GAME_UI_RET: u32 = 0x4b0;

/// Return address of the one `bl IGM_DrawMenu`, as an offset into
/// `GameMenuFrame_Render`. Also the recursion guard, since `Vm::call` enters
/// with `LR = RETURN_MAGIC`.
const IGM_MENU_RET: u32 = 0x648;

/// The menu is where the buff lines were asked for, and it needs its own hook:
/// the HP/MP block visible there is a **blurred snapshot**
/// (`GameMenuFrame_Initialize+0x288` runs `DM_BoxBlurLCD565` once and
/// `GameMenuFrame_Render` re-blits it), so a line drawn in-game arrives in the
/// menu blurred into mush. `IGM_DrawMenu` draws over the snapshot instead.
///
/// `CS_POTION_HUD`: 0 off, 1 menu only, 2 (default) menu and in-game.
///
/// The gamepad legend shares the world-screen hook — one address, one patch —
/// so `GameFrame_drawGameUI` is claimed whenever either feature can want it.
/// `host_hud` sorts out per call which of the two actually draws; with no pad
/// attached and no buff running the hook declines and the guest runs its own
/// instruction, which is what keeps the off-state byte-identical.
pub fn hud_patches() -> Vec<(&'static str, Patch)> {
    let mode = if crate::potions::enabled() { crate::potions::hud_mode() } else { 0 };
    let mut v = Vec::new();
    if mode != 0 {
        v.push(("IGM_DrawMenu", Patch::Hud));
    }
    if !matches!(mode, 0 | 1) || pad_hud() {
        v.push(("GameFrame_drawGameUI", Patch::Hud));
    }
    v
}

/// `CS_PAD_HUD=0` keeps the gamepad legend off even with a pad attached.
///
/// This knob is only half the switch: the other half is `Vm::gamepad_hud`,
/// which the Android shell sets while `InputDevice` reports a gamepad or
/// joystick source and clears the moment it stops (`cs_android::pad`), and
/// which the probe sets with `--pad-hud`. Both must be true for a single line
/// to be drawn, so an APK baked with the default changes nothing on a phone
/// that never sees a pad.
pub fn pad_hud() -> bool {
    knob("CS_PAD_HUD", 1) != 0
}

// ---- 패드 배지: 어느 패드 버튼이 어느 스킬 슬롯인가 ------------------------
//
// 처음 판은 화면 하단에 **설명 두 줄**을 깔았다. 제보로 죽었다 — 「게임화면에 각
// 패드버튼별 설명같은거 넣으면안돼. 그냥 스킬 버튼별로 어떤 패드버튼이 어떤
// 슬롯인지 표시만 해주면돼.」 자매 프로젝트(hero5)가 같은 결론에 먼저 도달해
// 있었고, 여기 그 방식을 옮긴다: **버튼 자기 자리에 작은 배지**.
//
// ## 어디에 그리는가 — 게스트가 이미 알려 준다
//
// 화면의 스킬 버튼 여섯 칸은 `GameFrame_PressTouchKeypad+0x5b8` 의 여섯 바퀴
// 루프가 히트테스트하고, 그 루프가 쓰는 좌표는 함수 프롤로그가 `.rodata` 에서
// 스택으로 복사해 오는 **6칸 표 세 개**다:
//
// ```text
//  0x1050ac  dx  [-159, -120, -88, -61, -41, -32]   화면 폭에서 뺀다
//  0x1050c4  dy  [ -32,  -43, -63, -95,-128,-166]   화면 높이에서 뺀다
//  0x1050dc  key [  49,   51,  55,  57,  42,  48]   = '1' '3' '7' '9' '*' '0'
// ```
//
// 히트 사각형은 `Check_Rect(x, y, W + dx - 6, H + dy - 6, 0x20, 0x20)` 이다 —
// 32x32, 앵커에서 6px 좌상단. 배지는 **그 사각형**에 놓는다. 즉 보이는 곳과
// 눌리는 곳이 같은 자리라는 것이 좌표의 출처 자체로 보장된다.
//
// 위 세 주소는 **읽는 사람을 위한 것**이고 코드에는 없다. 표에는 심볼이 없지만
// 절대 주소를 적어 둘 필요도 없다 — 함수 프롤로그의 두 `add` 가 블록 시작점을
// 만들고([`Vm::pad_slot_tables`]), 표는 그 시작점 기준 오프셋에서 **읽는다**.
//
// **세 번째 표가 앞의 둘을 증명한다.** 키 표가 정확히 게임이 스킬 슬롯을 부르는
// 그 여섯 숫자(`1 3 7 9 * 0`)라서, 우리가 옳은 표를 잡았는지가 값 자체로 판별된다
// — 그래서 설치 검증이 그 여섯 워드다.
//
// ## 무엇을 그리는가 — 글자가 아니라 **자리**
//
// 안드로이드는 face 버튼을 **위치로** 보고한다. 커널의 `BTN_SOUTH` 가 키레이아웃을
// 지나 `KEYCODE_BUTTON_A`(96) 가 되므로 96은 어느 패드에서나 「아래쪽 face」다 —
// Xbox 에는 `A` 라고 찍혀 있고, 스위치 패드에는 `B`(닌텐도는 좌우가 뒤집혔다),
// 플레이스테이션에는 글자가 아예 없다. 그러니 `A` 라고 쓴 배지는 세 집안 중 둘에서
// 틀린다. 그래서 어깨 버튼은 **좌/우 x 위/아래의 2x2**, 스틱 누르기는 **좌/우 두
// 원**으로 그리고, 묶인 쪽만 밝게 한다.
//
// **안 묶인 나머지도 보이게 그린다.** 안 그리면 「오른쪽 아래에 뭔가」로 읽히고,
// 그려야 「넷 중 오른쪽 아래」로 읽힌다.

/// `GameFrame_PressTouchKeypad` 프롤로그가 스택으로 복사하는 그 표들, 함수 자신의
/// 블록 시작점에서 재서. **절대 주소를 적어 두지 않는다** — 게스트 이미지는
/// 리베이스되고, 이 저장소의 규칙도 「표는 옮겨 적지 말고 게스트에서 읽는다」다.
/// 블록 시작점은 프롤로그의 두 `add` 가 만드는 값이고([`Vm::pad_slot_tables`]),
/// 우리가 옳은 곳을 잡았는지는 [`PAD_SLOT_KEYS`] 가 값으로 판별한다.
const PAD_DX_OFF: u32 = 0x25c;
const PAD_DY_OFF: u32 = 0x274;
const PAD_KEY_OFF: u32 = 0x28c;

/// 그 블록 시작점을 만드는 프롤로그는 네 명령이다 —
/// `ldr r4,[pc,#0x960]` / `ldr lr,[pc,#0x958]` / `add r4,pc,r4` / `add lr,r4,lr`.
/// 인코딩으로 확인하는 것은 **두 `add`** 이고(아래), 두 `ldr` 은 그 리터럴을
/// 어디서 읽는지로만 쓴다. 리터럴 자체는 못 검증하는데 그럴 필요도 없다 —
/// 틀린 값이면 [`PAD_SLOT_KEYS`] 여섯 워드가 안 맞아 그 자리에서 걸린다.
const PAD_TBL_LIT1: u32 = 0x96c; // `add r4, pc, r4` 가 쓰는 리터럴
const PAD_TBL_LIT2: u32 = 0x970; // `add lr, r4, lr` 가 쓰는 리터럴
const PAD_TBL_WORDS: [(u32, u32); 2] = [
    (0x18, 0xe08f_4004), // add r4, pc, r4
    (0x20, 0xe084_e00e), // add lr, r4, lr
];

/// 게임이 스킬 슬롯을 부르는 이름 — 피처폰 키패드의 그 숫자들. 이 여섯 워드가
/// [`PAD_KEY_OFF`] 표에 그대로 있어야 위의 두 좌표 표도 우리가 생각하는 그것이다.
const PAD_SLOT_KEYS: [i32; 6] =
    [b'1' as i32, b'3' as i32, b'7' as i32, b'9' as i32, b'*' as i32, b'0' as i32];

/// 히트 사각형 — `Check_Rect(..., W + dx - 6, H + dy - 6, 0x20, 0x20)`.
const PAD_SLOT_BACK: i32 = 6;
const PAD_SLOT_SIZE: i32 = 0x20;

/// 배지 반지름. 32px 칸 안에서 버튼을 가리지 않고 자리를 읽을 수 있는 최소치다.
const PAD_BADGE_R: i32 = 7;

/// RGB565. 어두운 원판 + 강철빛 테두리, 묶인 쪽은 밝은 호박색, 나머지는 흐린 청회색.
const PAD_BADGE_FILL: u16 = 0x0861; // 거의 검은 슬레이트
const PAD_BADGE_RIM: u16 = 0x9DBA; // 강철
const PAD_BADGE_LIT: u16 = 0xFD90; // 호박
const PAD_BADGE_UNLIT: u16 = 0x6331; // 흐린 청회색

// 게스트 키 ↔ 패드 자리(`pos_for_key`/[`PadPos`])는 [`crate::pad::SLOTS`] 하나에서
// 온다 — 안드로이드 키코드를 게스트 키로 바꾸는 `cs_android::wipi_key` 도 같은
// 표를 읽는다. 여기 두 번째 표를 두면 언젠가 갈리고, 갈리면 배지가 다른 일을
// 하는 버튼을 가리킨다.

/// 데이터 관리's popups are the only ones in the game that actually say
/// "(소량의 통화료 부과)" — the cash shop's own confirm says nothing about money.
/// The binary ships a complete, unreferenced duplicate of that eleven-string
/// block at `id + 0xb`, word for word identical except that the parenthetical
/// reads "(정보료/통화료 무료)". Six of the eleven are literally the same pointer;
/// the five that differ, differ only in that trailing phrase — checked string by
/// string. So the whole fee wording comes off by remapping one id range, with no
/// host-allocated string and no EUC-KR re-encoding.
const FEE_TEXT_IDS: std::ops::RangeInclusive<u32> = 0x217..=0x221;
const FEE_TEXT_FREE: u32 = 0xb;

/// Return address of the one `bl NpcUI_merchantSelectKeyPressed`, as an offset
/// into `NpcUI_merchantKeyPressed`. The function has exactly one caller.
const MERCHANT_SELECT_RET: u32 = 0x5e0;

/// Return address of the cash-shop price `HF_DrawTextOutLine` call, as an offset
/// into `NpcUI_DrawMerChant_Info`. The enclosing branch is `merchantState == 3`,
/// so matching on it is already cash-only: gold prices reach
/// `HF_DrawTextOutLine` from a different site and are left alone.
const CASH_PRICE_RET: u32 = 0x664;

/// Return address of the orb-removal confirm's price line, as an offset into
/// `NpcUI_drawOrbRemovePayment`. That function draws exactly one paid sentence
/// and has no other `HF_DrawTextOutLine` carrying a number, so the site is the
/// whole guard.
const ORB_PRICE_RET: u32 = 0x1fc;

/// Where the strike sits inside the text's glyph box. `HF_getHeightEng` reports
/// 10 for the font both price lines use, so mid-glyph is 5 and 4 sits a pixel
/// above it — still inside the digits. `CS_STRIKE_Y` overrides it, because
/// whether the game's `y` is the top of the box or its baseline is only settled
/// by looking at the screen.
fn strike_y() -> i32 {
    static Y: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    *Y.get_or_init(|| {
        // Build time as well as run time: the device has no environment to set,
        // the same reason `CS_SERVER_HOST` is read both ways.
        std::env::var("CS_STRIKE_Y")
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| option_env!("CS_STRIKE_Y").and_then(|v| v.parse().ok()))
            .unwrap_or(4)
    })
}

/// Size of the guest buffer the price patches build their strings in. The orb
/// confirm is a whole two-line sentence, not just `CASH &%d`.
const CASH_SCRATCH: usize = 160;

/// Whether the SKIP button is drawn is decided per event-script *opcode*, not
/// per scene: `EVENT_checkCanSkip` reads the running opcode from `_event+0x32c`
/// and returns `table[op - 0x15]` from a 0x36-byte, unnamed `.rodata` table at
/// 0x104d60. Nine opcodes are marked 0 — 0x15, 0x25, 0x29, 0x2a, 0x3a, 0x3d,
/// 0x45, 0x48, 0x4a — and `Event_Key` turns the *input* away for exactly the
/// same nine, so the button and its hit box always agree.
///
/// The ordinary dialogue box is 0x26 (table = 1, SKIP shows). The full-screen
/// narration is 0x25 and the screen fade is 0x3d, both 0 — which is precisely
/// the reported "SKIP disappears mid-cutscene". Both are already skip-aware:
/// `EVENT_TickMovie` force-completes the narration and talk boxes when
/// `_event+0x334` is set, and `Event_Set_Screen` jumps straight to
/// `Event_ScreenEnd`.
///
/// The other seven stay shut, for two different reasons. Six — pet name entry,
/// the quest popup, hard-mode select and the three that open an NPC UI — are
/// interactive screens, and each calls `Event_Init_Skip` inside its own
/// handler: the game cancels a running skip there on purpose, so SKIP would be
/// a button that does nothing. The seventh, 0x29 (the ~1.6 s timed message),
/// looks tempting and is a trap: it holds all input off through `_event+0x329`,
/// `EVENT_TickMovie+0x18c` re-arms that byte *every tick* while the opcode is
/// live, and once `Event_skipFunction` has zeroed `+0x953` every path that
/// clears it is unreachable short of `EVT_Initialize`. Forcing a skip there
/// buys a second and a half and can leave the next quest popup undismissable.
const EXTRA_SKIPPABLE: [u32; 2] = [0x25, 0x3d];

/// The one `_event` field the host reads. Everything else in the gate
/// (`+0x328` in-event, `+0x329` message lock, `+0x32a` message box, `+0x690`
/// popup, `+0x975` event wait, `+0x334` already skipping) is left to the guest,
/// which re-checks all of it after this patch answers.
const EV_OPCODE: u32 = 0x32c;

/// Return address of the one `bl EVENT_checkCanSkip`, as an offset into
/// `Event_Draw_Event`. The function has exactly one caller, and pinning to it
/// is also what makes re-entry impossible.
const EVENT_CAN_SKIP_RET: u32 = 0x108;

/// The two points inside `Event_Key` where an extra-skippable opcode is turned
/// away, and where the shipped code would have gone for an ordinary one.
/// Redirecting the branch rather than reimplementing the handler leaves the hit
/// test — including `Check_Rect`'s vibration — and the skip itself as guest
/// code, and it inherits the guest's own earlier gates for free: `PopopBox_key`
/// and the `+0x334` "already skipping" test both run before either site.
///
/// * `+0x88` is the key chain's first `cmp r3,#0x3d`; `+0xd0` is the inlined
///   `Event_skipFunction` the default opcode reaches.
/// * `+0x3b8` is the same compare on the touch chain; `+0x400` is
///   `Check_Rect(W-0x32, H-0x10, 0x32, 0x10)` — the SKIP button — followed by
///   `bl Event_skipFunction` and the shared tail. A tap that misses the button
///   falls through to that tail exactly as before, so tapping elsewhere still
///   advances the narration text.
const EVK_KEY_CHAIN: u32 = 0x88;
const EVK_KEY_SKIP: u32 = 0xd0;
const EVK_TOUCH_CHAIN: u32 = 0x3b8;
const EVK_TOUCH_SKIP: u32 = 0x400;

/// The instruction at each hook, checked at install time. Same reasoning as
/// [`BAG_RIGHT_TEST_WORD`]: a mid-function hook has no other way to notice it
/// is pointing at a different build's code.
const EVK_SITES: [(u32, u32); 2] = [
    (EVK_KEY_CHAIN, 0xe353_003d), // cmp r3, #0x3d
    (EVK_TOUCH_CHAIN, 0xe353_003d),
];

/// `CS_ALWAYS_SKIP=0` restores the shipped per-opcode table exactly.
pub fn always_skip() -> bool {
    knob("CS_ALWAYS_SKIP", 1) != 0
}

/// The `Event_Key` addresses to install [`Patch::EventSkipKey`] at, or empty if
/// the feature is off or the guest is not this build.
pub fn skip_key_sites(base: u32, mem: &mut GuestMem) -> Vec<u32> {
    if !always_skip() || EVK_SITES.iter().any(|&(o, w)| mem.r32(base.wrapping_add(o)) != w) {
        return Vec::new();
    }
    EVK_SITES.iter().map(|&(o, _)| base.wrapping_add(o)).collect()
}

/// Hooked at symbol entry when the SKIP opening is on.
pub const PATCHED_ALWAYS_SKIP: [(&str, Patch); 1] = [("EVENT_checkCanSkip", Patch::EventCanSkip)];

/// Fields of the hero struct (`MP_getHero(i)` = `heroes + 0xb90 * i`) that the
/// cast-abort fix reads or writes. The casting state is two parallel records —
/// the *active* cast `{0xc3 skill, 0x296 uiIndex, 0x29a slot, 0x29b page,
/// 0x28c ticks}` and the *queued* one `{0xc4, 0x297, 0x298, 0x299, 0x290}` —
/// and every teardown in the binary resets a whole record at once
/// (`HERO_init+0x78`, `HERO_End_Process+0xb00` and `+0x3ac`,
/// `HERO_Key_Press_Shortcut_Slot+0x454`).
const HERO_STATE: u32 = 0xc0; // 9 == casting
const HERO_ACT_SKILL: u32 = 0xc3;
const HERO_CHAIN_IDX: u32 = 0x294;
const HERO_CHAIN_PENDING: u32 = 0x67f;
/// The install mutex: set while a chain skill occupies a shortcut slot and
/// `_copyslot` holds what it displaced.
const HERO_CHAIN_HELD: u32 = 0x280;
const HERO_ACT_SLOT: u32 = 0x29a;
const HERO_QUE_SLOT: u32 = 0x298;
const HERO_QUE_TICKS: u32 = 0x290;
const HERO_ACT_TICKS: u32 = 0x28c;
const HERO_CASTING: u8 = 9;

/// `HERO_draw_casting_guage`'s file-static has no symbol, so its address is
/// recovered the way the function itself does: `add r4, pc, [f+0x26c]` at
/// `f+0x18` gives the GOT, and `[f+0x274]` is the GOTOFF of the block.
/// Fields: `+0 total`, `+4 initialised`, `+8 name[0x64]`, `+0x6c elapsed`.
const GAUGE_GOT_LIT: u32 = 0x26c;
const GAUGE_OFF_LIT: u32 = 0x274;
const GAUGE_TOTAL: u32 = 0;
const GAUGE_READY: u32 = 4;
const GAUGE_ELAPSED: u32 = 0x6c;

/// `CS_CAST_FIX=0` leaves the shipped cast-abort exactly as it is.
pub fn cast_fix() -> bool {
    knob("CS_CAST_FIX", 1) != 0
}

/// The in-game touch control scheme, `_menuOpt+0x14`.
///
/// `Draw_TouchPad` reads it three ways: **0 is the floating joystick** that
/// re-anchors to wherever the finger lands (`mainFrame+0x80/0x84`, arms ±0x28,
/// drawn only while a finger is down), non-zero is a pad bolted to
/// x = 0x35/0x6b/0x35/0x0a, and 2 additionally swaps the quick-button table.
/// Only four sites anywhere distinguish 0 from 1 — `Draw_TouchPad+0x3e8`
/// (0x5c13c) and three in `onHandleEvent` (0x3d1d8/0x3d2a8/0x3d430); the two in
/// `GameFrame_PressTouchKeypad` (0x603b0/0x607dc), like ~24 others, test for 2.
///
/// **This byte is the 터치방식 row of 환경설정** — row 3, drawn as `TYPE A`
/// (0x10aa60) / `TYPE B` (0x10aa68) / `가로` (0x10aa70) by
/// `MainMenuFrame_drawOption+0x5d8` (`ldrsb r3, [r3, #0x14]`, the label table
/// index). `MainMenuOptionKey` writes it inline and clamps to `{0, 1}` —
/// 0xab824/0xab82c for LEFT, 0xab9f8/0xaba04 for RIGHT — and every one of those
/// four branches falls straight into `bl GameFrame_saveOptionData` (0xab830,
/// 0xaba08, and 0xab05c for the 0→1 case, which arrives by `ble`). So the guest
/// round trip is complete: `GameFrame_saveOptionData+0x7c` (0x5f19c) writes it
/// to `OPTIONDATA` byte 9 and `GameFrame_loadOptionData+0xf4` (0x5f0d0) reads
/// it back. `GameFrame_initOptionData+0x44` (0x5970c) writes 0 for a fresh
/// profile.
///
/// An earlier version of this patch forced the byte to `CS_PAD_MODE` (then
/// defaulting to 0) after every load, on the premise that the game could not
/// change it — true only of `HERO_setKeyType` (0x8a41c), which does have zero
/// callers, but the settings screen never goes through it. That threw the
/// player's choice away on every cold boot **and** on every in-game → main menu
/// transition (the loader's one call site, `MainMenuFrame_Initialize+0x244`, is
/// reached from `LogoFrame_endProcess+0x13c` and `GameMenuFrame_goToMainMenu`),
/// and because the write lands in `_menuOpt` itself the next options save
/// pushed the 0 to disk.
///
/// What is left is a migration, not an override. `{0, 1}` is whatever the
/// player picked and is never touched. A value **> 1** cannot be produced by
/// the settings UI at all (both key paths clamp), so it can only come from an
/// old `CS_PAD_MODE=1/2` build of this port or a save carried in from
/// elsewhere, and it pins the fixed pad with no way back inside the game — that
/// one gets normalised to 0. The predicate is on the value alone, so it is
/// idempotent and needs no "first boot" marker.
///
/// `CS_PAD_MODE`: negative (**default**) follows the save, 0/1/2 forces that
/// value on every load — kept for debugging and repro, not for shipping.
/// `CS_PAD_MIGRATE=0` turns off the out-of-range normalisation too.
const MENU_OPT_PAD: u32 = 0x14;

pub fn pad_mode() -> i32 {
    knob("CS_PAD_MODE", -1)
}

pub fn pad_migrate() -> bool {
    knob("CS_PAD_MIGRATE", 1) != 0
}

/// What, if anything, the host should write over the byte the loader just
/// stored. `None` leaves the player's choice exactly as the save had it.
///
/// Split out from [`Vm::host_pad_mode`] because the whole correctness of this
/// patch is in these three lines: the shipped default must be "write nothing"
/// for every value the settings screen can produce, or the setting resets.
fn pad_target(was: u8, want: i32, migrate: bool) -> Option<u8> {
    match () {
        _ if want >= 0 => Some(want as u8),
        // Only {0, 1} are reachable in game — both `MainMenuOptionKey` paths
        // clamp — so anything above is a value no player can undo from inside.
        _ if migrate && was > 1 => Some(0),
        _ => None,
    }
}

/// Installed whenever the pad byte is being forced or migrated at all.
pub const PATCHED_PAD: [(&str, Patch); 1] = [("GameFrame_loadOptionData", Patch::PadMode)];

/// `CS_PHONE` — the identity this install reports, or `0` to report nothing.
///
/// Nothing is what the host used to do, and it is what every build before
/// 2026-08-16 shipped, so this is also the A/B knob: `CS_PHONE=0` restores the
/// old behaviour exactly (the hook is not installed at all).
pub fn phone_identity() -> bool {
    knob("CS_PHONE", 1) != 0
}

/// Report the **claimed ID** as `PHONENUMBER` instead of the minted number.
///
/// Off by default, and the default is load-bearing rather than caution:
///
/// * A non-numeric identity on the wire is only safe once the *server* handles
///   one, and the server is deployed separately. The order — server first,
///   client second — cannot be undone in the field, because letters delivered
///   to the wrong key are not recoverable (`LETTER_ID.md` R2).
/// * Flipping it moves this install's mailbox and save slot to a new key. That
///   is the point, but it is a migration, not a setting, and the old address
///   keeps working (`cs_server` aliases it) only when the claim was a real
///   move.
///
/// With no claimed ID this changes nothing at all — [`identity::Identity::wire`]
/// answers the number, which is what the hook already sent.
pub fn id_as_phone() -> bool {
    knob("CS_ID_AS_PHONE", 0) != 0
}

/// `CS_HOST_SLEEP` — serve `UM_Sleep`'s busy-wait with a real host sleep
/// (default on). `0` leaves the spin to the interpreter, which is the pre-2026-08-18
/// behaviour and the A/B for the heat this exists to remove.
pub fn host_sleep() -> bool {
    knob("CS_HOST_SLEEP", 1) != 0
}

/// `CS_LAZY_GL` — on frames where no texture upload happened, skip the
/// full-surface upscale, the present copy and the present itself (default on).
/// The game makes a new picture ~15–21 times a second while the host loop runs
/// at 60; the other two thirds of the display work moves identical pixels.
/// `0` restores the shipped 60 Hz path, which is the A/B knob.
pub fn lazy_gl() -> bool {
    knob("CS_LAZY_GL", 1) != 0
}

/// `getSKTPhoneNumber` — the entry, and the two words that pin it.
///
/// `push {r4, r5, r6, r7, r8, sb, sl, lr}` then `sub sp, sp, #0x60`. The
/// contract is read off its own tail:
///
/// ```text
///   0xd1b8c  mov sl, r0          ; r0 = where the string goes
///   0xd1c98  mov r0, sl
///   0xd1ca8  bl MC_knlSprintk    ; ...written here
///   0xd1cac  str r4, [sb]        ; sb = r1 = where the length goes
/// ```
const SKT_PHONE_WORD: u32 = 0xe92d_47f0;
const SKT_PHONE_SUB_WORD: u32 = 0xe24d_d060;

/// `MC_knlGetSystemProperty` — hooked only to answer `"PHONENUMBER"`.
///
/// This is the lowest useful seam. Everything that wants an identity goes
/// through here — `getSKTPhoneNumber` (the handshake and billing) and
/// `_midas_funcDspCrossRoadMakePacket` (the 데이터 관리 save-slot key) — so one
/// answer covers all of them, and every consumer keeps running the shipped code
/// that normalises the digits, prepends the leading `'0'`, formats with
/// `MC_knlSprintk` and computes the length. Hooking `getSKTPhoneNumber` instead
/// would mean reproducing all of that.
pub const PATCHED_PHONE: [(&str, Patch); 1] = [("MC_knlGetSystemProperty", Patch::PhoneNumber)];

/// `CS_DATA_MANAGE` — let 환경설정 → 데이터 관리 do what it shipped to do.
///
/// Off by default. That menu is a **one-way save-destroying button** in this
/// build: a successful upload ends with the client deleting the local save
/// (`MC_fsRemove`, the original design), and the download half is unreachable —
/// `DSP_NTCON`'s only reachable call site passes 0 (upload) because of the
/// `cmp r5,#0 / bne` above it. It cost a real save on 2026-08-16.
///
/// The server refuses the upload as well. Two independent stops, because the
/// failure is silent and the loss is total.
pub fn data_manage() -> bool {
    knob("CS_DATA_MANAGE", 0) != 0
}

/// The two instructions that mean "the player picked 데이터 관리".
///
/// `MainMenuOptionKey` chooses a screen by **return value**, and `0x64` is this
/// one. Exactly two instructions in the function produce it — the row-9 arm of
/// the jump table (`+0x544`) and the second selection path (`+0xcf4`) — and both
/// fall straight into the epilogue. So the hook is "don't produce 0x64", which
/// is narrower than touching the menu, the cursor or the row count.
///
/// Word: `mov r0, #0x64`. `only_two_instructions_choose_data_manage` sweeps the
/// whole function for that word and pins this list — it caught these offsets
/// being written down wrong the first time.
pub const DATA_MANAGE_SITES: [(u32, u32); 2] = [(0x544, 0xe3a0_0064), (0xcf4, 0xe3a0_0064)];

/// The message, EUC-KR — the encoding every guest string is in. A host `String`
/// here would reach the font as mojibake.
///
/// `setPopUp` ends in `MC_knlSprintk(state+0x8f, text)`, so this is a *format*
/// string. It contains no `%`, which is the only reason passing it straight
/// through is safe.
pub const DATA_MANAGE_MSG: &[u8] = &[
    0xc0, 0xcc, 0xbf, 0xeb, 0xc7, 0xd2, // 이용할
    0x20, 0xbc, 0xf6, // 수
    0x20, 0xbe, 0xf8, 0xb4, 0xc2, // 없는
    0x20, 0xb1, 0xe2, 0xb4, 0xc9, 0xc0, 0xd4, 0xb4, 0xcf, 0xb4, 0xd9, // 기능입니다
];

/// `CS_SAVEXFER` — the three save rows at the bottom of 환경설정 (세이브
/// 내보내기 / 불러오기 / 되돌리기), the way the sister port has them. On by
/// default. The rows are the code cave's (`cave/src/opt.c`, feature
/// `savexfer`, which also rides this knob); the files, the vault and the
/// phone's document screens are `savebundle.rs` / `savexfer.rs`.
pub fn savexfer() -> bool {
    knob("CS_SAVEXFER", 1) != 0
}

/// `CS_SMS_POPUP` — show the SMS 수신동의 screen the game opens with.
///
/// Off by default. It asks permission to send marketing SMS through a carrier
/// service that no longer exists, so there is nothing behind either answer, and
/// it stands between the player and the title screen on every fresh install.
///
/// The seam is `GameFrame_loadAgreeData`, which answers "is there an AGREE
/// record". All three callers — `LogoFrame_KeyPressed+0x7c` and
/// `LogoFrame_Initialize+0x90/+0xb0` — do the same thing with it:
/// non-zero puts `_logoState` at 2 (carry on), zero puts it at 4 (ask). So
/// reporting "already answered" is exactly the state a returning player is in,
/// and no new path is invented.
///
/// Nothing is written: the guest's own `GameFrame_saveAgreeData` still owns the
/// record, and a save that already has one is unaffected either way.
pub fn sms_popup() -> bool {
    knob("CS_SMS_POPUP", 0) != 0
}

pub const PATCHED_SMS: [(&str, Patch); 1] = [("GameFrame_loadAgreeData", Patch::SmsAgreed)];

/// The property name we answer. The guest compares against nine names
/// (`PHONENUMBER`, `MANUFACTURER`, `PHONEMODEL`, `IMEI`, `IMSI`,
/// `AIRPLANE_MODE`, `ROAMING_AREA`, `DS_LOCK`, `PLATFORMVERSION`); the other
/// eight are left to it.
pub const PHONE_PROPERTY: &[u8] = b"PHONENUMBER";

/// `CS_OPT_DEBUG=1` logs every options save with the values it is about to
/// write and the return address that asked for it. There are twelve call sites
/// and all of them are user-driven, so this is a handful of lines per session.
pub fn opt_debug() -> bool {
    knob("CS_OPT_DEBUG", 0) != 0
}

pub const PATCHED_OPT_LOG: [(&str, Patch); 1] = [("GameFrame_saveOptionData", Patch::OptSaveLog)];

/// Installed only when the cast-abort fix is on. Both change guest state, so
/// they do not ride the same switch as the perf patches.
pub const PATCHED_CAST: [(&str, Patch); 2] =
    [("HERO_Key_DirectKeyPress", Patch::CastCancel), ("HERO_draw_casting_guage", Patch::CastGauge)];

/// The cutscene skip's three-byte state machine, all inside `_event`.
///
/// `Event_skipFunction` (and the copy inlined at `Event_Key+0xd0`) arms a skip
/// by writing `+0x335 = 1`, `+0x337 = 5`, `+0x336 = 0` and `+0x334 = 1`. From
/// then until `+0x336` reaches `+0x337`, **`EVENT_Tick` does nothing at all**:
///
/// ```text
///   EVENT_Tick+0x28   ldrb r3, [r4, #0x334]   ; skipping?
///                     cmp  r3, #0
///                     beq  +0x60              ; no  -> EVENT_TickMovie
///   EVENT_Tick+0x34   ldrb r3, [r4, #0x337]   ; total
///                     ldrb r2, [r4, #0x336]   ; progress
///                     ...
///                     beq  +0x8c              ; progress == total-1 -> reset script
///                     cmp  r2, r3
///                     beq  +0x60              ; progress == total   -> EVENT_TickMovie
///                     pop                     ; otherwise: return, do nothing
/// ```
///
/// And the only thing in the whole binary that moves `+0x336` is
/// `Event_Draw_Skip_Effect+0x30`, whose only caller is `GameFrame_Render+0x340`.
///
/// **So the cutscene's logic is driven by its renderer**, and the two do not
/// agree on when to run. `GameFrame_Process+0x14` calls `EVENT_Tick` on one
/// condition — `_event[0x328]`, "an event is running" — while the path to
/// `Event_Draw_Skip_Effect` is behind three more gates: `_map[0x178]` with
/// `*_isShowUI` (`GameFrame_Render+0x2c4`, which exits the whole event draw),
/// the earlier `*_isShowUI` test at `+0x27c`, and the `_map[0x114] == 0x23`
/// branch at `+0x29c`. Whenever one of those is taken while a skip is armed,
/// progress can never advance, `EVENT_Tick` returns immediately on every frame
/// for the rest of the process, and the cutscene is **frozen** — showing
/// whatever was last drawn, which after the fade that follows a boss fight is a
/// black screen. Watching the dialogue instead of skipping takes the
/// `beq +0x60` above and is unaffected, exactly as reported.
///
/// This is the shipped game's own structure, not something the port
/// introduced; what the port did was widen the window in which a skip can be
/// armed, since [`EXTRA_SKIPPABLE`] adds the narration and the screen fade —
/// which is precisely what plays when a boss dies.
///
/// The watchdog does what the missing draw would have done, and only then: it
/// waits for [`SKIP_STALL_TICKS`] consecutive ticks in which a skip is armed
/// and progress has not moved, then advances it by one, clamped to the total,
/// which is `Event_Draw_Skip_Effect+0x30..0x40` verbatim. While the renderer is
/// running it bumps progress every frame, so the stall counter never gets near
/// the threshold and the watchdog is inert.
const EV_SKIPPING: u32 = 0x334;
const EV_SKIP_PROGRESS: u32 = 0x336;
const EV_SKIP_TOTAL: u32 = 0x337;

/// Ticks of no progress before the watchdog decides the renderer is not coming.
///
/// `EVENT_Tick` runs once per frame from `GameFrame_Process`, so this is about
/// a second — long enough that a frame or two of missed drawing (a resize, a
/// surface being recreated) is never mistaken for the deadlock, short enough
/// that a player who hit SKIP is not left staring at a black screen.
const SKIP_STALL_TICKS: u32 = 30;

/// `CS_SKIP_WATCHDOG=0` leaves a wedged skip wedged, which is what the original
/// binary does.
pub fn skip_watchdog() -> bool {
    knob("CS_SKIP_WATCHDOG", 1) != 0
}

/// Installed unconditionally when the watchdog is on: it guards the shipped
/// game's own deadlock, so it is not tied to `CS_ALWAYS_SKIP`.
pub const PATCHED_SKIP_WATCHDOG: [(&str, Patch); 1] = [("EVENT_Tick", Patch::SkipWatchdog)];

/// An item's "am I equipped" byte. `ITEM_setEquipSlotNum` writes 1 here and
/// `ITEM_releaseEquipSlotNum` writes -1; `ITEM_isEquipSetSlot` is `item[0x98]
/// >= 0`. The whole UI agrees — `drawItemBag+0x450`, `IGM_DrawEquip+0x1164`
/// and `ItemBag_checkAndSetItemSortSlot+0x124` all read this byte with `ldrsb`
/// to tell an equipped item from a spare — and `Item_saveStorage+0x1ec` writes
/// it into byte 6 of the item's save record, which `Item_loadStorage+0x294`
/// reads straight back. So it survives a save/load round trip and it is the
/// game's own answer to the question this patch asks.
const ITEM_EQUIP_FLAG: u32 = 0x98;

/// `bl ITEM_setEquip` inside `Item_loadStorage` — the call that rebuilds the
/// eight equip pointers after a load.
///
/// The loader clears all eight slots first (`+0x8c`, item = NULL), then for
/// every item it reads out of the save it walks the eight saved stamps and
/// equips the item into any slot whose stamp matches `item+0xd0`. The match is
/// on that 64-bit stamp *alone*, and `ITEM_setEquip` overwrites, so when two
/// items in one save carry the same stamp the later one in bag order takes the
/// slot. See [`crate::boot_clock_ms`] for why two items ever came to share one.
///
/// There are three `bl ITEM_setEquip` in this function; this is the middle one,
/// the only one driven by the stamp match. `+0xa0` is the clear and `+0x408` is
/// the pet's, and neither is pinned to.
const EQUIP_RESTORE_CALL: u32 = 0x3b0;

/// `CS_EQUIP_FIX=0` restores the shipped behaviour exactly — last stamp match
/// in bag order wins the slot.
pub fn equip_fix() -> bool {
    knob("CS_EQUIP_FIX", 1) != 0
}

/// Where to install [`Patch::EquipRestore`], or `None` if the feature is off or
/// this is not the binary it was written against.
///
/// The call site is verified rather than assumed: the word at
/// [`EQUIP_RESTORE_CALL`] has to actually be a `bl` whose target is
/// `ITEM_setEquip`. That is a stronger check than a hardcoded instruction word
/// and it needs no second table, because the encoding carries the target.
pub fn equip_restore_ret(base: u32, target: u32, mem: &mut GuestMem) -> Option<u32> {
    if !equip_fix() {
        return None;
    }
    let at = base.wrapping_add(EQUIP_RESTORE_CALL);
    let word = mem.r32(at);
    if word >> 24 != 0xeb {
        return None;
    }
    // `bl` is a sign-extended 24-bit word offset from PC+8.
    let off = ((word << 8) as i32 >> 8) as u32;
    (at.wrapping_add(8).wrapping_add(off << 2) == target).then(|| at.wrapping_add(4))
}

/// `cmp r2, #5` at `MONSTER_dropExp+0x3c` — the kill experience level gap.
///
/// `MONSTER_dropExp` opens by loading the monster's level (`monster+0x138`)
/// and the hero's (`hero+0x138`, the byte `HERO_getCurLevel` reads and
/// `HERO_add_Exp` increments), and if `hero - monster > 5` it returns before
/// computing anything: the kill pays no experience at all. That cliff is the
/// shipped game's — and it is the whole of "몬스터를 잡아도 경험치가 안
/// 오른다": quest rewards enter through `Event_setQuestReward` and never pass
/// this gate, which is why they keep flowing when kills stop. The growth 비약
/// (x2/x4) make the cliff far easier to walk off than on the original phone,
/// because the hero now outlevels an area's monsters mid-area.
///
/// The award itself is computed right after the gate from the *monster's*
/// level alone — level x8 up to 40, x12 for 41..59, x19 past that — so
/// bypassing the branch makes the guest produce the full award for the
/// monster, and `host_exp_mul` (already sitting on `HERO_add_Exp` for the
/// 비약) scales it down by the remembered gap: halved per level past five,
/// never below 1. A gap of five or less declines here and the guest's own
/// `cmp`/`bgt` run untouched — full award, exactly the shipped path.
const EXP_GAP_CMP: u32 = 0x3c;
const EXP_GAP_CMP_WORD: u32 = 0xe352_0005;
/// The `bgt` one word later that is the no-experience exit. Skipping both
/// lands on `cmp r7, #0x28`, which sets its own flags — nothing downstream
/// reads the skipped compare's.
const EXP_GAP_BGT_WORD: u32 = 0xca00_0015;
/// Return addresses of the two `bl HERO_add_Exp` inside `MONSTER_dropExp` —
/// the hero's and the pet's. `host_exp_mul` decays only awards returning to
/// one of these; quest rewards return elsewhere and are left whole.
pub const EXP_GAP_LRS: [u32; 2] = [0x7c, 0xcc];
/// Levels a monster may sit below the hero and still pay in full — the
/// shipped gate's own threshold, kept.
const EXP_GAP_FULL: i32 = 5;

/// `CS_EXP_GAP=0` restores the shipped cliff: a kill more than five levels
/// below the hero pays nothing.
pub fn exp_gap() -> bool {
    knob("CS_EXP_GAP", 1) != 0
}

/// Where to install [`Patch::ExpGapGate`], or `None` if this is not the binary
/// it was written against. Both instruction words are checked, and the two
/// `bl` whose return addresses gate the decay are decoded to make sure they
/// still call `HERO_add_Exp`.
pub fn exp_gap_site(base: u32, add_exp: u32, mem: &mut GuestMem) -> Option<u32> {
    let at = base.wrapping_add(EXP_GAP_CMP);
    if mem.r32(at) != EXP_GAP_CMP_WORD || mem.r32(at.wrapping_add(4)) != EXP_GAP_BGT_WORD {
        return None;
    }
    for lr in EXP_GAP_LRS {
        let bl_at = base.wrapping_add(lr).wrapping_sub(4);
        let word = mem.r32(bl_at);
        if word >> 24 != 0xeb {
            return None;
        }
        let off = ((word << 8) as i32 >> 8) as u32;
        if bl_at.wrapping_add(8).wrapping_add(off << 2) != add_exp {
            return None;
        }
    }
    Some(at)
}

/// `ldr r2, [sp, #0x1c]` at `GameMenuFrame_KeyPressed+0x32ec` — the single
/// place the top-level icon menu consumes RIGHT. The slot holds 1 for a
/// hardware ArrowRight, for the numeric alias '6', and for a tap on the drawn
/// D-pad's right arrow: three entry paths, one consumer. Hooking here rather
/// than at the function entry is what makes the touch arrow work too — the
/// touch path never produces a key code, it hit-tests with `Check_Rect` and
/// stores the result in this slot (`+0x36c`).
const BAG_RIGHT_TEST: u32 = 0x32ec;

/// `mov r6, #0` at the head of the same screen's OK handler. Entering there is
/// bit for bit what pressing OK does: it reads `_g_IGMKey`, copies it into
/// `_GameMenuState` and runs that entry's init (`init_ItemBag` for the bag).
/// It loads everything it needs itself, so the only live state it inherits is
/// `r4`/`sl`, which are set in the prologue and never rewritten.
const MENU_OK_ENTRY: u32 = 0x6e14;

/// Both instruction words, checked at install time. `[sp+0x1c]` is a raw frame
/// slot — legitimate only because `sp` never moves after the prologue — and a
/// differently built guest would leave the offsets pointing at something else
/// entirely, so the patch refuses to install unless the code is the code it
/// was written against.
const BAG_RIGHT_TEST_WORD: u32 = 0xe59d_201c;
const MENU_OK_ENTRY_WORD: u32 = 0xe3a0_6000;

/// `_GameMenuState` while the 3x3 icon menu is up (`IGM_DrawMenu`), and the
/// cell in it that is 가방. The cursor value *is* the state the OK handler
/// switches to, and state 4 renders `GameMenuFrame_drawItemBag`.
const IGM_STATE_MENU: u32 = 0xc;
const IGM_CELL_BAG: u8 = 4;

/// Where to install [`Patch::BagRight`], or `None` if the feature is off or the
/// guest is not the binary this was written against. Both instruction words are
/// checked, because a mid-function hook on a raw frame slot has no other way to
/// notice it has drifted.
pub fn bag_right_site(base: u32, mem: &mut GuestMem) -> Option<u32> {
    if !bag_right() {
        return None;
    }
    let test = base.wrapping_add(BAG_RIGHT_TEST);
    (mem.r32(test) == BAG_RIGHT_TEST_WORD
        && mem.r32(base.wrapping_add(MENU_OK_ENTRY)) == MENU_OK_ENTRY_WORD)
        .then_some(test)
}

/// `CS_BAG_RIGHT=1` makes RIGHT on 가방 open the bag. Off by default since
/// 2026-09-24: the player wanted RIGHT back to what the original does on that
/// screen, which is move the highlight 가방 -> 퀘스트.
pub fn bag_right() -> bool {
    knob("CS_BAG_RIGHT", 0) != 0
}

// ---- 두 번째 오토루팅은 가방에 박힌다 ---------------------------------------
//
// 오토루팅 (`NPC/MERCHANT_2` row 3, item kind 0x13) is a permanent per-slot
// unlock: using it sets `hero[0x898]`, which `MONSTER_dropItem+0x184` and
// `MONSTER_dropIndexItem+0x174` read to auto-pick every drop, and which
// `GameFrame_saveGame+0x2a0` writes to the save. Buying a second one is
// therefore always a mistake — and the shipped code makes that mistake
// permanent, because the copy can be neither used, sold nor dropped:
//
// * **used** — the arm below. `hero[0x898] != 0` sets the IGM error index to 6
//   (`getInCodeString(6 + 0x88)` = 「이미 오토루팅이 적용되어있습니다.」) and
//   returns. `ITEMSTORAGE_removeCurItem` is on the **other** arm only, so the
//   stack never shrinks.
// * **sold** — the template's price halfword (`item[0x10]`) is the sentinel
//   `0xffff`, which is what `drawItemInfo+0x1ac` tests to print the red
//   「판매불가」 tag. Measured in the shipped `ITEM/001` sub-blob 9: kind 0x13
//   has exactly one record and its price is `0xffff`.
// * **dropped** — the 사용/버리기 two-cell menu is `_GameMenuState` 0x15, and
//   the use dispatcher at `GameMenuFrame_KeyPressed+0x5924` only sends item
//   kinds 0x18 and 0x1d..0x27 there. Kind 0x13 gets its own 예/아니오 confirm
//   (state 0x19) instead, which has no 버리기 cell at all.
//
// So the host consumes the duplicate: one `ITEMSTORAGE_removeCurItem(1)` — the
// guest's own call, with the guest's own argument, taken from the arm four
// instructions away — and then it **declines**, leaving the message and the
// state change to the guest. Nothing else is touched: the flag is already set,
// so the drop path and the save see exactly what they saw before.

/// `ldr r3, [pc, #0x8c]` at `GameMenuFrame_KeyPressed+0x4958`, the head of the
/// "auto-loot is already on" arm. Reached only from the `beq` two instructions
/// earlier, which is itself reached only from state 0x19's 예 cell.
const AUTOLOOT_APPLIED: u32 = 0x4958;

/// The other arm of the same branch — `mov r0, #1` / `bl
/// ITEMSTORAGE_removeCurItem` / `strb r3, [r5, #0x898]`. Verified as well as
/// the hook site, because it is where the argument and the meaning of this
/// patch come from: if the guest ever stops consuming the item *here*, taking
/// one away over there is no longer "what using it does".
const AUTOLOOT_APPLY: u32 = 0x74c8;

/// `ldrb r1, [r2, #0x898]` / `cmp r1, #0` / `beq` — the flag test the hook
/// hangs off, then the arm's own first two instructions (`ldr r3, [pc, #0x8c]`
/// and `mov r2, #6`, the error index for that message), then `mov r0, #1` and
/// `strb r3, [r5, #0x898]` on the apply arm.
const AUTOLOOT_WORDS: [(u32, u32); 6] = [
    (AUTOLOOT_APPLIED - 0xc, 0xe5d2_1898),
    (AUTOLOOT_APPLIED - 8, 0xe351_0000),
    (AUTOLOOT_APPLIED, 0xe59f_308c),
    (AUTOLOOT_APPLIED + 4, 0xe3a0_2006),
    (AUTOLOOT_APPLY, 0xe3a0_0001),
    (AUTOLOOT_APPLY + 0x10, 0xe5c5_3898),
];

/// Where to install [`Patch::AutoLootDup`], or `None` if the feature is off or
/// the guest is not the binary this was written against.
///
/// Six instruction words, and two things the words alone cannot say: that the
/// `beq` really lands on the apply arm — which is what makes this address "the
/// duplicate case" rather than some other test of some other byte — and that
/// the `bl` next to it really calls `ITEMSTORAGE_removeCurItem`, which is the
/// function this patch is about to call itself.
pub fn autoloot_dup_site(base: u32, mem: &mut GuestMem, remove_cur: u32) -> Option<u32> {
    if !autoloot_dup() {
        return None;
    }
    if AUTOLOOT_WORDS.iter().any(|&(off, word)| mem.r32(base.wrapping_add(off)) != word) {
        return None;
    }
    let apply = base.wrapping_add(AUTOLOOT_APPLY);
    // The `beq` itself, decoded rather than compared: its offset is what ties
    // this address to that arm, and `branch_target` below only knows the
    // unconditional forms.
    let at = base.wrapping_add(AUTOLOOT_APPLIED - 4);
    let beq = mem.r32(at);
    let to = at.wrapping_add(8).wrapping_add((((beq << 8) as i32 >> 8) as u32) << 2);
    if beq >> 24 != 0x0a || to != apply {
        return None;
    }
    if branch_target(apply.wrapping_add(4), mem, true) != Some(remove_cur) {
        return None;
    }
    Some(base.wrapping_add(AUTOLOOT_APPLIED))
}

/// `CS_AUTOLOOT_DUP=0` leaves a duplicate 오토루팅 stuck in the bag forever,
/// which is what the original does. On by default: the item has no other exit.
pub fn autoloot_dup() -> bool {
    knob("CS_AUTOLOOT_DUP", 1) != 0
}

// ---- 메인메뉴 「내 ID」 타일 -------------------------------------------------
//
// The game's idea of "who you are" is a phone number, and `identity.rs` mints
// one per install. **A player cannot read it anywhere.** Measured: the boss sent
// two letters to their real number `<real number>` while the phone's game address
// was `01077778888`; the letters arrived at a slot nobody uses and had to be
// moved by hand on the server. Handing the APK to a friend makes 편지 unusable,
// because neither of them can say what address to write to.
//
// The main menu already has a place to put it. Its 3x3 grid is nine live tiles
// (`getInCodeString(0x1c2 + i)`), two of which — 1 패치 and 7 커뮤니티 — are
// **dead features drawn grey**. Not blank: `_menuState` 1 and 7 share a render
// arm and both draw `drawAndroidAlertMsg`, 「안드로이드 버전에서는 사용되지 않는
// 메뉴입니다」.
//
// And tile 7 is already reachable. The grey is computed **inside the draw
// function** from the tile's row and column and nowhere else; not one of the
// four cursor moves skips it, and the touch path hit-tests all nine cells. So
// this is not "wake a dead button" — it is "the button already works, point it
// somewhere useful and stop drawing it grey".
//
// Two hooks, one instruction each.

/// `MainMenuFrame_KeyPressed+0xa68` — `str r2, [r1]`, i.e.
/// `*_menuState = (s8)_menuCursor`. The one instruction where this screen
/// commits to a tile.
///
/// Both ways in pass through it: the OK key (`-5`, jump-table entry 11 at
/// `+0x13c8` -> `+0x15a0` -> `+0xa08`) and the second tap on an already
/// selected cell (`+0x25b8` sets the frame's "hit" slot and the loop falls out
/// here). Nothing else branches into the block — a whole-`.text` sweep finds
/// exactly one inbound branch, and it lands above this instruction.
const MY_ID_COMMIT: u32 = 0xa68;
const MY_ID_COMMIT_WORD: u32 = 0xe581_2000;

/// The three words around it, checked at install. `+0xa5c`/`+0xa60` are the
/// loads that put `&_menuCursor` and `&_menuState` in `r3`/`r1`, and `+0xa6c`
/// is the `cmp r3, #8` that runs **after** we skip the store — it is what sends
/// a tile-7 press to the epilogue instead of into some other arm, so it is part
/// of the contract, not scenery.
const MY_ID_LOAD_CURSOR_WORD: u32 = 0xe794_3001;
const MY_ID_LOAD_STATE_WORD: u32 = 0xe794_1007;
const MY_ID_AFTER_WORD: u32 = 0xe353_0008;

/// `MainMenuFrame_drawMainMenu+0xb0` — `cmp r4, #1`, the first of the two
/// instructions that decide a tile is drawn grey:
///
/// ```text
///   +0xb0  cmp   r4, #1       ; column
///   +0xb4  movne r5, #0
///   +0xb8  andeq r5, r5, #1   ; r5 = (row == 0 || row == 2) & (col == 1)
///   +0xbc  cmp   r5, #0       ; -> tiles 1 and 7
///   +0xc8  beq   +0x194       ; 0 = the ordinary tile: icon, label, full alpha
/// ```
///
/// Answering `r5 = 0` and resuming at `+0xbc` draws tile 7 with the **same
/// code** every live tile uses. No new drawing, and `r5` is reloaded from the
/// frame at the top of each tile's iteration so it cannot leak to tile 1.
const MY_ID_GREY: u32 = 0xb0;
const MY_ID_GREY_WORD: u32 = 0xe354_0001;
const MY_ID_GREY_NE_WORD: u32 = 0x13a0_5000;
const MY_ID_GREY_EQ_WORD: u32 = 0x0205_5001;
const MY_ID_GREY_TEST: u32 = 0xbc;
const MY_ID_GREY_TEST_WORD: u32 = 0xe355_0000;

/// Which tile. Row 2, column 1 — 커뮤니티, whose feature does not exist in this
/// build. Its neighbour, tile 1 (패치), is left grey on purpose: it is equally
/// dead but there is only one thing to say here.
const MY_ID_TILE: u32 = 7;

/// Tile 2 — 편지. Row 0, column 2. Retired with the servers it talked to;
/// see [`letter_tile`].
const LETTER_TILE: u32 = 2;

/// `MainMenuFrame_KeyPressed+0xa34` — `beq +0x21a8`, taken when the cursor is
/// on tile 2. A press on 편지 never reaches the commit: it goes to the letter's
/// own network-fee question first (`네트워크 연결 시 통화료가 부과됩니다`) and
/// only 예 there opens the frame. Retiring the tile means this branch is not
/// taken, so the press falls through to the commit like every other tile's,
/// where [`Vm::host_my_id_key`] turns it into the grey-tile state. The
/// `cmp ip, #2` before it and the branch's target are both checked.
const LETTER_BRANCH: u32 = 0xa34;
const LETTER_BRANCH_WORD: u32 = 0x0a00_05db;
const LETTER_BRANCH_CMP: u32 = 0xa30;
const LETTER_BRANCH_CMP_WORD: u32 = 0xe35c_0002; // cmp ip, #2
const LETTER_BRANCH_TARGET: u32 = 0x21a8;

/// The shipped grey-tile path in `MainMenuFrame_drawMainMenu` is **one call**,
/// `UI_RoundRect(g, w/2 - 0x15, h/2 - 0x40 + row, 0x2b, 0x28, 0x7f)`, and
/// nothing else — no icon, no label, no cursor highlight. Its `x` is column
/// 1's, hard-coded, because both tiles that shipped grey are in column 1; for
/// tile 2 it would paint over 공용창고. So the retired tile draws the same box
/// itself, one column over, and rejoins the loop at `+0x12c` (`cmp r5, #0`,
/// with `r5 = 1` meaning grey) exactly where the shipped path does. The
/// column and row offsets are the loop's own registers, `r6` and `sb`, which
/// step by [`MENU_TILE_STRIDE`].
const MENU_GREY_CONT: u32 = 0x12c;
const MENU_GREY_CONT_WORD: u32 = 0xe355_0000; // cmp r5, #0
const MENU_GREY_DX: i32 = 0x15;
const MENU_GREY_DY: i32 = 0x40;
const MENU_GREY_W: u32 = 0x2b;
const MENU_GREY_H: u32 = 0x28;
const MENU_GREY_ALPHA: u32 = 0x7f;
const MENU_TILE_STRIDE: i32 = 0x2e;
/// The words that say the box above is still that call: its width
/// (`mov r3, #0x2b` at +0xc4), height (`mov ip, #0x28` at +0xd4), `y`
/// (`sub r2, r2, #0x40` at +0xec), `x` (`sub r1, r1, #0x15` at +0xf0), row
/// (`add r2, r2, sb` at +0xf4), alpha (`mov ip, #0x7f` at +0xfc), the `bl`
/// itself (+0x104), and the two strides (+0x144, +0x158).
const MENU_GREY_WORDS: [(u32, u32); 8] = [
    (0xc4, 0xe3a0_302b),
    (0xd4, 0xe3a0_c028),
    (0xec, 0xe242_2040),
    (0xf0, 0xe241_1015),
    (0xf4, 0xe082_2009),
    (0xfc, 0xe3a0_c07f),
    (0x144, 0xe286_602e),
    (0x158, 0xe289_902e),
];
const MENU_GREY_CALL: u32 = 0x104;

/// The `_menuState` a press on a shipped grey tile stored — tile 7's own
/// number, because the commit stores the cursor and the render dispatch
/// (`MainMenuFrame_Render+0xb8`) sends states 1 and 7 to the same arm,
/// `drawAndroidAlertMsg`. A retired tile is sent here rather than to its own
/// state, so it behaves like the grey tiles the game shipped with, and
/// `the_dead_letter_tile_goes_where_the_shipped_grey_tiles_went` pins the
/// dispatch.
const UNUSED_MENU_STATE: u32 = 7;
/// `addls pc, pc, rN, lsl #2` — the two dispatches that test pins, as offsets
/// into `MainMenuFrame_Render` and `MainMenuFrame_KeyPressed`.
#[cfg(test)]
const MENU_RENDER_DISPATCH: u32 = 0xb8;
#[cfg(test)]
const MENU_KEY_DISPATCH: u32 = 0x5c;

/// The tile's label id in the `getInCodeString` table, and what to say instead.
///
/// `내 ID` in EUC-KR. `;` would be a line break and the twelve bytes
/// `HF_IsEscape` swallows are not in it — see `identity::REJECTED_ASCII` for
/// that list. Written as bytes because anything handed back to the guest never
/// goes through `String` (`CLAUDE.md`).
const MY_ID_LABEL_ID: u32 = 0x1c9;
const MY_ID_LABEL: &[u8] = &[0xB3, 0xBB, b' ', b'I', b'D'];

/// The popup's first line. The address follows on the next line — `;` is the
/// renderer's line break, which is how the shipped strings do it
/// (`안드로이드 버전에서는; 사용되지 않는 메뉴입니다.`).
///
/// **No `%`.** `setPopUp` ends in `MC_knlSprintk(mainFrame+0x8f, text)` with the
/// text as the *format*, and the address is spliced into this string.
///
/// It says 「ID」 even before one has been chosen, and that is deliberate: the
/// minted number **is** the player's ID as far as they are concerned — it is the
/// address letters arrive at. 「번호」 and 「ID」 is our distinction, not theirs,
/// and making them learn it buys nothing.
///
/// `이 ID로 편지를 받습니다;`
const MY_ID_TITLE: &[u8] = &[
    0xC0, 0xCC, 0x20, 0x49, 0x44, 0xB7, 0xCE, 0x20, 0xC6, 0xED, 0xC1, 0xF6, 0xB8, 0xA6, 0x20, 0xB9,
    0xDE, 0xBD, 0xC0, 0xB4, 0xCF, 0xB4, 0xD9, 0x3B,
];

/// The same popup before an ID has been claimed: `편지를 받을 ID가 없습니다;ID 정하기`
///
/// **The comment above used to justify showing the minted number here**, on the
/// grounds that it *is* the address letters arrive at and that 「번호」 vs 「ID」
/// is our distinction rather than the player's. That was true until
/// `REQUIRE_KNOWN_RECIPIENT` went live on 2026-08-17: the server now accepts a
/// recipient only if it is a **claimed name** in `ids.json`, so a bare minted
/// number bounces with [`ERR_NO_SUCH_ID`]. Measured against the live server —
/// `<real number>` is a real number with a real mailbox and it is refused.
///
/// So on a fresh install the old text put an address under 「이 ID로 편지를
/// 받습니다」 that **nobody can send to** — the same lie this tile was built to
/// close, pointed the other way (a friend who copies it down gets a bounce).
///
/// The second line is still the button: touching it opens the keyboard exactly
/// as touching an address does. It is a label for the action, not a hint about
/// the gesture — the hint line that was tried and removed said 「한 번 더 누르면
/// 변경」, which explained a gesture instead of naming the thing.
const MY_ID_NONE: &[u8] = &[
    0xC6, 0xED, 0xC1, 0xF6, 0xB8, 0xA6, 0x20, 0xB9, 0xDE, 0xC0, 0xBB, 0x20, 0x49, 0x44, 0xB0, 0xA1,
    0x20, 0xBE, 0xF8, 0xBD, 0xC0, 0xB4, 0xCF, 0xB4, 0xD9, 0x3B, 0x49, 0x44, 0x20, 0xC1, 0xA4, 0xC7,
    0xCF, 0xB1, 0xE2,
];

/// What the claim came back as. All four are single lines: `drawPopWindow` is
/// the game's own box and the shipped strings that use it are one or two lines.
///
/// `ID가 정해졌습니다`
pub const ID_SET_MSG: &[u8] = &[
    0x49, 0x44, 0xB0, 0xA1, 0x20, 0xC1, 0xA4, 0xC7, 0xD8, 0xC1, 0xB3, 0xBD, 0xC0, 0xB4, 0xCF, 0xB4,
    0xD9,
];
/// `이미 쓰이고 있는 ID 입니다`
pub const ID_TAKEN_MSG: &[u8] = &[
    0xC0, 0xCC, 0xB9, 0xCC, 0x20, 0xBE, 0xB2, 0xC0, 0xCC, 0xB0, 0xED, 0x20, 0xC0, 0xD6, 0xB4, 0xC2,
    0x20, 0x49, 0x44, 0x20, 0xC0, 0xD4, 0xB4, 0xCF, 0xB4, 0xD9,
];
/// `쓸 수 없는 ID 입니다`
pub const ID_REFUSED_MSG: &[u8] = &[
    0xBE, 0xB5, 0x20, 0xBC, 0xF6, 0x20, 0xBE, 0xF8, 0xB4, 0xC2, 0x20, 0x49, 0x44, 0x20, 0xC0, 0xD4,
    0xB4, 0xCF, 0xB4, 0xD9,
];
/// `서버에 연결할 수 없습니다`
pub const ID_OFFLINE_MSG: &[u8] = &[
    0xBC, 0xAD, 0xB9, 0xF6, 0xBF, 0xA1, 0x20, 0xBF, 0xAC, 0xB0, 0xE1, 0xC7, 0xD2, 0x20, 0xBC, 0xF6,
    0x20, 0xBE, 0xF8, 0xBD, 0xC0, 0xB4, 0xCF, 0xB4, 0xD9,
];

/// Where `drawPopWindow` puts its box, and where `PopWindowKey` puts the OK
/// button inside it — both in WIPI logical pixels, both relative to the middle
/// of the screen (`_lcdG+0x4c` / `+0x50`, halved).
///
/// ```text
///   drawPopWindow+0x20   draw_menu_box(w/2 - 0x58, h/2 - 0x32, 0xb0, 0x64)
///   PopWindowKey+0x98    Check_Rect(x, y, w/2 - 0x14, h/2 + 0x1e, 0x28, 0x12)
/// ```
///
/// The message occupies everything above that button, which is what
/// `host_my_id_popup_touch` treats as "touch the ID".
const POP_BOX_DX: i32 = -0x58;
const POP_BOX_DY: i32 = -0x32;
const POP_BOX_W: i32 = 0xb0;
const POP_OK_DY: i32 = 0x1e;

/// The event code `MainMenuFrame_KeyPressed` and `PopWindowKey` both branch on
/// for a touch (`cmp r0, #0x13`); `2` is a key press.
const TOUCH_EVENT: i32 = 0x13;

// ---- 편지 받는사람을 ID 로 --------------------------------------------------
//
// The letter screen still asks for a phone number, because that is what it was
// written for. Two shipped strings say so and the keyboard enforces it:
//
//   getInCodeString(0x1ef)  받을 전화번호를 입력하시오.
//   getInCodeString(0x275)  &받을 폰번호가 없습니다.;입력해주세요.
//   InputInit(0)            mask 8 (digits), cap 11, initial text "010"
//
// With IDs on, all three are wrong — the address a player has to type is a
// name. Each id has exactly one constant call site (`.text` sweep over 531
// `bl getInCodeString`), and `InputInit`'s mode-0 arm has exactly one caller.
//
// ## The format-string landmine is not reachable from the keyboard
//
// `MainMenuFrame_drawSendingLetter+0x310` hands the recipient to
// `MC_knlSprintk` as its **format** (a 255-byte stack buffer, no bound), which
// is why widening the mask looked dangerous. It is not: `%` appears in **no**
// keyset. The numeric one is
//
//   1234567890 - / : ; ( ) $ & @ . , ? ! ' ~ "
//
// and the Korean and English ones are letters and SPACE. So no on-screen key
// can produce a conversion specifier, at mask 8 or 0xe alike. (`;`, `$` and `&`
// *are* there and are the renderer's zero-width escapes — a recipient using one
// would render with pieces missing and could never be claimed as an ID, so it
// would reach a mailbox nobody owns. That is a wrong address, not a crash, and
// it is equally reachable today.)

/// `받을 ID를 입력하시오.`
const LETTER_ASK_ID: &[u8] = &[
    0xB9, 0xDE, 0xC0, 0xBB, 0x20, 0x49, 0x44, 0xB8, 0xA6, 0x20, 0xC0, 0xD4, 0xB7, 0xC2, 0xC7, 0xCF,
    0xBD, 0xC3, 0xBF, 0xC0, 0x2E,
];
/// `&받을 ID가 없습니다.;입력해주세요.` — the leading `&` is the shipped colour
/// marker and `;` its line break, both kept exactly as the original had them.
const LETTER_NO_ID: &[u8] = &[
    0x26, 0xB9, 0xDE, 0xC0, 0xBB, 0x20, 0x49, 0x44, 0xB0, 0xA1, 0x20, 0xBE, 0xF8, 0xBD, 0xC0, 0xB4,
    0xCF, 0xB4, 0xD9, 0x2E, 0x3B, 0xC0, 0xD4, 0xB7, 0xC2, 0xC7, 0xD8, 0xC1, 0xD6, 0xBC, 0xBC, 0xBF,
    0xE4, 0x2E,
];
const LETTER_ASK_ID_ID: u32 = 0x1ef;
const LETTER_NO_ID_ID: u32 = 0x275;

/// The status `cs_server` sends when the letter's recipient is not a registered
/// ID, and what to say instead of the guest's `<ERROR:9401>`.
///
/// `ProcRecvPacket_Single` reads the status **before** dispatching on the sub
/// (`UM_ByteToShort_BIG(pkt+0xa)`, then `bne`) and hands a non-zero one to
/// `PocErrorCode`. The send-success arm — `MainMenuFrame_afterSendLetterWithItem`,
/// which is where `hero[0x89c] -= money` lives — is never reached. So a refusal
/// costs the player nothing, and this hook only has to fix the wording.
const ERR_NO_SUCH_ID: u32 = 9401;
/// `없는 ID 입니다;확인 후 다시 보내세요`
pub const NO_SUCH_ID_MSG: &[u8] = &[
    0xBE, 0xF8, 0xB4, 0xC2, 0x20, 0x49, 0x44, 0x20, 0xC0, 0xD4, 0xB4, 0xCF, 0xB4, 0xD9, 0x3B, 0xC8,
    0xAE, 0xC0, 0xCE, 0x20, 0xC8, 0xC4, 0x20, 0xB4, 0xD9, 0xBD, 0xC3, 0x20, 0xBA, 0xB8, 0xB3, 0xBB,
    0xBC, 0xBC, 0xBF, 0xE4,
];

/// `InputInit`'s letter mode, and the `_mLetterState` that means "the recipient
/// line" (as opposed to `LETTER_STATE_BODY`, which shares the same entry).
const IME_MODE_LETTER: u32 = 0;

/// `bl InputInit` at `MainMenuLetterSendKey+0x5fc`, checked at install together
/// with the `strh r5, [r3, #0x4e]` before it — the last of eight stores that
/// clear `_mailBox+0x42..0x50`, the recipient field this call is about to ask
/// the player to fill. That store is what identifies the site as the recipient
/// one rather than some other `InputInit`.
const LETTER_INPUT_CALL: u32 = 0x5fc;
const LETTER_INPUT_CALL_PREV: u32 = 0xe1c3_54be;

/// The **first** of those eight stores, `strh r5, [r3, #0x50]`.
///
/// Hooked to read the recipient one instruction before the guest erases it.
/// Re-opening 받는사람 to fix a typo should not hand back an empty box, and by
/// the time `InputInit` runs the old text is already gone — this is the last
/// moment it exists.
const LETTER_CLEAR_FIRST: u32 = 0x5dc;
const LETTER_CLEAR_FIRST_WORD: u32 = 0xe1c3_55b0;

/// `_mailBox+0x42` — the recipient, 16 bytes up to the body at `+0x52`.
const MAILBOX_TO: u32 = 0x42;
const MAILBOX_TO_LEN: u32 = 0x10;

/// `CS_ID_RECIPIENT=0` leaves the letter screen asking for a phone number.
///
/// **Deliberately not tied to [`id_as_phone`].** It was, and that was wrong:
/// the two answer different questions. `CS_ID_AS_PHONE` decides what *this*
/// install reports about itself — the key its own mailbox lives under, which is
/// why it is off and stays off until the migration is worth doing. Who a letter
/// is *addressed to* is not that question. `cs_server` resolves a recipient
/// through `ids.json`/`aliases.json` no matter what the sender reported, so an
/// install still reporting its number can already reach a friend by ID.
///
/// Coupling them shipped versionCode 68 with the whole letter side inert: the
/// recipient line still seeded `"010"`, still refused letters, and a bounce
/// still drew `<ERROR:9401>`. Worse now than before — with the server's
/// `REQUIRE_KNOWN_RECIPIENT` on, a numeric keypad asks for the one thing that
/// is guaranteed to be refused.
pub fn id_recipient() -> bool {
    knob("CS_ID_RECIPIENT", 1) != 0
}

/// `MX_imeSetProperty` — the input-mode mask. `InputInit` sets 8 (digits) for
/// the letter and 0xe (Hangul + letters + digits) for a pet name.
const IME_PROP_MASK: u32 = 0;
const IME_MASK_NAME: u32 = 0xe;

/// `MX_imeSetProperty` — the maximum length. `InputInit` sets it to 11 in both
/// its modes (`+0x74`, `+0xb4`); the ID needs the full
/// [`crate::identity::ID_MAX_BYTES`].
const IME_PROP_MAXLEN: u32 = 3;

/// The buffer `MX_imeGetString` fills, sized as the guest sizes its own
/// (`memset(mainFrame+0xf3, 0, 0x64)` right before the same call).
///
/// **Not sized to the length cap.** The cap we set is 14, but whether
/// `MX_imeSetProperty(3, n)` counts bytes or characters is not something this
/// codebase has proved, and a Hangul syllable is two bytes. Matching the
/// guest's own buffer means the answer does not matter.
const IME_SCRATCH: u32 = 0x64;

/// Inside `_midas_internalImeInstance`: the edit buffer and the caret.
///
/// Both read straight out of `MX_imeGetString`, which is the only function that
/// has to agree with us:
///
/// ```text
///   0x2ee1c  add r6, r4, #0x174     ; the buffer
///   0x2ee24  bl strlen              ; r8 = the WHOLE string in it
///   0x2ee2c  ldr r2, [r4, #0x274]   ; the caret
///   0x2ee38  memcpy(dst, buf, caret)                 ; before the caret
///   0x2ee58  memcpy(dst+n, ime+0x5c, ime[0x64])      ; the syllable being composed
///   0x2ee84  memcpy(dst+n, buf+caret, r8 - caret)    ; **and everything after it**
/// ```
///
/// That last `memcpy` is why the field has to be *zeroed* and not merely
/// terminated. `MX_imeSetProperty(2, "")` does `strcpy(buf, "")`, which writes
/// one NUL at index 0 — so `strlen` is 0 and the field looks empty, but the old
/// bytes are still there. The moment two bytes are inserted at the caret they
/// overwrite that NUL and the leftovers become live again: seeding the shipped
/// default 가름 and then clearing it, typing one letter, gives 「ㅍ름」 on screen.
/// Measured, and visible in the field — the display walks the same three pieces.
const IME_BUF: u32 = 0x174;
const IME_CARET: u32 = 0x274;
/// The buffer runs from [`IME_BUF`] up to the caret word that follows it.
const IME_BUF_LEN: u32 = IME_CARET - IME_BUF;

/// `CS_MY_ID=1` puts 「내 ID」 on tile 7. **Off by default since 2026-09-24**:
/// the ID is the address letters arrive at, and the letter tile is gone (see
/// [`letter_tile`]), so the tile went back to what it shipped as — grey, and
/// going to 「사용되지 않는 메뉴입니다」. Everything behind it (the popup, the
/// keyboard, the claim to the server) is still here and rides this one knob.
pub fn my_id_tile() -> bool {
    knob("CS_MY_ID", 0) != 0
}

/// `CS_LETTER=1` keeps main-menu tile 2 as the letter screen. **Off by
/// default since 2026-09-24**: 편지 is the online half of this game and the
/// servers it was written for are gone, so the tile is drawn grey and a press
/// on it goes to 「사용되지 않는 메뉴입니다」 — exactly what the two tiles that
/// shipped grey do, through the same two instructions. Nothing behind it is
/// removed: the letter frame is still reached by 공용창고, which borrows its
/// two widgets.
pub fn letter_tile() -> bool {
    knob("CS_LETTER", 0) != 0
}

/// Where to install [`Patch::MyIdKey`], or `None` if the guest is not the
/// binary this was written against.
///
/// Two features ride this one instruction — 「내 ID」 on tile 7 and 공용창고 on
/// tile 1 — so it is installed if either wants it and
/// [`Vm::host_my_id_key`] re-checks the knobs per tile.
pub fn my_id_key_site(base: u32, mem: &mut GuestMem) -> Option<u32> {
    if !my_id_tile() && !vault_on() && letter_tile() {
        return None;
    }
    let at = base.wrapping_add(MY_ID_COMMIT);
    (mem.r32(at.wrapping_sub(0xc)) == MY_ID_LOAD_CURSOR_WORD
        && mem.r32(at.wrapping_sub(8)) == MY_ID_LOAD_STATE_WORD
        && mem.r32(at) == MY_ID_COMMIT_WORD
        && mem.r32(at.wrapping_add(4)) == MY_ID_AFTER_WORD)
        .then_some(at)
}

/// Where to install [`Patch::MyIdTile`]. All four words of the grey test are
/// checked: the hook answers one of them and resumes inside the sequence, so a
/// drifted build must refuse rather than jump into the middle of something else.
pub fn my_id_draw_site(base: u32, mem: &mut GuestMem) -> Option<u32> {
    // Also 공용창고's tile, and also the place `vault_screen` is cleared: the
    // 3x3 grid being drawn is the proof that the chest is not on screen. And
    // the dead letter tile's grey.
    if !my_id_tile() && !vault_on() && letter_tile() {
        return None;
    }
    let at = base.wrapping_add(MY_ID_GREY);
    (mem.r32(at) == MY_ID_GREY_WORD
        && mem.r32(at.wrapping_add(4)) == MY_ID_GREY_NE_WORD
        && mem.r32(at.wrapping_add(8)) == MY_ID_GREY_EQ_WORD
        && mem.r32(base.wrapping_add(MY_ID_GREY_TEST)) == MY_ID_GREY_TEST_WORD)
        .then_some(at)
}

/// Where to install [`Patch::LetterTileBranch`], or `None` if any part of
/// retiring 편지 would not fit this binary.
///
/// All or nothing: the tile is retired by three hooks (the commit and the
/// grey test, shared with 「내 ID」 and 공용창고, plus this branch) and one
/// call the draw hook makes itself, and a build where only some of them fit
/// would leave the tile grey but alive, or alive but grey. So this checks the
/// other two sites, the branch's own two words and target, and the grey box's
/// words and `bl`, and the caller installs the branch and sets
/// `Vm::letter_retired` together.
pub fn letter_sites(key: u32, draw: u32, round_rect: u32, mem: &mut GuestMem) -> Option<u32> {
    if letter_tile() {
        return None;
    }
    if my_id_key_site(key, mem).is_none() || my_id_draw_site(draw, mem).is_none() {
        return None;
    }
    let at = key.wrapping_add(LETTER_BRANCH);
    let word = mem.r32(at);
    let target = at.wrapping_add(8).wrapping_add((((word << 8) as i32 >> 8) as u32) << 2);
    let branch_ok = word == LETTER_BRANCH_WORD
        && word >> 28 == 0 // beq
        && target == key.wrapping_add(LETTER_BRANCH_TARGET)
        && mem.r32(key.wrapping_add(LETTER_BRANCH_CMP)) == LETTER_BRANCH_CMP_WORD;
    let grey_ok = MENU_GREY_WORDS.iter().all(|&(off, w)| mem.r32(draw.wrapping_add(off)) == w)
        && mem.r32(draw.wrapping_add(MENU_GREY_CONT)) == MENU_GREY_CONT_WORD
        && branch_target(draw.wrapping_add(MENU_GREY_CALL), mem, true) == Some(round_rect);
    (branch_ok && grey_ok).then_some(at)
}

// ---- 보스 처치의 확대 연출 ------------------------------------------------
//
// The zoom the screen does when a boss takes its killing blow is
// `DM_RadialBlurLcd565`, and `Map_drawMiddleObject` is the whole of its clock:
//
//   +0x174  hero[0x899]           armed by `HERO_Attack_Monster+0x10d8/+0x18e0`
//   +0x210  mov r2, #0x14         first frame's counter, written once
//   +0x19c  bl DM_RadialBlurLcd   ray length = radius * counter / diag
//   +0x1a4  add r3, r3, #4        <- the whole of the animation
//   +0x1a8  cmp r3, #0x27
//   +0x1b0  ble  ...              past 0x27 -> DM_ReleaseLCDEffect, flags cleared
//
// So the counter takes 20, 24, 28, 32, 36 and the effect is **five frames**.
// `_lcdG+0x8c` is touched nowhere else in the image (six instructions, all in
// this function, plus the blur's own read) — it is this animation's clock and
// nothing else's.
//
// On the shipped 32-bit build those five frames were five *stalls*: 53–62 M
// guest instructions each, 2.9 s on an S25. That is what a player remembers as
// "the screen slows down on the killing blow". Porting the blur to the host
// (「성능 6」) took it to five ordinary frames — and took the flourish with it,
// which is the report this answers.
//
// Bringing it back as a stall is not on the table; that stall is the ANR
// candidate. What is wanted is the *animation* stretched, so the counter is
// stepped from the clock instead of by four:
//
//     counter = 0x14 + (0x28 - 0x14) * elapsed / CS_BOSS_ZOOM_MS
//
// which spans exactly the same range over exactly the same shipped end
// condition, just not in five frames. Everything else stays the guest's: it
// arms the effect, it releases it, it clears `hero[0x899]`.

/// The `add r3, r3, #4` that is the whole animation, as an offset into
/// `Map_drawMiddleObject`, and the words either side of it. The counter's
/// offset (`0x8c`) appears in three of them, the first value in the arm, and
/// the end in the compare — so verifying these five pins every constant this
/// retime depends on rather than trusting a single instruction.
const ZOOM_STEP: u32 = 0x1a4;
const ZOOM_STEP_WORD: u32 = 0xe283_3004;
/// `+0x1a0 ldr r3, [r4, #0x8c]` — the value the step is applied to.
const ZOOM_LOAD_WORD: u32 = 0xe594_308c;
/// `+0x1a8 cmp r3, #0x27` and `+0x1ac str r3, [r4, #0x8c]`, which run after the
/// hook returns: the host writes `r3` and skips only the `add`, so the end test
/// and the store are still the guest's.
const ZOOM_CMP_WORD: u32 = 0xe353_0027;
const ZOOM_STORE_WORD: u32 = 0xe584_308c;
/// `+0x210 mov r2, #0x14` — the counter the guest arms a fresh effect with.
const ZOOM_ARM: u32 = 0x210;
const ZOOM_ARM_WORD: u32 = 0xe3a0_2014;
/// `+0x19c bl DM_RadialBlurLcd`, decoded at install: without it the counter
/// being stepped here is not the one that drives the blur.
const ZOOM_BL: u32 = 0x19c;

// ---- 보스 중심 --------------------------------------------------------------
//
// The shipped effect is centred on the middle of the *screen* and reaches the
// corners, which is why two seconds of it reads as "the whole picture slid" —
// the drop list, the timer, the far scenery all smear as hard as the kill does.
//
// Both halves of that are fixable without inventing a new effect. The centre is
// `(w>>1, h>>1)` in the guest and `radius` is measured from it; feed a different
// centre and the same arithmetic converges on the boss instead. Then fade the
// ray length out with distance and the streaks stop before they reach the parts
// of the screen nobody was looking at.
//
// This is the first thing here that does **not** reproduce the shipped pixels.
// `CS_BOSS_ZOOM_R=0` puts both back.

/// The two `strb r3, [r7, #0x899]` that mark a boss as freshly killed, as
/// offsets into `HERO_Attack_Monster`, each with the distance back to the
/// `bl MONSTER_isBoss` that decided it was a boss and to the `mov r0, rX` that
/// handed it over. The monster register is decoded from that `mov` rather than
/// written down here, and the `bl` target is checked — a drifted site then
/// refuses instead of reading coordinates off whatever a register happens to
/// hold.
const ZOOM_KILL_SITES: [(u32, u32, u32); 2] = [(0x10d8, 0x20, 0x24), (0x18e0, 0x1c, 0x20)];
const ZOOM_KILL_WORD: u32 = 0xe5c7_3899;

/// `char[8]` / `char[0xc]` — a character's position in map space.
/// `CHAR_setPosition` is the proof: it is `char[8] += dx; char[0xc] += dy`.
const CHAR_X: u32 = 8;
const CHAR_Y: u32 = 0xc;

/// How `Map_drawMiddleObject` turns map space into screen space, as the six
/// loads in its prologue (`+0x1c ..= +0x30`) read them:
///
/// ```text
///     screen_x = obj_x + _map[0x490] + _map[0x150] - _map[0x140]
///     screen_y = obj_y + _map[0x494] + _map[0x154] - _map[0x144]
/// ```
///
/// Recomputed from `_map` rather than lifted out of `r7`/`r8`, because the blur
/// needs it in a different function from the one that holds them.
const MAP_DRAW_OFF: [(u32, u32); 6] = [
    (0x1c, 0xe593_c150),
    (0x20, 0xe593_1154),
    (0x24, 0xe593_7490),
    (0x28, 0xe593_8494),
    (0x2c, 0xe593_2144),
    (0x30, 0xe593_3140),
];

/// How far the effect reaches, as a percentage of the screen height. `0` — the
/// default — is the shipped reach: every corner, which is what the original does
/// and what the reference footage of it shows. Anything else confines the streaks
/// to a bubble around the boss; 40 is 96 px on the 240-line screen, which leaves
/// the shelf, the potions and the map at the edges untouched.
///
/// It is a percentage, not a switch: `1` is a two-pixel bubble, i.e. no visible
/// effect at all.
///
/// **It does not control the centring.** That follows the boss whenever the kill
/// sites resolve, on every value of this knob including `0` — so `0` is the
/// shipped *reach* around a centre the shipped code would not have used, which
/// is the reference footage's look and not the shipped bytes. Only
/// `CS_BOSS_ZOOM_MS=0`, which installs none of this, gives those back. Reaching
/// the corners read as "the whole picture slid" while the effect was *also*
/// compounding forty-six times at full strength; that part is what the
/// applications and strength knobs answer.
pub fn boss_zoom_radius_pct() -> i32 {
    knob("CS_BOSS_ZOOM_R", 0).clamp(0, 400)
}

/// Resolve [`ZOOM_KILL_SITES`], decoding the monster register from each site's
/// `mov r0, rX`. Empty when the guest is not the binary this was written
/// against, which leaves the effect centred on the screen.
pub fn zoom_kill_sites(base: u32, is_boss: u32, mem: &mut GuestMem) -> Vec<(u32, Patch)> {
    let mut out = Vec::new();
    for (at, bl_back, mov_back) in ZOOM_KILL_SITES {
        let site = base.wrapping_add(at);
        let bl_at = site.wrapping_sub(bl_back);
        let mov = mem.r32(site.wrapping_sub(mov_back));
        let word = mem.r32(bl_at);
        let target = bl_at.wrapping_add(8).wrapping_add((((word << 8) as i32 >> 8) as u32) << 2);
        // `mov r0, rX` is `e1a0000X`, and the call it feeds must be the one that
        // says this monster is a boss.
        if mem.r32(site) != ZOOM_KILL_WORD
            || word >> 24 != 0xeb
            || target != is_boss
            || mov & 0xffff_fff0 != 0xe1a0_0000
        {
            continue;
        }
        out.push((site, Patch::ZoomKill { rn: (mov & 0xf) as u8 }));
    }
    out
}

/// `hero[0x899]` — "a boss just died". Written by the two killing blows in
/// `HERO_Attack_Monster`, read by `Map_drawMiddleObject` to start the effect
/// and by `HERO_Key_Press`/`HERO_Key_Press_Shortcut_Slot` to swallow movement
/// and skills while it runs. Cleared by `+0x1d4`/`+0x1d8` at the end, on both
/// heroes. Sixteen references in the image, and that is all of them.
const HERO_BOSS_ZOOM: u32 = 0x899;
/// `hero[0x282]`: the whole-screen zoom percent, and the only thing that
/// switches `HERO_Draw_Resize_Background` on — `GameFrame_Render+0x8c` reads
/// it as a signed halfword and skips the call when it reads exactly 100.
/// `+0x278`/`+0x27c` next to it are the centre, as an *offset* from the middle
/// of the screen, so zero is centred.
const HERO_RESIZE_SCALE: u32 = 0x282;
/// `_event[0xcfb]`: an event opened an NPC screen; the smith's close clears it.
const EVENT_NPC_UI_OPEN: u32 = 0xcfb;
/// `NpcUI_setNpc`'s argument for the blacksmith (0 skill master, 2 merchant).
const NPC_BLACKSMITH: u32 = 1;
/// ...and the merchant, whose category menu 「레시피2」 adds a row to. 2, not
/// 0: `NpcUI_setNpc+0xdc` clears `merchantState` for that one value, which is
/// what puts the screen on the category list rather than in a shop. 0 is the
/// skill master.
const NPC_MERCHANT: u32 = 2;

/// The counter a fresh effect starts at, and the first value that ends it.
/// `cmp #0x27` + `ble` means 0x27 still draws and 0x28 releases, so the span
/// the retime spreads over the requested duration is `0x14 ..= 0x28`.
pub(crate) const ZOOM_FIRST: i32 = 0x14;
pub(crate) const ZOOM_END: i32 = 0x28;

/// Sub-steps per unit of counter.
///
/// Twenty integer steps spread over two seconds is one visible change every
/// hundred milliseconds, and the change is a whole sample added to the ray —
/// the exact stepping the original hid behind being too slow to see. The blur's
/// ray length is `radius * counter / diag`, so scaling numerator and
/// denominator together gives eighths of a step that are *bit for bit* the
/// shipped value whenever the eighth is a whole one. `host_radial_blur` uses
/// the fine value only when it agrees with the counter the guest holds.
pub(crate) const ZOOM_SUB: i32 = 8;

/// Frames one zoom may last before it is ended regardless of the clock.
///
/// `hero[0x899]` locks movement and skills until this counter passes the end,
/// so a clock that stopped advancing would leave the player standing there for
/// good. Twenty seconds at 60 fps — far past any duration worth asking for, and
/// unreachable while time moves at all.
const ZOOM_MAX_FRAMES: u32 = 1200;

/// The shipped step, `add r3, r3, #4`, kept as the size of a *world* step.
///
/// Stretching only the zoom would leave the scene running underneath it at full
/// speed, which is not what the effect used to look like: the shipped five
/// frames were five **stalls**, so the world advanced five ticks across the
/// whole 2.9 s and everything moved together. `host_zoom_world` reproduces that
/// by letting `GameFrame_Process` through once per four counts — the same five
/// ticks, at the same five points of the animation, spread over the same wall
/// time the zoom now takes.
const ZOOM_SHIPPED_STEP: i32 = 4;

/// How many world ticks the shipped effect gave the world, which is what
/// `add r3, r3, #4` produced across `0x14 ..= 0x27`. The default for
/// [`boss_zoom_ticks`] and what the tests hold the shipped case to.
pub(crate) const ZOOM_WORLD_TICKS: i64 = ((ZOOM_END - ZOOM_FIRST) / ZOOM_SHIPPED_STEP) as i64;

/// How long the boss-kill zoom should take, in milliseconds. `0` installs
/// nothing and leaves the shipped five frames.
pub fn boss_zoom_ms() -> i32 {
    knob("CS_BOSS_ZOOM_MS", 1000).clamp(0, 60_000)
}

/// How many times the world is let through across the effect.
///
/// **Five is not a smoothness setting — it is how far into the death the effect
/// gets.** The kill plays a bright flash a few ticks in, and the shipped five
/// end before it lands. Seven, tried on the device, reaches it: the flash goes
/// into the framebuffer, and since the blur compounds, the applications after
/// it smear that white across the whole screen. That is the second 흰색 화면
/// report, and it is not the same cause as the first — the count of *blur*
/// applications went **down** (10 -> 7) in the same build, so the applications
/// were not it.
///
/// Raising it does make the scene crawl in finer steps, which is the only
/// reason it is a knob at all. It is not free.
pub fn boss_zoom_ticks() -> i64 {
    knob("CS_BOSS_ZOOM_TICKS", 1000).clamp(1, 5000) as i64
}

/// How many times the blur is applied across the effect.
///
/// **This is an intensity dial as much as a smoothness one**, because the blur
/// compounds — see `host_radial_blur`. Five is what the shipped effect did and
/// what its look is; ten is twice as many stages, so twice as smooth and about
/// twice as smeared. `CS_BOSS_ZOOM_STRENGTH` is the other half of that trade:
/// it scales the ray length, so halving it roughly cancels a doubled count.
///
/// Kept apart from the world's five ticks. The shipped effect paired one blur
/// with one tick, but the world's pace and the animation's smoothness are not
/// the same question once neither is tied to the frame rate.
pub fn boss_zoom_steps() -> i64 {
    knob("CS_BOSS_ZOOM_STEPS", 1000).clamp(1, 5000) as i64
}

/// Percentage applied to the ray length, on top of everything else. 100 leaves
/// each application exactly as the shipped code would draw it for its counter.
pub fn boss_zoom_strength() -> i32 {
    knob("CS_BOSS_ZOOM_STRENGTH", 50).clamp(10, 400)
}

/// Where the sweep starts, and how the time is spread across it.
///
/// Both exist because of what the effect actually *is*. It does not enlarge
/// anything — every output pixel is the mean of the pixels between it and the
/// screen centre, over a ray `radius * counter / diag` long — so the counter is
/// a **smear length**, and at the shipped 0x14 the corners are already smeared
/// half as far as they ever get. Shown for a fifth of a second nobody looks at
/// it; held for two seconds it reads as a mess rather than a rush.
///
/// Neither knob touches a pixel value. The range still ends where the guest's
/// `cmp #0x27` ends it, and every counter along the way produces exactly the
/// bytes the shipped code would for that counter. What changes is *when* each
/// one is on screen:
///
///   * `CS_BOSS_ZOOM_FROM` — the counter the sweep begins at. The shipped value
///     is 0x14, which starts with a jolt; a low value starts from an almost
///     clean screen and builds. Written over `mov r2, #0x14` at `+0x210`, so
///     the guest still arms, stores and ends the effect itself.
///   * `CS_BOSS_ZOOM_CURVE` — the exponent on elapsed time. 1 is a straight
///     ramp; 2 spends most of the duration in the low half and only touches the
///     hard smear at the very end.
pub fn boss_zoom_from() -> i32 {
    knob("CS_BOSS_ZOOM_FROM", 6).clamp(1, ZOOM_END - 1)
}

pub fn boss_zoom_curve() -> i32 {
    knob("CS_BOSS_ZOOM_CURVE", 2).clamp(1, 4)
}

/// Whether the world slows with the zoom. `CS_BOSS_ZOOM_SLOWMO=0` leaves
/// `GameFrame_Process` alone, and the scene runs at full speed under a stretched
/// overlay — which is what stretching the zoom alone gives.
pub fn boss_zoom_slowmo() -> bool {
    knob("CS_BOSS_ZOOM_SLOWMO", 1) != 0
}

/// Where to install [`Patch::BossZoom`], or `None` if the feature is off or the
/// guest is not the binary this was written against.
pub fn boss_zoom_site(base: u32, blur_wrapper: u32, mem: &mut GuestMem) -> Option<u32> {
    if boss_zoom_ms() <= 0 {
        return None;
    }
    let step = base.wrapping_add(ZOOM_STEP);
    let ok = mem.r32(step.wrapping_sub(4)) == ZOOM_LOAD_WORD
        && mem.r32(step) == ZOOM_STEP_WORD
        && mem.r32(step.wrapping_add(4)) == ZOOM_CMP_WORD
        && mem.r32(step.wrapping_add(8)) == ZOOM_STORE_WORD
        && mem.r32(base.wrapping_add(ZOOM_ARM)) == ZOOM_ARM_WORD;
    if !ok {
        return None;
    }
    // And that the counter being stepped is the one the blur reads.
    let bl_at = base.wrapping_add(ZOOM_BL);
    let word = mem.r32(bl_at);
    if word >> 24 != 0xeb {
        return None;
    }
    let off = ((word << 8) as i32 >> 8) as u32;
    (bl_at.wrapping_add(8).wrapping_add(off << 2) == blur_wrapper).then_some(step)
}

/// Where to install [`Patch::ZoomArm`] — the `mov r2, #0x14` a fresh effect is
/// armed with, and the one instruction per effect that runs *before* its first
/// blur. The word is already verified by [`boss_zoom_site`].
pub fn boss_zoom_arm_site(base: u32) -> u32 {
    base.wrapping_add(ZOOM_ARM)
}

/// The screen-space offset `Map_drawMiddleObject` draws objects at, or `None`
/// if its prologue is not the six loads this was read off.
fn map_draw_offset(draw: u32, map: u32, mem: &mut GuestMem) -> Option<(i32, i32)> {
    if MAP_DRAW_OFF.iter().any(|&(o, w)| mem.r32(draw.wrapping_add(o)) != w) {
        return None;
    }
    let mut at = |o: u32| mem.r32(map.wrapping_add(o)) as i32;
    Some((
        at(0x490).wrapping_add(at(0x150)).wrapping_sub(at(0x140)),
        at(0x494).wrapping_add(at(0x154)).wrapping_sub(at(0x144)),
    ))
}

/// Installed only when the D-pad nudge is asked for. Both entries move the
/// same thing — one the picture, one the touch target — and neither is much
/// use without the other.
pub const PATCHED_DPAD: [(&str, Patch); 2] =
    [("DM_DrawImgRotateEx", Patch::MenuArrow), ("Check_Rect", Patch::MenuArrowHit)];

/// The bottom-left "arrow" is a four-arrow virtual D-pad: one 27x16 sprite
/// (`getImage(*_imgGameUI, 6)`, alpha 0x78) blitted four times at 0/90/180/270
/// degrees. It is inlined — not a shared helper — into five screen draws, and
/// the constants are identical in all five: x = 0x20 / 0x47 / 0x20 / 4 and
/// y = H - 0x51 / 0x37 / 0x12 / 0x37, where `H = *(_lcdG+0x50)` is 240 in every
/// configuration. So at 400x240 the cluster sits 4 px from the left edge and
/// 2 px from the bottom.
///
/// These are the *return* addresses (call site + 4) of the eighteen
/// `bl DM_DrawImgRotateEx` that draw it, as offsets into each screen's draw
/// function. Enumerated by decoding every `bl 0x46950` in those functions and
/// keeping the ones whose sprite is frame 6 — hence exactly the four x values
/// above, and hence `Draw_TouchPad` (the in-game *world* pad, x = 53/107/53/10
/// plus a movable variant) is excluded: it is a different control that is not
/// flush against the edges.
const MENU_ARROW_SITES: [(&str, u32); 18] = [
    ("GameMenuFrame_Render", 0x360),
    ("GameMenuFrame_Render", 0x3ac),
    ("GameMenuFrame_Render", 0x3f8),
    ("GameMenuFrame_Render", 0x448),
    ("NpcUI_Render", 0xb4),
    ("NpcUI_Render", 0x100),
    ("NpcUI_Render", 0x14c),
    ("NpcUI_Render", 0x19c),
    ("MainMenuFrame_drawOption", 0x3fc),
    ("MainMenuFrame_drawOption", 0x448),
    ("MainMenuFrame_drawOption", 0x494),
    ("MainMenuFrame_drawOption", 0x4e4),
    ("MainMenuFrame_drawHelp", 0x1f8),
    ("MainMenuFrame_drawHelp", 0x244),
    ("GameFrame_drawWorldMap", 0xbf0),
    ("GameFrame_drawWorldMap", 0xc3c),
    ("GameFrame_drawWorldMap", 0xc88),
    ("GameFrame_drawWorldMap", 0xcd8),
];

/// The D-pad's four touch boxes, as `(rx, height - ry, w, h)` — the shape of the
/// four `Check_Rect` calls the menu key handlers make before any state dispatch.
///
/// They are *not* the sprite boxes: they tile the whole corner contiguously
/// (x 0..91, y 157..240 at H = 240), which is how this table was checked. That
/// tiling is also why they have to move with the sprites — the same quadruple is
/// copy-pasted into some twenty key handlers, so matching on the values fixes
/// every copy at once, whereas enumerating call sites would miss one and leave a
/// screen whose arrows are drawn in one place and tappable in another.
const MENU_ARROW_RECTS: [(i32, i32, i32, i32); 4] = [
    (0x1a, 0x53, 0x27, 0x17), // up
    (0x1a, 0x17, 0x27, 0x17), // down
    (0x00, 0x3c, 0x1a, 0x25), // left
    (0x41, 0x3c, 0x1a, 0x25), // right
];

/// [`MENU_ARROW_RECTS`] in the same order, as directions.
const MENU_ARROW_DIRS: [Dir; 4] = [Dir::Up, Dir::Down, Dir::Left, Dir::Right];

/// The 16 key handlers the D-pad quadruple is copy-pasted into. Matching the
/// rect *values* is what finds all 69 copies, but the drawn cluster is now
/// resized as well as moved, so the hit test is pinned to these callers too:
/// value and caller, so a guest whose rects happened to collide degrades to
/// "no move" rather than to a rect that moves without its sprite.
const MENU_ARROW_KEYS: [&str; 16] = [
    "GameFrame_worldMapKeyPressed",
    "IGM_moveIconMtx",
    "GameMenuFrame_simpleBagKeyOnCube",
    "Compose_RecipeInputNumKey",
    "GameMenuCharPetKey",
    "Compose_RecipeKey",
    "GameMenuFrame_KeyPressed",
    "GameMenuQuest_KeyPressed",
    "ItemBagKey",
    "MainMenu_HelpKey",
    "MainMenuOptionKey",
    "NpcUI_tableKeyPressed",
    "NpcUI_skillTableKeyPressed",
    "NpcUI_skillMasterKeyPressed",
    "NpcUI_blackSmithKeyPressed",
    "NpcUI_merchantKeyPressed",
];

/// `Compose_RecipeKey+0x864` — the 72x16 button at `(0x50, H - 0x10)` on the
/// cube's compose screen, and the **only** thing the cluster can reach when it
/// moves off its corner. The margins above are chosen against it, so the words
/// that build it are pinned by
/// `the_margins_clear_the_one_button_the_cluster_can_reach`.
#[cfg(test)]
const COMPOSE_BTN: u32 = 0x864;
#[cfg(test)]
const COMPOSE_BTN_WORDS: [(u32, u32); 5] = [
    (0x83c, 0xe3a0_2050), // mov r2, #0x50        x
    (0x844, 0xe593_3050), // ldr r3, [r3, #0x50]  H
    (0x848, 0xe243_3010), // sub r3, r3, #0x10    y = H - 0x10
    (0x84c, 0xe3a0_c048), // mov ip, #0x48        w  -> [sp]
    (0x858, 0xe3a0_c010), // mov ip, #0x10        h  -> [sp+4]
];
#[cfg(test)]
const COMPOSE_BTN_RECT: (i32, i32, i32, i32) = (0x50, 0x10, 0x48, 0x10);

/// The cluster's own margins at 1:1: its sprites span x 4..87 and y 159..238 at
/// H = 240, so 4 px from the left edge and 2 px from the bottom. The transform
/// below scales about that box's bottom-left corner and then puts the corner at
/// (`dx`, `H - dy`), which is what makes `dx`/`dy` the *resulting* margins at
/// any scale rather than a nudge applied on top of the shipped ones.
const DPAD_LEFT: i32 = 4;
const DPAD_BOTTOM: i32 = 2;

/// Where `DM_DrawImgRotateEx`'s scale pair sits in the caller's outgoing stack
/// block, as read at the entry (before its own `push`). The full signature is
/// `(g, x, y, w, [+0]=h, [+4]=img, [+8]=sx, [+0xc]=sy, [+0x10]=rot,
/// [+0x14]=angle, [+0x18]=scaleX%, [+0x1c]=scaleY%, [+0x20]=level,
/// [+0x24]=alpha, [+0x28]=anchor)`, and the scales are integer percent —
/// `sw = max(1, w * scaleX / 100)` in `DM_DrawRotate565Ex`, which
/// `host_rotate_blit` already reproduces.
const DPAD_SCALE_X: u32 = 0x18;
const DPAD_SCALE_Y: u32 = 0x1c;

/// Screen height in `_lcdG`. Always 240 here, but the guest reads it rather
/// than assuming, and so does this.
const LCD_H: u32 = 0x50;

/// Screen width in `_lcdG`, the field right before it.
const LCD_W: u32 = 0x4c;

/// Margins to leave between the D-pad cluster and the two screen edges it is
/// flush against, in logical pixels, and how big to draw it as a percentage.
/// All three at their shipped values (`4, 2, 100`) leave it alone and the
/// patches are not installed at all.
///
/// Read from the environment *and* baked in at build time, since the device has
/// no environment to set — same as `strike_y`.
///
/// **These shipped as 4 and 4, which is not a margin.** `DPAD_LEFT` is 4, so
/// `dx = 4` moved the cluster sideways by exactly nothing, and `dy = 4` lifted
/// it two pixels — the player's report was that it is "stuck to the bottom-left
/// corner again", and measuring the probe's in-game menu agreed: the two builds
/// differ only in that the cluster is 25 % bigger, in the same corner.
///
/// **`dy` is what buys `dx`.** The one thing in reach of the cluster is
/// `Compose_RecipeKey+0x864`, the 72x16 button at `(0x50, H - 0x10)` on the
/// cube's compose screen, and the down arrow shares its rows. At 125 % the
/// arrow's right edge is `dx + 75`, so `dx` alone cannot pass 5 before it eats
/// that button — the same collision that capped the scale at 125 %. Lift the
/// cluster until the arrow clears `y = 224` (`dy >= 18` at this scale) and the
/// horizontal room opens up. 12/20 and 20/24 are both clean; see
/// `tools_re/dpad_rects.py`, which recovers every `Check_Rect` rectangle in the
/// sixteen handlers and intersects them against the transformed cluster.
pub fn dpad_dx() -> i32 {
    knob("CS_DPAD_DX", 40)
}

pub fn dpad_dy() -> i32 {
    knob("CS_DPAD_DY", 26)
}

/// 인게임 3x3 아이콘 메뉴의 왼쪽 열이 **답하기 시작하는** 논리 x, `W/2 - 65`.
///
/// 그려진 테두리보다 한 칸 오른쪽이다(테두리는 `W/2 - 67`). 실측으로 갈랐다 —
/// 400폭에서 x=134 는 커서를 안 옮기고 135 는 옮긴다.
const MENU_GRID_LEFT: i32 = 65;

/// 화면 폭이 허락하는 만큼만 민 왼쪽 여백.
///
/// **`dx` 는 화면 폭 없이는 고를 수 없다.** 3x3 메뉴는 가운데 정렬이라 왼쪽 열이
/// `W/2 - 65` 에서 시작하고, D-pad 사각형은 그 화면에서 **가장 먼저** 검사되므로
/// 묶음이 거기까지 닿으면 그만큼 메뉴 왼쪽 열의 탭을 가로챈다. 실측:
/// `dx = 40`, 400폭에서 잠김 칸의 왼쪽 8px 을 누르면 그 칸이 **선택되고 곧바로
/// 오른쪽으로 한 칸 더 간다**(캐릭터 → 잠김 → 큐브).
///
/// 그래서 노브 값은 **희망값**이고 여기서 화면에 맞춰 깎는다 — 기기(520폭)는
/// 40 을 그대로 쓰고, 프로브의 400폭은 32 로 내려온다. 4:3(320폭)처럼 애초에
/// 자리가 없는 화면은 출고 여백([`DPAD_LEFT`])으로 주저앉는다.
///
/// `Check_Rect` 는 **양 끝을 포함한다**(`rx <= px <= rx+w`, 0xaa370 의
/// `cmp/cmpge/ble`). 그래서 묶음의 오른쪽 끝이 `W/2 - 65` 와 같기만 해도 겹친다.
fn dpad_dx_for(w: i32, s: i32) -> i32 {
    // `dpad_plate` 이 쓰는 그 산술이어야 한다 — 먼저 곱하고 나중에 더하면
    // 반올림이 한 픽셀 다르고, 그 한 픽셀이 곧 이 함수가 막는 것이다.
    let span = 2 * (DPAD_ARM * s / 100) + dpad_thick() * s / 100;
    let room = w / 2 - MENU_GRID_LEFT - 1 - span;
    dpad_dx().min(room.max(DPAD_LEFT))
}

/// 125 and not more, because the hit boxes grow with the sprites and the
/// D-pad's quadruple is tested *before* everything else on its screen. Resolving
/// every `Check_Rect` in the sixteen handlers and intersecting them against the
/// scaled cluster puts the first collision at 130%: `Compose_RecipeKey+0x864`,
/// the 72x16 button at (0x50, H-0x10) on the compounding screen, whose left
/// 15 px the down arrow would swallow. 125% is the largest scale that steals
/// nothing — and it is still 56% more area to hit.
///
/// Higher values work and look fine; they just cost that button. Clamped at 250
/// regardless, since the anchor is the bottom-left corner and the cluster grows
/// up and right into the rest of the UI.
pub fn dpad_scale() -> i32 {
    knob("CS_DPAD_SCALE", 125).clamp(50, 250)
}

/// 새 D-pad: 육각 플레이트 넷에 흰 삼각형. `0` 이면 출고 스프라이트를
/// 옮기고 키우기만 하던 옛 동작이다(그래서 A/B 노브다).
pub fn dpad_hex() -> bool {
    knob("CS_DPAD_HEX", 1) != 0
}

/// 클러스터의 기하, 배율 100 기준 논리 픽셀.
///
/// 중심에서 플레이트 중심까지가 [`DPAD_ARM`] 이고, **네 판은 한 모양의
/// 회전이다** — 좌/우 판이 `SHORT x LONG` 에 좌우가 뾰족하고, 위/아래 판은
/// 그것을 90° 돌린 `LONG x SHORT` 에 위아래가 뾰족하다. 상자가 **가리키는
/// 방향을 가로지르는 쪽이 길다**는 관례는 그대로다 — 출고 메뉴 히트 상자
/// (39x23 과 26x37)도, 출고 월드 패드(57x38 과 38x57)도 그렇고, 뒤엣것은
/// 아예 서로의 90° 회전이다.
///
/// 회전이라 묶음은 `2*ARM + SHORT` **정사각**이다. 기본 배율 125%에서
/// 102x102 — 출고 묶음(91x91)과 같은 자리다.
const DPAD_ARM: i32 = 27;
const DPAD_LONG: i32 = 38;
const DPAD_SHORT: i32 = 23;

/// 판의 **두께** — 위/아래 판의 높이이자 좌/우 판의 폭, 배율 100 기준.
///
/// 제보: 「메뉴 화살표 버튼의 높이를 줄여 줘. 너무 버튼이 넓어.」 29 였을 때
/// 125%에서 위/아래가 47x36 이라 네 판이 대각선에서 서로 겹쳐 한 덩어리로
/// 보였다. 23 이면 47x28 이고 모서리만 3px 스친다 — **출고 메뉴 히트 상자의
/// 두께 그대로**다(위/아래 39x23). 길이(`DPAD_LONG`)는 안 건드리므로 네 판이
/// 한 모양의 회전인 것도 그대로다.
///
/// 묶음이 `2*ARM + 두께` 정사각이므로 이 값이 곧 묶음 크기이기도 하다 —
/// 125%에서 102 -> **94**. 가로 span 이 줄어 `dpad_dx_for` 의 여유는 넓어진다.
pub fn dpad_thick() -> i32 {
    knob("CS_DPAD_THICK", DPAD_SHORT).clamp(10, 40)
}

/// 육각형의 평평한 두 변이 마주 보는 지름의 몇 분의 몇인가. 작을수록 뾰족하다.
///
/// **넷이 한 값을 나눠 쓴다.** 값이 둘이던 판(눕힌 판 7/10, 세운 판 1/2)은
/// 참고 스크린샷을 잰 수치를 그대로 옮긴 것이었는데, 그러면 위/아래 판이
/// 좌/우 판의 회전이 아니라 **다른 모양**이 된다 — 제보가 그것이다.
const DPAD_FLAT: (i32, i32) = (1, 2);

/// 채움은 **검정 그라데이션**이고 화살표는 없다 — 방향은 자리가 말한다.
/// 위가 진하고 아래로 갈수록 옅어진다. 테두리만 옅게 밝은 선인데, 그게 없으면
/// 어두운 맵 위에서 육각형의 모양 자체가 안 보인다.
const DPAD_ALPHA_TOP: i32 = 0xb4;
const DPAD_ALPHA_BOTTOM: i32 = 0x38;
const DPAD_FILL: u16 = 0x0000;
const DPAD_RIM: (u16, u32) = (0xd6de, 0x7c);

/// 출고 블록이 네 화살표를 놓는 자리, `(x, H - y, 방향)`.
///
/// 한 프레임 안에서 같은 스프라이트를 네 번 그리므로 **좌표가 곧 방향**이다.
/// 넷 중 어느 것과도 안 맞으면 우리가 아는 그림이 아니므로 손대지 않고
/// 게스트에게 넘긴다.
const DPAD_SPRITES: [(i32, i32, Dir); 4] = [
    (0x20, 0x51, Dir::Up),
    (0x47, 0x37, Dir::Right),
    (0x20, 0x12, Dir::Down),
    (0x04, 0x37, Dir::Left),
];

/// 터치방식 **B**(환경설정 「터치방식」 = TYPE B)의 월드 고정 패드.
///
/// 메뉴의 D-pad 와 완전히 다른 컨트롤이다 — `Draw_TouchPad` 가 그리고
/// `GameFrame_PressTouchKeypad` 가 잰다 — 그런데 플레이어에게는 같은 패드여야
/// 하므로 **같은 판을 같은 자리에** 띄운다.
///
/// 네 방향이 각각 자기 호출 지점을 갖는다(실측: `x` 가 0x35/0x6b/0x35/0x0a,
/// `y` 가 112/155/209/155). 그래서 좌표를 맞출 필요 없이 복귀 주소가 곧 방향이다.
const WORLD_PAD_DRAW: [(u32, Dir); 4] =
    [(0x698, Dir::Up), (0x6e4, Dir::Right), (0x730, Dir::Down), (0x784, Dir::Left)];

/// 월드 패드의 히트 상자만 판보다 이만큼 사방으로 넓다(배율 100 기준).
///
/// **자리도 간격도 메뉴 패드와 똑같아야 한다**(제보) — 그래서 월드 판은
/// `dpad_plate` 가 그대로 낸다. 그런데 걷기는 누르는 것이 아니라 **쥐고 미는
/// 것**이라, 판이 곧 상자이면 손가락이 조금만 흔들려도 release 가 나가 걷기가
/// 끊긴다. 그 하나만 보이지 않게 메워 둔다 — 중심도 간격도 안 움직이고
/// 넓어지는 것은 판정뿐이다.
///
/// 6 이면 125%에서 사방 7px, 상자가 61x42 / 42x61 로 출고(57x38 / 38x57)와
/// 비슷해진다. 가운데에는 24x24 의 중립 지대가 남는다 —
/// `slop < arm - thick/2` 인 동안은 네 상자가 중심을 안 덮는다.
///
/// 메뉴 쪽에는 안 준다. 거기 탭은 한 번씩 누르는 것이라 정확한 편이 낫고,
/// 넓히면 3x3 메뉴 상자와 큐브 합성 버튼에 닿는다.
pub fn dpad_slop() -> i32 {
    knob("CS_DPAD_SLOP", 6).clamp(0, 16)
}

/// 그 패드의 히트 사각형, `(호출 오프셋, (rx, H - ry, w, h), 방향)`.
///
/// 메뉴 쪽과 크기가 겹치지 않는다(57x38 / 38x57 대 39x23 / 26x37)라 값만으로도
/// 갈리지만, 여기서는 호출 지점이 방향까지 말해 주므로 둘 다 본다.
const WORLD_PAD_HIT: [(u32, (i32, i32, i32, i32), Dir); 4] = [
    (0x268, (38, 138, 57, 38), Dir::Up),
    (0x294, (95, 100, 38, 57), Dir::Right),
    (0x2b8, (0, 100, 38, 57), Dir::Left),
    (0x2e8, (38, 43, 57, 38), Dir::Down),
];

/// 네 방향. [`MENU_ARROW_RECTS`] 와 같은 순서여야 한다 — 히트 상자와 그림이
/// 같은 칸을 가리키는 것이 이 패치의 전부다.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

/// 인게임 월드 조이스틱을 hero5 의 것으로 (`CS_JOY_HERO5`, 기본 켜짐).
///
/// 제보 그대로다 — 「인게임을 hero5 의 조이스틱 방식과 동일한 UI 로. 위치와
/// 크기 전부 hero5 와 동일하게, 고정/자유 방식 둘 다.」 메뉴 D-pad 는 그대로
/// 두라고 했으므로 이 노브는 **월드 패드만** 바꾼다. 아트·기하·출처는
/// `joy.rs` 머리 주석에 있다(hero5 프레임버퍼에서 알파 복원, 평균 오차
/// 1.1/255).
///
/// 켜면:
/// * **TYPE A(자유)** — 게스트의 부동 조이스틱 그리기(`Draw_TouchPad` 의
///   팔 4 + 하이라이트 4)를 hero5 스틱 하나로 바꾼다. 자리는 게스트 자신의
///   앵커(`mainFrame+0x80/0x84` — 그 draw 호출의 인자에 그대로 실려 온다),
///   노브는 게스트 자신의 현재 방향(`mainFrame+0x6c`, WIPI −1..−4)이다.
///   idle 에 아무것도 안 그리는 것은 게스트도 hero5 도 같다.
/// * **TYPE B(고정)** — 고정 패드 화살표 넷 대신 hero5 스틱을 hero5 의 고정
///   앵커 (65, H−65) 에 그린다. 방향이 잡혀 있으면 밝은 판 + 노브 디플렉션,
///   아니면 hero5 의 반투명 idle 판이다. 히트도 사각형 넷 대신 **hero5 의
///   판정 그대로**(우세축 + 데드존 10 + 도달 반경) — `Check_Rect` 넷을
///   그 답으로 서브한다. 게스트의 `Check_Rect` 는 히트에 진동도 겸하는데
///   이 빌드에 진동은 아직 없으므로(「아직 없는 것」) 잃는 것이 없다.
///
/// 끄면 옛 동작 그대로다 — TYPE B 는 육각 플레이트(`CS_DPAD_HEX`), TYPE A 는
/// 게스트의 팔 네 개.
pub fn joy_hero5() -> bool {
    knob("CS_JOY_HERO5", 1) != 0
}

/// TYPE B 히트의 도달 반경(체비셰프, 논리 px). hero5 의 고정 모드는 걷기
/// 영역 전체가 그랩이지만, 크로노스의 `GameFrame_PressTouchKeypad` 는 이
/// 검사 넷을 스킬 버튼들보다 **먼저** 돌리므로 무한정 넓히면 그쪽을 가린다.
/// 60 이면 앵커 (65,175) 기준 x ≤ 125 — 스킬 버튼 열(x ≥ 241)과 상단 바
/// (y ≤ 12)에 안 닿고, 판(±36)보다는 손가락 하나만큼 너그럽다.
pub fn joy_reach() -> i32 {
    knob("CS_JOY_REACH", 60).clamp(20, 120)
}

/// `Draw_TouchPad` 안의 조이스틱 draw 호출들, `bl` 의 **복귀 주소** 오프셋.
///
/// 고정 패드(TYPE B) 넷과 부동 패드(TYPE A)의 팔 넷 + 방향 하이라이트 넷.
/// 각 묶음의 **첫** 호출에서 hero5 스틱 전체를 그리고 나머지는 그냥
/// 건너뛴다 — 한 프레임에 스틱은 하나다. 스킵 목록에 하이라이트가 들어
/// 있는 것이 곧 「방향 표시」를 노브가 대신한다는 뜻이다.
///
/// 같은 함수의 나머지 draw 호출 12개(OK 버튼·퀵슬롯 등, `+0x180..+0x570`)와
/// 터치방식 2(UI 도달 불가)의 것들은 건드리지 않는다.
const JOY_FIXED_FIRST: u32 = 0x698;
const JOY_FLOAT_FIRST: u32 = 0x808;
const JOY_SKIP_DRAWS: [u32; 10] = [
    0x6e4, 0x730, 0x784, // 고정 패드의 나머지 화살표 셋
    0x85c, 0x8b0, 0x908, // 부동 패드의 나머지 팔 셋
    0x998, 0xab0, 0xb20, 0xb78, // 방향 하이라이트 넷 (−3/−2/−4/−1)
];

/// 설치 전 확인하는 `Draw_TouchPad` 안의 명령 워드들. 이 기능이 게스트에서
/// 읽는 값 하나하나의 근거를 고정한다:
///
/// * `+0x400` `ldrb r3,[r5,#0x88]` — 부동 패드가 **앵커됨 플래그**로
///   갈린다(r5 = `mainFrame`). 즉 부동 draw 사이트는 앵커가 서 있을 때만
///   돈다.
/// * `+0x66c` `mov r1,#0x35` — 고정 패드 첫 화살표의 x. 이 워드가 그대로면
///   고정 사이트의 인자 배치가 출고 그대로라는 뜻이다.
/// * `+0x7b4/+0x7c0/+0x7d0` `ldr r2,[r5,#0x84]` / `ldr r1,[r5,#0x80]` /
///   `sub r2,r2,#0x28` — 부동 첫 팔의 인자가 **앵커 그 자체**(x)와
///   앵커−0x28(y)이라는 근거. 그래서 훅은 앵커를 게스트 메모리가 아니라
///   **그 호출의 인자에서** 읽는다.
/// * `+0x908/+0x90c` `ldr r3,[r5,#0x6c]` / `cmn r3,#1` — 게스트 자신이
///   `mainFrame+0x6c` 를 현재 방향(WIPI −1..−4)으로 읽어 하이라이트를
///   고르는 그 자리. 노브가 같은 값을 읽는 근거다.
const JOY_WORDS: [(u32, u32); 7] = [
    (0x400, 0xe5d5_3088),
    (0x66c, 0xe3a0_1035),
    (0x7b4, 0xe595_2084),
    (0x7c0, 0xe595_1080),
    (0x7d0, 0xe242_2028),
    (0x908, 0xe595_306c),
    (0x90c, 0xe373_0001),
];

/// `mainFrame` 의 필드들 — `Draw_TouchPad` 가 자기 코드로 읽는 그것.
/// +0x6c 현재 방향(s32, WIPI −1..−4, 없으면 −999), +0x80/+0x84 부동 앵커.
const JOY_MF_DIR: u32 = 0x6c;

/// `mainFrame+0x70` — **직전 방향**. `onHandleEvent` 의 TYPE B MOVE 경로가
/// 자기 팔 꼬리에서 유지하고(`str dir,[r7,#0x70]`), 방향이 안 바뀐 이동의
/// 재-press 를 건너뛰는 비교(`ldr r3,[r7,#0x70]` / `cmp r3,r5`)에 쓴다.
/// 그 경로는 rect 를 재기 **전에** `+0x6c` 를 −999 로 미리 지우므로
/// (`str fp,[r7,#0x6c]`, +0x4c4), 이동 시점의 「쥐고 있던 방향」은 이 워드가
/// 유일한 진실이다. 실측 셋: press 만으로는 −999(press 경로는 안 세운다),
/// 이동 팔이 지나가면 그 방향, **release 처리가 +0x6c 와 함께 −999 로
/// 지운다**(스테일 없음 — press+move+release 뒤 peek 로 확인).
const JOY_MF_PREV: u32 = 0x70;

/// `onHandleEvent` 의 TYPE B **MOVE** 경로가 같은 월드 패드 사각형 넷을 자기
/// `bl Check_Rect` 사본으로 다시 잰다 — `GameFrame_PressTouchKeypad` 는
/// PRESS 만 받고(`GameFrame_KeyPressed` 가 타입 0x13/2 만 넘긴다), 드래그와
/// 쥔 손가락의 미세 이동은 전부 이쪽이다. **처음 이 넷을 빼먹은 것이 기기의
/// 「이동이 뚝뚝 끊긴다」였다**: press 는 hero5 판정으로 잡았는데 move 는
/// 출고 사각형으로 재니, 새 스틱의 중앙(= 출고 패드의 가운데 빈틈)에 얹힌
/// 엄지의 첫 흔들림마다 미스-전부 → release 가 나갔다.
///
/// 형식은 [`WORLD_PAD_HIT`] 그대로 `(복귀 오프셋, (rx, H−ry, w, h), 방향)`.
/// 사각형 값도 그쪽과 동일하다 — LEFT 의 rx=0 은 리터럴이 아니라 직전 미스의
/// `subs r2,r0,#0` 이 남긴 0 이고, 서브가 `PC := LR` 로 돌아가면 그 `subs` 는
/// 게스트가 그대로 실행하므로 값이 유지된다.
const WORLD_PAD_MOVE_HIT: [(u32, (i32, i32, i32, i32), Dir); 4] = [
    (0x4e8, (38, 138, 57, 38), Dir::Up),
    (0x664, (95, 100, 38, 57), Dir::Right),
    (0x7e8, (0, 100, 38, 57), Dir::Left),
    (0x8c8, (38, 43, 57, 38), Dir::Down),
];

/// MOVE 경로에서 설치 전 확인하는 명령 워드들(`onHandleEvent` 기준 오프셋).
/// `+0x4c4` 가 rect 검사 전의 `+0x6c` 프리클리어, `+0x4f0`/`+0x534` 가
/// 「+0x70 = 직전 방향」의 근거(팔의 비교와 꼬리 저장), `+0x8d4` 가 DOWN 팔의
/// 방향 상수(`mvn r2,#1` = −2)다.
const JOY_MOVE_WORDS: [(u32, u32); 4] = [
    (0x4c4, 0xe587_b06c),
    (0x4f0, 0xe597_3070),
    (0x534, 0xe583_2070),
    (0x8d4, 0xe3e0_2001),
];

/// 메뉴 D-pad 한 방향의 플레이트 상자, 화면 좌표로 `(x, y, w, h)`.
fn dpad_plate(dir: Dir, h: i32, s: i32, dx: i32, dy: i32) -> (i32, i32, i32, i32) {
    let arm = DPAD_ARM * s / 100;
    let (long, short) = (DPAD_LONG * s / 100, dpad_thick() * s / 100);
    // 위/아래 판이 좌/우 판의 90° 회전이라 묶음은 정사각이다.
    let span = 2 * arm + short;
    // 가로는 **왼쪽** 끝을, 세로는 **아래** 끝을 여백에 맞춘다. `span` 이
    // 홀수면 두 반쪽이 한 픽셀 다르므로 각 축이 자기 쪽 반쪽을 써야 한다 —
    // 둘 다 `span / 2` 로 두면 묶음이 아래로 1px 삐져나가고, 그 1px 이
    // `dpad_dx_for` 이 세는 그 1px 이다.
    let (cx, cy) = (dx + span / 2, h - dy - (span - span / 2));
    let (ox, oy, pw, ph) = match dir {
        Dir::Up => (0, -arm, long, short),
        Dir::Down => (0, arm, long, short),
        Dir::Left => (-arm, 0, short, long),
        Dir::Right => (arm, 0, short, long),
    };
    (cx + ox - pw / 2, cy + oy - ph / 2, pw, ph)
}

/// Whether anything about the cluster differs from what the guest ships.
pub fn dpad_changed() -> bool {
    dpad_hex() || dpad_scale() != 100 || dpad_dx() != DPAD_LEFT || dpad_dy() != DPAD_BOTTOM
}

/// Scale a screen coordinate about the cluster's bottom-left corner and put
/// that corner at (`dx`, `H - dy`). Used for the sprites and, unchanged, for
/// the hit rects — the two boxes are concentric at 1:1 (both centre on
/// (45.5, 198.5) at H = 240) and this keeps them so.
fn dpad_x(x: i32, s: i32, dx: i32) -> i32 {
    dx + (x - DPAD_LEFT) * s / 100
}

fn dpad_y(y: i32, h: i32, s: i32, dy: i32) -> i32 {
    h - dy - (h - y - DPAD_BOTTOM) * s / 100
}

/// Resolve [`MENU_ARROW_SITES`] against the loaded guest. A site whose symbol is
/// missing is dropped rather than guessed at, and a guest that resolves none
/// leaves the patches uninstalled.
pub fn menu_arrow_lrs(symbol: impl Fn(&str) -> Option<u32>) -> Vec<u32> {
    MENU_ARROW_SITES
        .iter()
        .filter_map(|(sym, off)| symbol(sym).map(|a| a.wrapping_add(*off)))
        .collect()
}

/// `[start, end)` of each menu key handler, `end` taken from the symbol's own
/// `st_size`.
///
/// This used to be "the next symbol along", on the assumption that the next
/// symbol after a function is the next function. That is false exactly once in
/// this binary, and it cost the D-pad four of its hit boxes: the linker's
/// `_stack` is defined at `+0x80000`, which is 0x21c0 bytes *inside*
/// `GameMenuFrame_KeyPressed` (`+0x77e40`, 0xa730 long). The window stopped
/// there, so every `Check_Rect` in the function's tail was declined —
/// including the whole quadruple at `+0xa630` / `+0xa668` / `+0xa69c` /
/// `+0xa6d4`, which is the one the cube's item-compose popup tests. The
/// sprites still moved and grew, because those are matched on the *draw*
/// return address in `GameMenuFrame_Render` and that is nowhere near the
/// truncation, so on that screen alone the arrows were drawn at 125% in the
/// new corner while taps were still judged against the shipped boxes.
///
/// `st_size == 0` (hand-written asm, none of these sixteen) falls back to the
/// old next-symbol scan rather than dropping the handler.
pub fn menu_arrow_key_ranges(elf: &crate::elf::Elf) -> Vec<(u32, u32)> {
    let mut addrs: Vec<u32> = elf.symbols.values().copied().collect();
    addrs.sort_unstable();
    let mut v: Vec<(u32, u32)> = MENU_ARROW_KEYS
        .iter()
        .filter_map(|s| elf.symbols.get(*s).map(|&a| (*s, a)))
        .map(|(s, a)| {
            let end = match elf.sizes.get(s) {
                Some(&size) => a.wrapping_add(size),
                None => match addrs.binary_search(&a) {
                    Ok(i) | Err(i) => {
                        addrs[i..].iter().copied().find(|&v| v > a).unwrap_or(u32::MAX)
                    }
                },
            };
            (a, end)
        })
        .collect();
    v.sort_unstable();
    v
}

pub(crate) fn knob(name: &'static str, default: i32) -> i32 {
    // Not memoised behind a `OnceLock` keyed on the name — these are read once
    // per install and once per matched call, and a matched call is rare.
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| match name {
            "CS_WATCH_MEM" => option_env!("CS_WATCH_MEM").and_then(|v| v.parse().ok()),
            "CS_CAVE" => option_env!("CS_CAVE").and_then(|v| v.parse().ok()),
            "CS_BAG10" => option_env!("CS_BAG10").and_then(|v| v.parse().ok()),
            "CS_SKIN" => option_env!("CS_SKIN").and_then(|v| v.parse().ok()),
            "CS_SKIN_CELLS" => option_env!("CS_SKIN_CELLS").and_then(|v| v.parse().ok()),
            "CS_OPTIONS" => option_env!("CS_OPTIONS").and_then(|v| v.parse().ok()),
            "CS_AUTOATK" => option_env!("CS_AUTOATK").and_then(|v| v.parse().ok()),
            "CS_BOX_DEBUG" => option_env!("CS_BOX_DEBUG").and_then(|v| v.parse().ok()),
            "CS_CHEST_BREAK" => option_env!("CS_CHEST_BREAK").and_then(|v| v.parse().ok()),
            "CS_DPAD_DX" => option_env!("CS_DPAD_DX").and_then(|v| v.parse().ok()),
            "CS_DPAD_HEX" => option_env!("CS_DPAD_HEX").and_then(|v| v.parse().ok()),
            "CS_JOY_HERO5" => option_env!("CS_JOY_HERO5").and_then(|v| v.parse().ok()),
            "CS_JOY_REACH" => option_env!("CS_JOY_REACH").and_then(|v| v.parse().ok()),
            "CS_DPAD_SLOP" => option_env!("CS_DPAD_SLOP").and_then(|v| v.parse().ok()),
            "CS_DPAD_THICK" => option_env!("CS_DPAD_THICK").and_then(|v| v.parse().ok()),
            "CS_DPAD_DY" => option_env!("CS_DPAD_DY").and_then(|v| v.parse().ok()),
            "CS_DPAD_SCALE" => option_env!("CS_DPAD_SCALE").and_then(|v| v.parse().ok()),
            "CS_BAG_RIGHT" => option_env!("CS_BAG_RIGHT").and_then(|v| v.parse().ok()),
            "CS_AUTOLOOT_DUP" => option_env!("CS_AUTOLOOT_DUP").and_then(|v| v.parse().ok()),
            "CS_ALWAYS_SKIP" => option_env!("CS_ALWAYS_SKIP").and_then(|v| v.parse().ok()),
            "CS_CAST_FIX" => option_env!("CS_CAST_FIX").and_then(|v| v.parse().ok()),
            "CS_EQUIP_FIX" => option_env!("CS_EQUIP_FIX").and_then(|v| v.parse().ok()),
            "CS_SKIP_WATCHDOG" => option_env!("CS_SKIP_WATCHDOG").and_then(|v| v.parse().ok()),
            "CS_PAD_MODE" => option_env!("CS_PAD_MODE").and_then(|v| v.parse().ok()),
            "CS_PAD_MIGRATE" => option_env!("CS_PAD_MIGRATE").and_then(|v| v.parse().ok()),
            "CS_OPT_DEBUG" => option_env!("CS_OPT_DEBUG").and_then(|v| v.parse().ok()),
            "CS_NET_EQUIP" => option_env!("CS_NET_EQUIP").and_then(|v| v.parse().ok()),
            "CS_NET_DROP" => option_env!("CS_NET_DROP").and_then(|v| v.parse().ok()),
            "CS_NET_DROP_RATE" => option_env!("CS_NET_DROP_RATE").and_then(|v| v.parse().ok()),
            "CS_EXP_GAP" => option_env!("CS_EXP_GAP").and_then(|v| v.parse().ok()),
            "CS_BOSS_ZOOM_MS" => option_env!("CS_BOSS_ZOOM_MS").and_then(|v| v.parse().ok()),
            "CS_BOSS_ZOOM_SLOWMO" => {
                option_env!("CS_BOSS_ZOOM_SLOWMO").and_then(|v| v.parse().ok())
            }
            "CS_BOSS_ZOOM_FROM" => option_env!("CS_BOSS_ZOOM_FROM").and_then(|v| v.parse().ok()),
            "CS_BOSS_ZOOM_CURVE" => option_env!("CS_BOSS_ZOOM_CURVE").and_then(|v| v.parse().ok()),
            "CS_BOSS_ZOOM_R" => option_env!("CS_BOSS_ZOOM_R").and_then(|v| v.parse().ok()),
            "CS_BOSS_ZOOM_STEPS" => option_env!("CS_BOSS_ZOOM_STEPS").and_then(|v| v.parse().ok()),
            "CS_BOSS_ZOOM_TICKS" => option_env!("CS_BOSS_ZOOM_TICKS").and_then(|v| v.parse().ok()),
            "CS_BOSS_ZOOM_STRENGTH" => {
                option_env!("CS_BOSS_ZOOM_STRENGTH").and_then(|v| v.parse().ok())
            }
            "CS_IME_H" => option_env!("CS_IME_H").and_then(|v| v.parse().ok()),
            "CS_NAME_BACKSPACE" => option_env!("CS_NAME_BACKSPACE").and_then(|v| v.parse().ok()),
            "CS_NAME_OK" => option_env!("CS_NAME_OK").and_then(|v| v.parse().ok()),
            "CS_IME_DEBUG" => option_env!("CS_IME_DEBUG").and_then(|v| v.parse().ok()),
            // These three shipped with **no arm at all** — read through `knob()`
            // but absent from this list, so the compiled default always won and
            // `build_android.ps1 CS_PHONE=0` did nothing on a device. That is
            // the third time this hand-maintained list has silently lost an
            // entry (`CS_IME_H`/`CS_IME_DEBUG` were the second), which is why
            // `every_knob_we_read_is_also_baked_in` now walks the source.
            "CS_PHONE" => option_env!("CS_PHONE").and_then(|v| v.parse().ok()),
            "CS_DATA_MANAGE" => option_env!("CS_DATA_MANAGE").and_then(|v| v.parse().ok()),
            "CS_SAVEXFER" => option_env!("CS_SAVEXFER").and_then(|v| v.parse().ok()),
            "CS_SMS_POPUP" => option_env!("CS_SMS_POPUP").and_then(|v| v.parse().ok()),
            "CS_MY_ID" => option_env!("CS_MY_ID").and_then(|v| v.parse().ok()),
            "CS_ID_AS_PHONE" => option_env!("CS_ID_AS_PHONE").and_then(|v| v.parse().ok()),
            "CS_ID_RECIPIENT" => option_env!("CS_ID_RECIPIENT").and_then(|v| v.parse().ok()),
            "CS_HOST_SLEEP" => option_env!("CS_HOST_SLEEP").and_then(|v| v.parse().ok()),
            "CS_LAZY_GL" => option_env!("CS_LAZY_GL").and_then(|v| v.parse().ok()),
            "CS_PAD_HUD" => option_env!("CS_PAD_HUD").and_then(|v| v.parse().ok()),
            "CS_RECIPE2" => option_env!("CS_RECIPE2").and_then(|v| v.parse().ok()),
            "CS_PET_SMART" => option_env!("CS_PET_SMART").and_then(|v| v.parse().ok()),
            "CS_PET_SMART_COMBAT" => {
                option_env!("CS_PET_SMART_COMBAT").and_then(|v| v.parse().ok())
            }
            "CS_RECIPE2_PRICE" => option_env!("CS_RECIPE2_PRICE").and_then(|v| v.parse().ok()),
            "CS_VAULT" => option_env!("CS_VAULT").and_then(|v| v.parse().ok()),
            "CS_LETTER" => option_env!("CS_LETTER").and_then(|v| v.parse().ok()),
            "CS_VAULT_SLOTS" => option_env!("CS_VAULT_SLOTS").and_then(|v| v.parse().ok()),
            "CS_POTION_GROWTH" => option_env!("CS_POTION_GROWTH").and_then(|v| v.parse().ok()),
            "CS_CASH_BAGSLOT" => option_env!("CS_CASH_BAGSLOT").and_then(|v| v.parse().ok()),
            _ => None,
        })
        .unwrap_or(default)
}

impl Vm {
    /// Host `MC_grpFillRect(fb, x, y, w, h, ctx)`.
    ///
    /// The same shape as the blitter: the guest re-tests the four clip edges
    /// and recomputes the row base for every pixel of every filled rectangle,
    /// and this game fills a lot of them — 26% of the guest instructions that
    /// remained once the blitter moved to the host.
    /// Host `__divsi3` / `__udivsi3`.
    ///
    /// ARMv5TE has no divide instruction, so GCC calls one of these for every
    /// `/` and `%` in the game — 55 instructions of shift-and-subtract each.
    /// They are hot in aggregate rather than in one place: three per pixel in
    /// the box blur below, one per `DM_GET_FRAME_BUFFER_INFO`, and one in every
    /// `%` in the UI code.
    ///
    /// A zero divisor declines rather than answering 0: the shipped routine
    /// calls `__div0` first, and whatever that does is not worth reproducing
    /// for a case this game never reaches. `i32::MIN / -1` is the one case Rust
    /// would panic on where ARM wraps.
    pub(crate) fn host_div(&mut self, signed: bool) -> Result<bool, String> {
        let (n, d) = (self.cpu.reg_get(Mode::User, 0), self.cpu.reg_get(Mode::User, 1));
        if d == 0 {
            return Ok(false);
        }
        let q = if signed { (n as i32).wrapping_div(d as i32) as u32 } else { n / d };
        self.cpu.reg_set(Mode::User, 0, q);
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Sleep for the guest, instead of interpreting it staying awake.
    ///
    /// `UM_Sleep(ms_lo, ms_hi)` never sleeps: it stamps `MC_knlCurrentTime()`
    /// into a global and then calls it again in a tight loop until the
    /// difference reaches the asked-for milliseconds. On the original phone
    /// that was one hot loop of native code; here every iteration is ~50
    /// interpreted instructions plus an HLE `ftime`, so a 50 ms hit-stop
    /// (`TargetEffect_draw+0x178` — every landed hit with the flash flag set)
    /// burns the CPU flat out for its whole duration. The wall-clock behaviour
    /// of sleeping is *identical* — the loop's exit is time-based either way —
    /// so the only thing serving this changes is which component waits: the
    /// scheduler instead of the interpreter. On a phone that difference is
    /// heat.
    ///
    /// The hook sits **on the loop**, not the entry, so the guest has already
    /// stamped its start time and parked the duration in `r7:r6` — nothing
    /// about the function's observable state is reproduced here. It computes
    /// what is still left (reading the start back exactly as the loop's own
    /// `ldr`/`ldrd` pair would, off the live `r4`/`r5`), waits that long, and
    /// **declines**: the guest re-reads the clock, finds the time gone, and
    /// leaves through its own epilogue. Re-entrant by construction — if the
    /// host slept short and the loop comes back, only the remainder is slept.
    ///
    /// Under the probe's virtual clock a real sleep would do nothing (the
    /// clock is frozen inside a frame, so the shipped loop would spin against
    /// the run budget forever); the virtual clock is advanced by the remainder
    /// instead, which is deterministic and is exactly what the guest observes
    /// on a real phone: time passed.
    pub(crate) fn host_um_sleep(&mut self) -> Result<bool, String> {
        if !host_sleep() {
            return Ok(false);
        }
        let lo = self.cpu.reg_get(Mode::User, 7) as u64;
        let hi = self.cpu.reg_get(Mode::User, 6) as u64;
        let asked = ((hi << 32) | lo) as i64;
        // The start-of-sleep stamp, read the way the loop itself reads it two
        // instructions from now: r3 = [r4 + r5]; start = ldrd [r3].
        let table = self.cpu.reg_get(Mode::User, 4).wrapping_add(self.cpu.reg_get(Mode::User, 5));
        let slot = self.mem.r32(table);
        let start = (self.mem.r32(slot) as u64 | ((self.mem.r32(slot + 4) as u64) << 32)) as i64;
        let now = self.now_ms();
        let left = asked.saturating_sub(now.saturating_sub(start));
        // A start in the future or a duration already elapsed is not ours to
        // interpret — decline and let the loop's own compare decide.
        if left <= 0 || asked <= 0 || now < start {
            return Ok(false);
        }
        if self.cfg.virtual_clock {
            self.clock_ms += left;
            self.sleep_ms += left as u64;
        } else {
            let ms = left.min(SLEEP_HOST_CAP_MS);
            std::thread::sleep(std::time::Duration::from_millis(ms as u64));
            self.sleep_ms += ms as u64;
        }
        self.sleep_host += 1;
        Ok(false)
    }

    /// Shrink the keyboard's keys to fit the sub-window it is being drawn into.
    ///
    /// The metrics are never read or reverse-engineered. The guest's own layout
    /// runs — through [`Vm::call`], as many times as it takes — on a **narrower**
    /// surface than the real one, and the narrowing is what makes it small. All
    /// forty-odd halfwords and the whole key table come out consistent with each
    /// other because the guest computed them together, which no amount of
    /// post-scaling could guarantee. Same shape as `host_cash_potion_list`: let
    /// the guest build the structure, then adjust it.
    ///
    /// It is **self-calibrating**: how tall a keyboard of a given width comes out
    /// is measured, not predicted, so this holds for any surface and would still
    /// hold if the layout's arithmetic were different from what was measured. The
    /// search is a plain bisection on width, and the height to compare against is
    /// the panel's own — every key and the input line are inside it, and the
    /// layout always ends with `panel.y + panel.h` exactly on the bottom edge.
    ///
    /// The one thing left over is that a narrow keyboard sits against x = 0, so
    /// the x of the two boxes and of every key is translated to centre it. Only x:
    /// the vertical numbers are the guest's, and they already put the keyboard on
    /// the bottom edge of the real screen because that is the height it was given.
    pub(crate) fn host_ime_layout(&mut self) -> Result<bool, String> {
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        // Our own re-issues of the layout come back through here.
        if lr == crate::RETURN_MAGIC {
            return Ok(false);
        }
        let (Some(init), Some(ime), Some(get_fb)) = (
            self.symbol("imeLocal_button_init"),
            self.symbol("_midas_internalImeInstance"),
            self.symbol("MC_grpGetScreenFrameBuffer"),
        ) else {
            return Ok(false);
        };
        let real = self.call(get_fb, &[0])?;
        if real == 0 {
            self.ime_lay_w = 0;
            self.call(init, &[])?;
            self.cpu.reg_set(Mode::User, reg::PC, lr);
            return Ok(true);
        }
        let fb = FrameBuffer::read(&mut self.mem, real);
        let avail = (fb.h * ime_height_pct() / 100).max(1);

        // Run the guest's layout for a given width and report how tall it came
        // out. `ime_lay_w` is what `host_ime_surface` answers with, and it is
        // cleared before the error is propagated so a failed call cannot leave
        // the rest of the process seeing a narrowed screen.
        let lay = |vm: &mut Self, w: i32| -> Result<i32, String> {
            vm.ime_lay_w = w;
            let r = vm.call(init, &[]);
            vm.ime_lay_w = 0;
            r?;
            Ok(vm.mem.r16(ime + IME_BOXES[0].4) as i16 as i32)
        };
        // Full width first: on a screen the shipped keyboard already fits, that
        // is the answer, and nothing is moved or measured further.
        let mut best = fb.w;
        let mut tall = lay(self, fb.w)?;
        if tall > avail {
            let mut laid;
            // Bisect for the widest layout that fits. The floor is used as-is
            // until something is known to fit, so a screen too small for any
            // keyboard still gets the smallest one rather than the overflowing
            // full-width one.
            let (mut lo, mut hi) = ((fb.w / 4).max(1), fb.w);
            (best, laid) = (lo, lo);
            tall = lay(self, lo)?;
            while lo + 1 < hi {
                let mid = lo + (hi - lo) / 2;
                laid = mid;
                let t = lay(self, mid)?;
                if t <= avail {
                    (lo, best, tall) = (mid, mid, t);
                } else {
                    hi = mid;
                }
            }
            // The bisection can end on a candidate it rejected, and what is left
            // in memory is what the keyboard will be drawn from — so the chosen
            // width has to be the one laid out last, not merely the one recorded.
            if laid != best {
                tall = lay(self, best)?;
            }
        }

        let dx = ((fb.w - best) / 2).max(0);
        if dx > 0 {
            for &(_, x, ..) in &IME_BOXES {
                let v = self.mem.r16(ime + x) as i16 as i32;
                self.mem.w16(ime + x, (v + dx) as u16);
            }
            let count = match self.mem.r32(ime + IME_MODE) {
                0 => IME_KEYS_QWERTY,
                _ => IME_KEYS_NUMERIC,
            };
            for i in 0..count {
                let at = ime + IME_KEYS + i * IME_KEY_STRIDE;
                // A zero-width record is an unused slot; the paint and both hit
                // tests skip it, so moving it would only muddy a debug dump.
                if self.mem.r16(at + 4) as i16 > 0 {
                    let v = self.mem.r16(at) as i16 as i32;
                    self.mem.w16(at, (v + dx) as u16);
                }
            }
        }
        log::info!(
            "ime layout: {}x{} of {}x{} screen ({} px budget), centred at x {dx}",
            best,
            tall,
            fb.w,
            fb.h,
            avail
        );
        if ime_debug() {
            self.ime_dump(ime, &fb, avail);
        }
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Print everything `imeLocal_QWERTY_Paint` positions itself from, so two
    /// runs at different widths can be compared line for line. `CS_IME_DEBUG=1`.
    ///
    /// The key table is only half of the picture, and printing only it is what
    /// misled the previous attempt: it measured `max(y + h)`, saw the keys fit,
    /// and concluded the layout was fine — while the panel this also prints was
    /// 293 px tall on a 240 px screen with the input line off the top of it.
    fn ime_dump(&mut self, ime: u32, fb: &FrameBuffer, avail: i32) {
        let (h16, w32) = (
            |vm: &mut Self, o: u32| vm.mem.r16(ime + o) as i16 as i32,
            |vm: &mut Self, o: u32| vm.mem.r32(ime + o) as i32,
        );
        log::info!(
            "ime dump: surface {}x{} avail {avail}; rot {} gate {} font {}",
            fb.w,
            fb.h,
            h16(self, 0x296),
            self.mem.r8(ime + 0x294),
            w32(self, 0x58),
        );
        // Panel, then input line. Both are (x, y, w, h) in the same space as the
        // key table, so anything that failed to follow the narrowing shows up
        // here as a box that no longer agrees with the keys below it.
        for &(what, x, y, w, h) in &IME_BOXES {
            log::info!(
                "ime dump: {what} x {} y {} w {} h {} (bottom {})",
                h16(self, x),
                h16(self, y),
                h16(self, w),
                h16(self, h),
                h16(self, y) + h16(self, h),
            );
        }
        let count = match self.mem.r32(ime + IME_MODE) {
            0 => IME_KEYS_QWERTY,
            _ => IME_KEYS_NUMERIC,
        };
        let (mut top, mut bottom, mut left, mut right) = (i32::MAX, i32::MIN, i32::MAX, i32::MIN);
        for i in 0..count {
            let r = ime + IME_KEYS + i * IME_KEY_STRIDE;
            let (x, y, w, h) = (
                self.mem.r16(r) as i16 as i32,
                self.mem.r16(r + 2) as i16 as i32,
                self.mem.r16(r + 4) as i16 as i32,
                self.mem.r16(r + 6) as i16 as i32,
            );
            if w <= 0 {
                continue;
            }
            (top, bottom) = (top.min(y), bottom.max(y + h));
            (left, right) = (left.min(x), right.max(x + w));
            let code = self.mem.r32(r + 0xc) as i32;
            log::debug!("ime dump: key {i:2} x {x} y {y} w {w} h {h} code {code}");
        }
        log::info!("ime dump: keys x {left}..{right} y {top}..{bottom}");
    }

    /// Log the key the keyboard decided was pressed, and let it run.
    ///
    /// `imeLocal_onHandleEventKey(code)` is the single funnel: a tap is
    /// hit-tested by `imeLocal_onHandleEventTouch`, which returns the touched
    /// record's stored code (`ime + 0x2e8 + i*0x14`), and the release re-enters
    /// here as a synthetic key press. So this one line settles what a button
    /// actually is, which reading the ARM cannot: the codes come from the layout
    /// the guest built at run time, not from the image.
    ///
    /// The named ones are the four that are not characters. `-16` is backspace —
    /// `imeLocal_onHandleEventKey+0x360` runs `imeCore_MakeHangul_revertIng` and
    /// then shrinks the buffer — and `-5` is 완료, the only code that releases the
    /// overlay.
    pub(crate) fn host_ime_key_log(&mut self) -> Result<bool, String> {
        let code = self.cpu.reg_get(Mode::User, 0) as i32;
        let name = match code {
            -5 => " (완료: flush, release, close callback)",
            -16 => " (backspace)",
            -6 => " (한글/영문 toggle)",
            -7 => " (shift)",
            _ => "",
        };
        let ime = self.symbol("_midas_internalImeInstance").unwrap_or(0);
        let len = if ime == 0 { -1 } else { self.mem.r32(ime + 0x274) as i32 };
        log::info!("ime key: {code}{name}; buffer now {len} bytes");
        Ok(false)
    }

    /// Let the OK button drawn behind the naming popup answer it.
    ///
    /// Rewrites the event in place — kind touch to kind key, the x slot (which is
    /// also the key slot) to `-5` — and declines, so the popup is answered by its
    /// own key path. Everything before the rewrite is a guard: our naming only,
    /// the popup only, a touch only, and the corner the button is drawn in.
    pub(crate) fn host_name_ok(&mut self) -> Result<bool, String> {
        if !self.pet_rename_pending {
            return Ok(false);
        }
        let (Some(idx), Some(mf), Some(sel), Some(lcd)) = (
            self.symbol("nameTitleIdx"),
            self.symbol("mainFrame"),
            self.symbol("nameTitleSel"),
            self.symbol("_lcdG"),
        ) else {
            return Ok(false);
        };
        if self.mem.r16(idx) as i16 as i32 != NAME_CONFIRM
            || self.mem.r32(mf + MF_EVENT) != MF_TOUCH
        {
            return Ok(false);
        }
        let (w, h) = (self.mem.r32(lcd + 0x4c) as i32, self.mem.r32(lcd + 0x50) as i32);
        let (x, y) = (self.mem.r32(mf + MF_X) as i32, self.mem.r32(mf + MF_Y) as i32);
        let (bw, bh) = OK_BUTTON;
        if x < w - bw || y < h - bh || x >= w || y >= h {
            return Ok(false);
        }
        // The selection is never drawn, so activating whatever the two-tap idiom
        // last left in it would be a coin flip. OK means 예.
        self.mem.w8(sel, NAME_SEL_YES);
        self.mem.w32(mf + MF_EVENT, MF_KEY);
        self.mem.w32(mf + MF_X, NAME_KEY_OK);
        log::debug!("name popup: OK button at ({x}, {y}) answered 예");
        Ok(false)
    }

    /// Make `←` on an empty name do nothing instead of cancelling.
    ///
    /// Hooked on the `beq` itself, so everything the backspace handler does
    /// before the test still happens and only the destination changes: taken
    /// means "nothing left to delete", and that is pointed at `IME_BS_NOTHING`,
    /// which is where the guest's own two failed-backspace cases already go. Not
    /// taken is declined, so the real delete stays entirely the guest's.
    pub(crate) fn host_ime_backspace(&mut self) -> Result<bool, String> {
        let Some(base) = self.symbol("imeLocal_onHandleEventKey") else {
            return Ok(false);
        };
        // The guest has just done `cmp r2, #0` on the first byte of the text.
        // Reading r2 rather than the flags keeps this honest about which
        // comparison it is standing in for.
        if self.cpu.reg_get(Mode::User, 2) & 0xff != 0 {
            return Ok(false);
        }
        self.cpu.reg_set(Mode::User, reg::PC, base + IME_BS_NOTHING);
        Ok(true)
    }

    /// Answer the keyboard layout's `__knlGR_getScreenFrameBuffer` with a
    /// narrower surface.
    ///
    /// Hooked on the **return address**, not the function: the other six callers
    /// must keep seeing the real surface, and the guest's own body has already run
    /// and left the real pointer in `r0`. So this reads that, copies it into a
    /// scratch block with the width replaced, and puts the scratch pointer back —
    /// the callers only ever read through it.
    ///
    /// **Same origin, same stride, same height** — only `w` differs, and only
    /// while [`Vm::host_ime_layout`] is measuring. A moved base would put every
    /// glyph in the wrong place, since the font writes to `*_midas_lcd` at raw
    /// screen coordinates and never looks at the context it was handed.
    pub(crate) fn host_ime_surface(&mut self) -> Result<bool, String> {
        let real = self.cpu.reg_get(Mode::User, 0);
        let want = self.ime_lay_w;
        if real == 0 || want <= 0 {
            return Ok(false);
        }
        let fb = FrameBuffer::read(&mut self.mem, real);
        if fb.pixels == 0 || fb.pitch <= 0 || fb.w <= 0 || fb.h <= 0 || want > fb.w {
            return Ok(false);
        }
        if self.ime_fb == 0 {
            self.ime_fb = self.heap.alloc(0x18).ok_or("heap: ime framebuffer")?;
        }
        let at = self.ime_fb;
        self.mem.w32(at, want as u32);
        self.mem.w32(at + 4, fb.h as u32);
        self.mem.w32(at + 8, fb.pitch as u32);
        self.mem.w32(at + 0xc, fb.bpp as u32);
        self.mem.w32(at + 0x10, fb.pixels);
        self.cpu.reg_set(Mode::User, 0, at);
        // Not a served call — the guest function already returned. Let the
        // instruction at this address run with the substituted result.
        Ok(false)
    }

    /// Host `_midas_drawPixel` / `_midas_drawLineH` / `_midas_drawLineV` /
    /// `_midas_fillRect` — the MIDAS drawing family underneath every rounded
    /// rectangle in the game, and all four are one solid-colour rectangle.
    ///
    /// This is the pet-naming keyboard, measured on the device: a frame that
    /// paints `imeLocal_QWERTY_Paint` costs **9.45 M guest instructions (86 ms on
    /// an S25)**, and the slow-frame profiler named the leaves — `_midas_drawPixel`
    /// **75.3%** plus `_midas_drawLineH` **20.6%**. Serving only those two left
    /// `_midas_drawLineV` at 26% of what remained (it reaches the same pixels
    /// through its own per-pixel `bl`), so the family is served as a family.
    /// None of it is the keyboard's alone: `_midas_drawLineH` has 23 callers and
    /// `_midas_drawPixel` is also `__drawEngChar`'s.
    ///
    /// Every entry is `(g, x, y, ...)` and reduces to an **inclusive** run: the
    /// guest draws `+len` first and then walks back down to `+0`, so a `len` of 0
    /// still draws one pixel and a line is `len + 1` long. `_midas_fillRect` is
    /// that in both axes.
    ///
    /// The mapping is a global rotation, `*_midas_drawDIR`, and the clip is
    /// against the context's own `w`/`h` **after** the rotation — the one part
    /// that looks wrong and is not:
    ///
    /// | `*_midas_drawDIR` | destination |
    /// |---|---|
    /// | `0` | `(x, y)` |
    /// | `> 0` | `(y, h - 1 - x)` |
    /// | `< 0` | `(w - 1 - y, x)` |
    ///
    /// A pixel is dropped if **either** coordinate is negative (the guest tests
    /// `orrs`/`bmi` on the pair, so one sign check does both) or if it is not
    /// `dx < w && dy < h`. Since all three mappings are axis-aligned, a source
    /// rectangle maps to a destination rectangle, so the two source corners give
    /// the whole destination extent and the work is done there — no per-pixel
    /// mapping, and the touched span is validated once so the host either serves
    /// the call completely or declines untouched.
    ///
    /// **A bpp that is neither 16 nor 32 writes nothing at all** — the guest falls
    /// off the end of its own compare chain. That is declined rather than
    /// reproduced: "draws nothing" is not worth claiming.
    pub(crate) fn host_midas_line(&mut self, op: MidasOp) -> Result<bool, String> {
        let g = self.cpu.reg_get(Mode::User, 0);
        let x = self.cpu.reg_get(Mode::User, 1) as i32;
        let y = self.cpu.reg_get(Mode::User, 2) as i32;
        let arg3 = self.cpu.reg_get(Mode::User, 3);
        // Stack arguments start at `[sp]`: the guest reads its fifth at
        // `[sp+0x18]` only because its own `push {r4..r8, lr}` has run by then,
        // and this hook is at the entry, before that push. The same reason
        // `host_fill_rect` reads its fifth at `r32(sp)`.
        let sp = self.cpu.reg_get(Mode::User, reg::SP);
        let (len_x, len_y, colour) = match op {
            MidasOp::Pixel => (0, 0, arg3),
            MidasOp::LineH => (arg3 as i32, 0, self.mem.r32(sp)),
            MidasOp::LineV => (0, arg3 as i32, self.mem.r32(sp)),
            MidasOp::FillRect => (arg3 as i32, self.mem.r32(sp) as i32, self.mem.r32(sp + 4)),
        };
        if g == 0 || self.midas_dir == 0 {
            return Ok(false);
        }
        let fb = FrameBuffer::read(&mut self.mem, g);
        let bytes = match fb.bpp {
            16 => 2i32,
            32 => 4,
            _ => return Ok(false),
        };
        // A negative length is not a shape the guest can draw — its loop counts
        // down to zero, so it would spin some four billion times.
        if fb.pixels == 0 || fb.pitch <= 0 || len_x < 0 || len_y < 0 {
            return Ok(false);
        }
        let dir = self.mem.r32(self.midas_dir) as i32;
        let ret = |vm: &mut Vm| {
            let lr = vm.cpu.reg_get(Mode::User, reg::LR);
            vm.cpu.reg_set(Mode::User, reg::PC, lr);
            vm.midas_host += 1;
            Ok(true)
        };

        let map = |x: i32, y: i32| -> (i32, i32) {
            match dir {
                0 => (x, y),
                d if d > 0 => (y, fb.h.wrapping_sub(1).wrapping_sub(x)),
                _ => (fb.w.wrapping_sub(1).wrapping_sub(y), x),
            }
        };
        let (Some(x1), Some(y1)) = (x.checked_add(len_x), y.checked_add(len_y)) else {
            return Ok(false);
        };
        let (ax, ay) = map(x, y);
        let (bx, by) = map(x1, y1);
        // Clip in destination space. Empty is a legitimate outcome — a run wholly
        // off the surface draws nothing and the guest still returns normally.
        let (dx0, dx1) = (ax.min(bx).max(0), ax.max(bx).min(fb.w - 1));
        let (dy0, dy1) = (ay.min(by).max(0), ay.max(by).min(fb.h - 1));
        if dx0 > dx1 || dy0 > dy1 {
            return ret(self);
        }

        let off = |dx: i32, dy: i32| dy as i64 * fb.pitch as i64 + dx as i64 * bytes as i64;
        let (lo, hi) = (off(dx0, dy0), off(dx1, dy1) + bytes as i64);
        if lo < 0 || hi > u32::MAX as i64 {
            return Ok(false);
        }
        let row = ((dx1 - dx0 + 1) * bytes) as usize;
        let Some(buf) = self.mem.slice_mut(fb.pixels.wrapping_add(lo as u32), (hi - lo) as u32)
        else {
            return Ok(false);
        };
        let unit: [u8; 4] = colour.to_le_bytes();
        for dy in dy0..=dy1 {
            let at = (off(dx0, dy) - lo) as usize;
            for o in (at..at + row).step_by(bytes as usize) {
                buf[o..o + bytes as usize].copy_from_slice(&unit[..bytes as usize]);
            }
        }
        ret(self)
    }

    /// Host `DM_BoxBlurLCD565(fb, radius)` — the frosted-glass background.
    ///
    /// This one call is the single most expensive thing the game does. It runs
    /// once when a menu, a shop or a dialogue opens, and at 400x240 with the
    /// radius the game passes (5) it costs **55.6 million guest instructions in
    /// one frame** — about 1.2 s on a Galaxy S21, 1.6 s at the 533x240 the
    /// device actually runs. It is 99.5% of that frame. The main menu's radius
    /// 3 costs 33 M. That is the "menu open" stutter, whole.
    ///
    /// It is quadratic for no reason: for every output pixel it re-walks the
    /// whole k×k box, and then does **three `__divsi3` calls** — by a count
    /// that varies, because samples outside the surface are skipped rather than
    /// clamped. So the mean is over the clipped box and the divisor is however
    /// many samples that was.
    ///
    /// The host keeps that exactly — same clipped box, same variable divisor,
    /// same truncating division, same 565 recomposition (blue masked, green
    /// masked after its shift, red not masked because a 5-bit mean cannot
    /// overflow) — but computes it as a separable sliding window: column sums
    /// carried down the rows, then a row sum carried across. Every intermediate
    /// is an exact integer, so the result is identical pixel for pixel while
    /// the cost drops from O(w·h·k²) to O(w·h).
    ///
    /// The guest writes its output into the off-screen framebuffer
    /// (`*(_lcdG+8)`) as a scratch buffer, packed at the *source* width and
    /// ignoring that buffer's own pitch, and then `memcpy`s it back over the
    /// source. Both writes are reproduced: the scratch is observable, and the
    /// next thing to draw into it would otherwise see different bytes.
    pub(crate) fn host_box_blur(&mut self) -> Result<bool, String> {
        let fb_p = self.cpu.reg_get(Mode::User, 0);
        let k = (self.cpu.reg_get(Mode::User, 1) as i32).max(2);
        let Some(lcd_g) = self.symbol("_lcdG") else { return Ok(false) };
        if fb_p == 0 {
            return Ok(false);
        }
        // `DM_BoxBlurLCD` checks this before tail-branching in; a direct call
        // would not, and the 565 arithmetic is only right at 16bpp.
        if self.mem.r8(lcd_g.wrapping_add(0x8a)) != 1 {
            return Ok(false);
        }
        let (w, h) = (self.mem.r32(fb_p) as i32, self.mem.r32(fb_p + 4) as i32);
        let src_p = self.mem.r32(fb_p + 0x10);
        let off_fb = self.mem.r32(lcd_g.wrapping_add(8));
        if w <= 0 || h <= 0 || src_p == 0 || off_fb == 0 {
            return Ok(false);
        }
        let scratch_p = self.mem.r32(off_fb + 0x10);
        let bytes = (w as u32).checked_mul(h as u32).and_then(|n| n.checked_mul(2));
        let (Some(bytes), true) = (bytes, scratch_p != 0) else { return Ok(false) };
        // The guest interleaves reads of the source with writes of the scratch;
        // computing every output from a snapshot is only the same thing if the
        // two do not overlap. They are different framebuffers (`_lcdG+4` and
        // `_lcdG+8`) so they never do, but a silent difference here would leave
        // no trace in an A/B of the frames that happen to be dumped.
        if src_p < scratch_p.wrapping_add(bytes) && scratch_p < src_p.wrapping_add(bytes) {
            return Ok(false);
        }
        let Some(src) = self.mem.slice(src_p, bytes) else { return Ok(false) };

        let (w, h) = (w as usize, h as usize);
        let px: Vec<u16> = src.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();

        // Column sums over the row window, carried down the image.
        let (mut cr, mut cg, mut cb) = (vec![0i32; w], vec![0i32; w], vec![0i32; w]);
        let (mut row_lo, mut row_hi) = (0usize, 0usize);
        let mut out = vec![0u8; bytes as usize];
        for y in 0..h {
            let want_hi = ((y + k as usize).saturating_sub(1)).min(h);
            let want_lo = y.saturating_sub(1);
            while row_hi < want_hi {
                let base = row_hi * w;
                for x in 0..w {
                    let p = px[base + x] as i32;
                    cr[x] += p >> 11;
                    cg[x] += (p & 0x7e0) >> 5;
                    cb[x] += p & 0x1f;
                }
                row_hi += 1;
            }
            while row_lo < want_lo {
                let base = row_lo * w;
                for x in 0..w {
                    let p = px[base + x] as i32;
                    cr[x] -= p >> 11;
                    cg[x] -= (p & 0x7e0) >> 5;
                    cb[x] -= p & 0x1f;
                }
                row_lo += 1;
            }
            let rows = (want_hi - want_lo) as i32;

            let (mut sr, mut sg, mut sb) = (0i32, 0i32, 0i32);
            let (mut col_lo, mut col_hi) = (0usize, 0usize);
            for x in 0..w {
                let hi = ((x + k as usize).saturating_sub(1)).min(w);
                let lo = x.saturating_sub(1);
                while col_hi < hi {
                    sr += cr[col_hi];
                    sg += cg[col_hi];
                    sb += cb[col_hi];
                    col_hi += 1;
                }
                while col_lo < lo {
                    sr -= cr[col_lo];
                    sg -= cg[col_lo];
                    sb -= cb[col_lo];
                    col_lo += 1;
                }
                let n = rows * (col_hi - col_lo) as i32;
                let v = ((sr / n) as u16) << 11
                    | (((sg / n) as u16) << 5) & 0x7e0
                    | ((sb / n) as u16) & 0x1f;
                let o = (y * w + x) * 2;
                out[o..o + 2].copy_from_slice(&v.to_le_bytes());
            }
        }

        self.mem.write_bytes(scratch_p, &out);
        self.mem.write_bytes(src_p, &out);
        self.blur_host += 1;
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Host `DM_RadialBlurLcd565(fb)` — the zoom streak a dying boss leaves.
    ///
    /// The most expensive single thing in this game. Measured on an S25 across
    /// ten boss kills it runs five frames back to back at 53–70 M guest
    /// instructions each: 2.9 s of solid main-thread block there, and 6.4 s on
    /// an S21-class phone at 46 M inst/s. Android's input-dispatch ANR fires at
    /// 5 s, which is the most likely reading of "sometimes force-closes on the
    /// killing blow, on slower phones" — and of why no `cs_crash.txt` is left
    /// behind, since an ANR kill runs neither the fault path nor the panic hook.
    ///
    /// The cost is not the blur but its leaves: `Sqrt` and `_atan2` run *per
    /// output pixel*, and `_atan2` is a `__divsi3` plus a binary search, on a
    /// core with no divide instruction. Sampling along the ray is the cheap
    /// part.
    ///
    /// Everything the original reads is read here too, from guest memory by
    /// symbol rather than hardcoded — `nSin`/`nCos` are 360×Q10 but are *not*
    /// exactly sin/cos (`nSin[90]` is 1023), and `nTan` is searched with a
    /// count of 180 while the symbol is 179 entries long, so the last probe
    /// reads the word after the table. Both are why a table written out here
    /// would be a different function.
    pub(crate) fn host_radial_blur(&mut self) -> Result<bool, String> {
        let fb_p = self.cpu.reg_get(Mode::User, 0);
        let (Some(lcd_g), Some(n_cos_p), Some(n_sin_p), Some(n_tan_p)) =
            (self.symbol("_lcdG"), self.symbol("nCos"), self.symbol("nSin"), self.symbol("nTan"))
        else {
            return Ok(false);
        };
        if fb_p == 0 {
            return Ok(false);
        }
        // `DM_RadialBlurLcd` checks this before tail-branching in, exactly as
        // the box blur's wrapper does, and the 565 arithmetic below is only
        // right at 16bpp.
        if self.mem.r8(lcd_g.wrapping_add(0x8a)) != 1 {
            return Ok(false);
        }
        // `DM_GET_FRAME_BUFFER_INFO` copies `fb[0]`/`fb[4]`/`fb[0x10]` into its
        // out-struct at +8/+0xc/+0x10; the guest then reads those three. Same
        // three words `host_box_blur` reads straight off the framebuffer.
        let (w, h) = (self.mem.r32(fb_p) as i32, self.mem.r32(fb_p + 4) as i32);
        let src_p = self.mem.r32(fb_p + 0x10);
        let off_fb = self.mem.r32(lcd_g.wrapping_add(8));
        if w <= 0 || h <= 0 || src_p == 0 || off_fb == 0 {
            return Ok(false);
        }
        let dst_p = self.mem.r32(off_fb + 0x10);
        let bytes = (w as u32).checked_mul(h as u32).and_then(|n| n.checked_mul(2));
        let (Some(bytes), true) = (bytes, dst_p != 0) else { return Ok(false) };
        // Same reasoning as `host_box_blur`: every output is computed from a
        // snapshot of the source, which is only the same thing the guest does
        // if the two buffers are disjoint. They are (`_lcdG+4` and `_lcdG+8`),
        // but a silent difference here would leave no trace in an A/B.
        if src_p < dst_p.wrapping_add(bytes) && dst_p < src_p.wrapping_add(bytes) {
            return Ok(false);
        }
        // **This blur compounds.** It reads the screen, writes the result back
        // over it, and the game does not redraw the map underneath every frame —
        // so frame two blurs frame one's *output*. The shipped effect runs five
        // times and the look everyone remembers is those five compoundings.
        //
        // Stretching the animation to two seconds ran it forty-six times
        // instead, and forty-six compoundings is not "more of the same": it
        // converges on the local average, which is a white screen, and it leaves
        // that mush in the framebuffer for whatever is not redrawn afterwards.
        // Both reports — 흰색 화면 and 끝난 뒤에도 뭉개짐 — are this.
        //
        // So the *number of applications* stays what it was. It is already
        // counted: the same five-way split that gives the world its five ticks
        // (`host_zoom_world`), which is what paired one blur with one tick in
        // the shipped effect too. In between, hold the frame — there is nothing
        // to redraw, because nothing under it moved.
        if self.zoom_running {
            if self.zoom_blur_due == self.zoom_blur_done {
                let lr = self.cpu.reg_get(Mode::User, reg::LR);
                self.cpu.reg_set(Mode::User, reg::PC, lr);
                return Ok(true);
            }
            self.zoom_blur_done = self.zoom_blur_due;
        }

        // The animation counter `Map_drawMiddleObject` steps until it passes
        // 0x27. It scales the ray length, so it is what makes the streak grow
        // over the effect's life — five frames as shipped, or the whole of
        // `CS_BOSS_ZOOM_MS` once `host_boss_zoom` is stepping it.
        let counter = self.mem.r32(lcd_g.wrapping_add(0x8c)) as i32;
        let n_cos = self.read_words(n_cos_p, 360);
        let n_sin = self.read_words(n_sin_p, 360);

        // Where it converges, and how far it reaches. The shipped answers are
        // the middle of the screen and "everywhere"; `CS_BOSS_ZOOM_R` fades the
        // reach out, and the kill sites move the centre onto the boss. The two
        // are separate: `R = 0` restores the shipped *reach* only, and the centre
        // still follows the boss. Nothing short of `CS_BOSS_ZOOM_MS = 0` — which
        // installs none of this — puts the shipped image back.
        let r_pct = self.zoom_r_pct;
        let fall = if r_pct > 0 { h.saturating_mul(r_pct) / 100 } else { 0 };
        let (cx, cy) = match self.zoom_centre {
            // Clamped: a boss dying at the edge of the view would otherwise put
            // the centre off-screen, and every ray would point the same way.
            // Independent of the falloff — "converges on the boss" and "stops
            // before the edges" are separate wants, and the reference footage
            // of the original is the first without the second.
            Some((x, y)) => (x.clamp(0, w - 1), y.clamp(0, h - 1)),
            None => (w >> 1, h >> 1),
        };

        // Everything per-pixel that does not depend on the counter, hoisted out
        // of the frame. See [`RadialMap`] — this is what makes an effect that
        // runs for two seconds cost what five frames used to.
        if self.radial_map.as_ref().is_none_or(|m| (m.w, m.h, m.cx, m.cy) != (w, h, cx, cy)) {
            // 180, not the symbol's 179: `_atan2` passes `#0xb4` to
            // `BinarySearch`, so index 179 is a real probe the original makes
            // past the end.
            let n_tan = self.read_words(n_tan_p, 180);
            self.radial_map = RadialMap::build(w, h, cx, cy, &n_tan);
        }
        let Some(map) = self.radial_map.as_ref() else { return Ok(false) };
        let diag = map.diag;

        // Eighths of a counter, when the retime left a value that belongs to
        // the counter the guest is holding. Scaling both sides of the division
        // leaves a whole eighth bit-for-bit equal to `radius * counter / diag`,
        // so the shipped five frames — and every test written against them —
        // go down the same arithmetic as before.
        let (num, den) = match self.zoom_fine {
            f if f > 0 && f / ZOOM_SUB == counter => (f, diag.wrapping_mul(ZOOM_SUB)),
            _ => (counter, diag),
        };
        // The other half of the applications trade: more of them compound to a
        // heavier smear, and this takes it back out of each one. 100 is the
        // shipped ray for that counter, and the arithmetic is untouched then.
        let (num, den) = match self.zoom_strength {
            100 => (num, den),
            s => (num.saturating_mul(s), den.saturating_mul(100)),
        };
        if den == 0 {
            return Ok(false); // `__divsi3` would call `__div0`; let the guest.
        }

        let (w1, h1) = (w - 1, h - 1);
        // Both buffers are reused between frames: at 520x240 they are a quarter
        // of a megabyte each, and the effect now asks for them a hundred times
        // rather than five.
        let mut px = std::mem::take(&mut self.radial_px);
        let mut out = std::mem::take(&mut self.radial_out);
        px.clear();
        match self.mem.slice(src_p, bytes) {
            Some(src) => px.extend(src.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]))),
            None => {
                self.radial_px = px;
                self.radial_out = out;
                return Ok(false);
            }
        }
        out.clear();
        out.resize(bytes as usize, 0);

        let mut declined = false;
        for (i, &(radius, angle)) in map.polar.iter().enumerate() {
            let radius = radius as i32;
            // `RadialMap::build` folded the angle into 0..359, which is the
            // whole of both tables.
            let (cos_a, sin_a) = (n_cos[angle as usize], n_sin[angle as usize]);
            let mut n = radius.wrapping_mul(num) / den;
            if fall > 0 {
                // Outside the bubble the pixel is left exactly as it was — not
                // run through a one-sample walk, which lands a fraction of a
                // pixel off and would show as a faint shimmer on the parts of
                // the screen this is trying to keep still.
                if radius >= fall {
                    out[i * 2..i * 2 + 2].copy_from_slice(&px[i].to_le_bytes());
                    continue;
                }
                n = n.wrapping_mul(fall - radius) / fall;
            }
            // The original's `blt` skips sampling for a negative count and
            // divides the zeroed accumulators by `n+1`. Unreachable while the
            // counter is positive, and a `n == -1` would be a divide by zero,
            // so hand those back rather than model them.
            if n < 0 || n > 4096 {
                declined = true;
                break;
            }
            let (mut sr, mut sg, mut sb) = (0i32, 0i32, 0i32);
            let (mut px_q, mut py_q) = (cos_a.wrapping_mul(radius), sin_a.wrapping_mul(radius));
            // `n + 1` samples: the guest increments before its `cmp`, so the
            // walk runs one past the count it computed.
            for _ in 0..=n {
                let mut sx = cx - (px_q >> 10);
                let mut sy = cy - (py_q >> 10);
                // `cmp/ldrle/bicgt`: clamp high to the last row or column, low
                // to zero — an if/else on the same condition, not two
                // independent clamps.
                sx = if w <= sx { w1 } else { sx.max(0) };
                sy = if h <= sy { h1 } else { sy.max(0) };
                let p = px[(w.wrapping_mul(sy) + sx) as usize] as i32;
                sr += p >> 11;
                sg += (p & 0x7e0) >> 5;
                sb += p & 0x1f;
                px_q = px_q.wrapping_sub(cos_a);
                py_q = py_q.wrapping_sub(sin_a);
            }
            let d = n + 1;
            // Same 565 reassembly as the box blur, including the asymmetry:
            // blue masked, green shifted then masked, red neither — the `strh`
            // is what truncates it.
            let v = ((sb / d) as u32 & 0x1f)
                | (((sr / d) as u32) << 11)
                | ((((sg / d) as u32) << 5) & 0x7e0);
            let o = i * 2;
            out[o..o + 2].copy_from_slice(&(v as u16).to_le_bytes());
        }

        if !declined {
            // The guest fills the offscreen buffer at the *source's* width,
            // then copies it back over the source. Both, as `host_box_blur`
            // does.
            self.mem.write_bytes(dst_p, &out);
            self.mem.write_bytes(src_p, &out);
            self.radial_host += 1;
            let lr = self.cpu.reg_get(Mode::User, reg::LR);
            self.cpu.reg_set(Mode::User, reg::PC, lr);
        }
        self.radial_px = px;
        self.radial_out = out;
        Ok(!declined)
    }

    /// Retime the boss-kill zoom — `Map_drawMiddleObject+0x1a4`.
    ///
    /// The hook stands in for one instruction: the `add r3, r3, #4` that is the
    /// entire animation. `r3` is answered from the clock instead, `PC` steps
    /// past the `add`, and the guest's own `cmp #0x27` / `str` / `ble` decide as
    /// they always did whether this was the last frame. Nothing else about the
    /// effect is touched — the guest arms it, releases it, and clears
    /// `hero[0x899]`.
    ///
    /// A fresh effect is recognised by the counter not being the one we last
    /// wrote: the guest arms it at `ZOOM_FIRST` (`+0x210`) and zeroes it at the
    /// end, and it is written nowhere else in the image. Zero is a safe initial
    /// sentinel because the guest only reaches this instruction with a counter
    /// it has already found non-zero.
    pub(crate) fn host_boss_zoom(&mut self) -> Result<bool, String> {
        let ms = self.zoom_ms as i64;
        if ms <= 0 {
            return Ok(false);
        }
        let cur = self.cpu.reg_get(Mode::User, 3) as i32;
        let now = self.now_ms();
        if cur != self.zoom_last {
            self.zoom_t0 = now;
            self.zoom_frames = 0;
            self.zoom_running = true;
            // Before the first tick of the new effect, so the frame the guest
            // would have run at counter 0x14 is let through — and likewise the
            // first of the five blurs.
            self.zoom_world_bucket = -1;
            // Zero, not -1: the effect's *first* blur has already run by the
            // time this hook does — it is four instructions earlier in the same
            // frame — so bucket 0 is spent. Five applications, not six.
            self.zoom_blur_done = 0;
            self.zoom_world_due = 0;
            self.zoom_blur_due = 0;
            log::debug!("boss zoom: retiming the kill flourish over {ms} ms");
        }
        self.zoom_frames += 1;

        let from = self.zoom_from as i64;
        let span = (ZOOM_END as i64 - from) * ZOOM_SUB as i64;
        let elapsed = (now - self.zoom_t0).clamp(0, ms);
        // Elapsed time as 16.16, raised to `zoom_curve`. At the ends it is
        // exactly 0 and exactly 1, so the sweep still starts on `from` and lands
        // on `ZOOM_END` — which is the value the guest's `cmp #0x27` refuses.
        let t = ((elapsed << 16) / ms).clamp(0, 1 << 16);
        let mut e = t;
        for _ in 1..self.zoom_curve {
            e = (e * t) >> 16;
        }
        let mut fine = from * ZOOM_SUB as i64 + ((span * e) >> 16);
        // The world's five ticks are spread over the *time*, not the counter:
        // with a curve the two are no longer the same thing, and a scene that
        // crawled and then lurched at the end would be worse than either.
        self.zoom_world_due = (self.zoom_ticks * elapsed / ms) as i32;
        // And the blur's applications the same way, on their own count.
        self.zoom_blur_due = (self.zoom_steps * elapsed / ms) as i32;
        // `hero[0x899]` stays set — no movement, no skills — until this counter
        // passes the end, so a clock that stopped would be a soft lock rather
        // than a long flourish.
        if self.zoom_frames > ZOOM_MAX_FRAMES {
            fine = (ZOOM_END * ZOOM_SUB) as i64;
        }
        let fine = fine.clamp(0, (ZOOM_END * ZOOM_SUB) as i64) as i32;
        // Never below what the guest already holds: the shipped animation only
        // grows, and the end test is the only thing that stops it.
        let next = (fine / ZOOM_SUB).max(cur);

        if next >= ZOOM_END {
            // The guest's `ble` fails on this value and releases the effect, so
            // the world is the guest's again from the next tick on.
            self.zoom_running = false;
            // Info, not debug: this is the only place the effect says how long
            // it actually took and how many frames it got to draw, and on the
            // device the second number is the one that says whether the
            // flourish was smooth or a slideshow.
            log::info!(
                "boss zoom: {} frames over {} ms, {} world ticks",
                self.zoom_frames,
                now - self.zoom_t0,
                self.zoom_world_bucket + 1
            );
        }
        self.zoom_fine = fine;
        self.zoom_last = next;
        self.cpu.reg_set(Mode::User, 3, next as u32);
        let pc = self.cpu.reg_get(Mode::User, reg::PC);
        self.cpu.reg_set(Mode::User, reg::PC, pc.wrapping_add(4));
        Ok(true)
    }

    /// Slow the world to the zoom's pace — `GameFrame_Process`, held shut on
    /// most frames while the kill flourish is on screen.
    ///
    /// Stretching the zoom on its own gets the timing right and the feel wrong:
    /// the scene keeps moving at full speed under it, which the effect never did.
    /// The shipped five frames were five *stalls*, so the world advanced five
    /// ticks in the whole 2.9 s and everything crawled together. This gives back
    /// exactly that — **the same five ticks, at the same five points of the
    /// animation**, one per `ZOOM_SHIPPED_STEP` of counter, now spread over
    /// whatever `CS_BOSS_ZOOM_MS` asks for.
    ///
    /// `GameFrame_Process` is the whole of the world and none of the drawing:
    /// `Map_objectPreProcess` (hero, NPCs, monsters, effects, pet) has this one
    /// caller, and so does every one of those in turn. What is *not* in it
    /// matters as much — `MainFrame_Process` plays the music and polls the
    /// network either side of the frame's own tick, `MainFrame_KeyPressed` runs
    /// before it, and `GameFrame_Render` (which owns the counter, the blur and
    /// the release of `hero[0x899]`) is a different call altogether. So the
    /// effect still ends on time with the world standing still.
    pub(crate) fn host_zoom_world(&mut self) -> Result<bool, String> {
        if !self.zoom_running {
            return Ok(false);
        }
        // Which of the five is due, as of the render before this tick.
        if self.zoom_world_due != self.zoom_world_bucket {
            self.zoom_world_bucket = self.zoom_world_due;
            return Ok(false); // one of the shipped five: let the world run
        }
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Arm a fresh zoom lower than the guest does — the `mov r2, #0x14` at
    /// `Map_drawMiddleObject+0x210`, answered with `CS_BOSS_ZOOM_FROM`.
    ///
    /// The guest's own `str r2, [r3, #0x8c]` on the next instruction still does
    /// the writing, and the first blur of the effect is the one that reads it.
    /// Only installed when the two differ, so the shipped start costs nothing.
    pub(crate) fn host_zoom_arm(&mut self) -> Result<bool, String> {
        if self.zoom_ms <= 0 {
            return Ok(false);
        }
        // Fix where the effect converges, once, while the boss that set this
        // off is still where it died. The world is held still for the duration
        // (`host_zoom_world`), so re-deriving it per frame would only let a
        // camera shake wobble the centre — and rebuild the polar map each time.
        self.zoom_centre =
            match (self.zoom_boss, self.symbol("Map_drawMiddleObject"), self.symbol("_map")) {
                (Some((x, y)), Some(draw), Some(map)) => map_draw_offset(draw, map, &mut self.mem)
                    .map(|(ox, oy)| (ox.wrapping_add(x), oy.wrapping_add(y))),
                _ => None,
            };
        if self.zoom_from == ZOOM_FIRST {
            return Ok(false); // nothing to say about the counter itself
        }
        self.cpu.reg_set(Mode::User, 2, self.zoom_from as u32);
        let pc = self.cpu.reg_get(Mode::User, reg::PC);
        self.cpu.reg_set(Mode::User, reg::PC, pc.wrapping_add(4));
        Ok(true)
    }

    /// Note where the boss that just died is standing — one of the two
    /// `strb r3, [r7, #0x899]` in `HERO_Attack_Monster`.
    ///
    /// Reads the position and **declines**, so the guest sets its own flag. The
    /// coordinates rather than the pointer, because the object is dead: by the
    /// time the effect ends the guest may have freed it, and nothing here should
    /// depend on it still being there.
    pub(crate) fn host_zoom_kill(&mut self, rn: u8) -> Result<bool, String> {
        let monster = self.cpu.reg_get(Mode::User, rn);
        if monster != 0 {
            let (x, y) = (
                self.mem.r32(monster.wrapping_add(CHAR_X)) as i32,
                self.mem.r32(monster.wrapping_add(CHAR_Y)) as i32,
            );
            self.zoom_boss = Some((x, y));
        }
        Ok(false)
    }

    /// `n` consecutive guest words, as signed. The trig tables the radial blur
    /// reads are all this shape.
    fn read_words(&mut self, base: u32, n: u32) -> Vec<i32> {
        (0..n).map(|i| self.mem.r32(base.wrapping_add(i * 4)) as i32).collect()
    }

    /// Host `MIDASKernelManager::getAssetSizeFromVFS(name, &offset) -> size`.
    ///
    /// See `files::VfsIndex` for why: the guest's version re-scans the whole
    /// archive with two 4-byte `fread`s per record, every time.
    pub(crate) fn host_vfs_lookup(&mut self) -> Result<bool, String> {
        let name_p = self.cpu.reg_get(Mode::User, 1);
        let out_p = self.cpu.reg_get(Mode::User, 2);
        if name_p == 0 {
            return Ok(false);
        }
        // Hash the raw bytes: the guest's own hash walks `unsigned char`s, and
        // a lossy UTF-8 decode would rewrite any non-ASCII name before hashing.
        let name = self.mem.cstr_bytes(name_p);
        let hash = crate::files::vfs_hash(&name);

        let (off, size) = match self.vfs_index.get(hash) {
            Some(v) => v,
            // Not in the archive. The guest answers -1 for this, having walked
            // to the end; so do we, without the walk.
            None => (0, 0),
        };
        // `CS_VFS_DEBUG=1` names every asset the guest asks for and whether the
        // archive had it. The names are the only readable handle on what the
        // game thinks it is loading; a scene that draws nothing is usually a
        // miss here. Bytes, not `cstr`: asset names go through the same EUC-KR
        // rule as everything else the guest hands us.
        static VFS_DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *VFS_DEBUG.get_or_init(|| std::env::var("CS_VFS_DEBUG").is_ok()) {
            log::info!(
                "vfs {} {:?} -> {}",
                if size == 0 { "MISS" } else { "hit " },
                String::from_utf8_lossy(&name),
                size
            );
        }
        if size == 0 {
            self.cpu.reg_set(Mode::User, 0, u32::MAX);
        } else {
            if out_p != 0 {
                self.mem.write_bytes(out_p, &off.to_le_bytes());
            }
            self.cpu.reg_set(Mode::User, 0, size);
        }
        self.vfs_host += 1;

        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Rule through a real-money price and put `0` after it.
    ///
    /// Two lines in the game quote won and are now free, and both are drawn by
    /// `HF_DrawTextOutLine`, so both are served here and told apart by the call
    /// site:
    ///
    /// * the cash shop's, `MC_knlSprintk(buf, getInCodeString(0x2ea)
    ///   /* "|CASH &%d" */, price)` — `|` and `&` are the font's zero-width
    ///   colour escapes, so the visible run is `CASH ` and the digits;
    /// * the orb-removal confirm's, `getInCodeString(0x300)` =
    ///   `{실제현금 %d원&을 결제합니다.;결제 하시겠습니까?` with a literal 500.
    ///
    /// The second is why the price is *spliced* rather than appended: its digits
    /// sit mid-sentence, with `원을 결제합니다.` right behind them. Inserting
    /// ` 0` after the digits gives `실제현금 500 0원을 결제합니다.`, and the
    /// inserted `0` inherits the colour the digits already had, so no escape has
    /// to be restated. The shipped renderer then draws the whole thing.
    ///
    /// The rule's x comes from the guest's own measurer, but it takes two
    /// different calls to get it right, because `HF_GetWidth2` is not quite the
    /// draw loop:
    ///
    /// * `HF_DrawTextOutlineH2` starts the line at `x`, `x - w/2` or `x - w`
    ///   according to `flags`, with `w = HF_GetWidth(s, len, font)` where
    ///   `font = (c2 >= 0)` — *not* `c2` itself (`mvn`/`lsr #31` at `+0x70`).
    ///   The cash shop passes `flags = 0`, the orb confirm `flags = 1`.
    /// * a glyph's x is `line start + HF_GetWidth(s, i, 0)`, with `font` **0**:
    ///   the measurer adds a pixel per ASCII glyph when `font` is set that the
    ///   draw loop does not, and the 1 it seeds its accumulator with is exactly
    ///   the outline pixel the blit adds (`add r3, r8, sl`), so the two cancel
    ///   and no fudge is needed.
    ///
    /// Both measurements stop at the first `;` or NUL, which is what makes them
    /// line-1 quantities on a two-line popup. Everything is checked before
    /// anything is drawn — the site, both helpers, the digit run, the room in
    /// the scratch buffer — and any surprise declines, leaving the shipped
    /// price on screen.
    ///
    /// This is the one patch here that is *not* pixel-identical to the original
    /// by design. It rides `CS_FREE_CASH`, alongside the payment answering in
    /// `hle.rs` that makes the price true.
    pub(crate) fn host_cash_price(&mut self) -> Result<bool, String> {
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        // Also the recursion guard: `Vm::call` enters with `LR = RETURN_MAGIC`,
        // so the re-issued draw below declines.
        let at = |vm: &Self, sym: &str, off: u32| {
            vm.symbol(sym).map(|a| a.wrapping_add(off)) == Some(lr)
        };
        if !at(self, "NpcUI_DrawMerChant_Info", CASH_PRICE_RET)
            && !at(self, "NpcUI_drawOrbRemovePayment", ORB_PRICE_RET)
        {
            return Ok(false);
        }
        let (Some(draw), Some(get_width)) =
            (self.symbol("HF_DrawTextOutLine"), self.symbol("HF_GetWidth"))
        else {
            return Ok(false);
        };

        let sp = self.cpu.reg_get(Mode::User, reg::SP);
        let fb = self.cpu.reg_get(Mode::User, 0);
        let x = self.cpu.reg_get(Mode::User, 1) as i32;
        let y = self.cpu.reg_get(Mode::User, 2) as i32;
        let s = self.cpu.reg_get(Mode::User, 3);
        let (len, c1, c2, flags) =
            (self.mem.r32(sp), self.mem.r32(sp + 4), self.mem.r32(sp + 8), self.mem.r32(sp + 0xc));

        // Bytes, not `str`: these strings are EUC-KR and go back to the guest.
        // Walked the way the renderer walks them — a byte with bit 7 set is a
        // two-byte syllable and is stepped over whole — so an ASCII digit here
        // can only be part of the price, never the trail byte of a pair. The
        // scan is also the guard at any site that draws more than one string:
        // one without digits declines.
        let text = self.mem.cstr_bytes(s);
        let mut i = 0;
        let head = loop {
            match text.get(i) {
                // Line 2 is not where a price is.
                None | Some(b';') => return Ok(false),
                Some(&b) if b >= 0x80 => i += 2,
                Some(&b) if b.is_ascii_digit() => break i,
                Some(_) => i += 1,
            }
        };
        let mut tail = head;
        while text.get(tail).is_some_and(u8::is_ascii_digit) {
            tail += 1;
        }

        let mut out = Vec::with_capacity(text.len() + 3);
        out.extend_from_slice(&text[..tail]);
        out.extend_from_slice(b" 0");
        out.extend_from_slice(&text[tail..]);
        out.push(0);
        if out.len() > CASH_SCRATCH {
            return Ok(false);
        }
        let Some(buf) = self.cash_scratch() else { return Ok(false) };
        self.mem.write_bytes(buf, &out);

        // Where the renderer will start the line, and the pen offsets of the two
        // ends of the digit run. `head`/`tail` still index `buf`: the splice
        // goes in behind them.
        let font = u32::from(c2 as i32 >= 0);
        let w_line = self.call(get_width, &[buf, len, font])? as i32;
        let pen = match flags {
            1 => x - (w_line >> 1),
            2 => x - w_line,
            _ => x,
        };
        let x0 = pen + self.pen_offset(buf, &out, head)?;
        let x1 = pen + self.pen_offset(buf, &out, tail)?;

        self.call(draw, &[fb, x as u32, y as u32, buf, len, c1, c2, flags])?;
        self.strike_through(fb, x0, x1, y + strike_y());

        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// The one entry hook on `getInCodeString`, doing two unrelated jobs.
    ///
    /// **데이터 관리's fee wording.** `getInCodeString(id)` is `return table[id]`
    /// and nothing else, so an id-for-id swap at the entry needs no memory and
    /// no control-flow change; declining afterwards lets the guest do the lookup
    /// with the new id. Only the wording changes — both confirms still appear,
    /// and the second one still warns that the server copy is about to be
    /// overwritten, which is a real destructive action against a protocol
    /// `cs_server` does not implement.
    ///
    /// **The 「내 ID」 tile's label.** This one cannot use the id swap: the
    /// string is ours, not the table's. So it answers `r0` with a pointer into
    /// the guest heap and returns — the guest's own body never runs, which is
    /// the whole of the function anyway.
    pub(crate) fn host_in_code_string(&mut self) -> Result<bool, String> {
        let id = self.cpu.reg_get(Mode::User, 0);
        if crate::net::free_cash() && FEE_TEXT_IDS.contains(&id) {
            self.cpu.reg_set(Mode::User, 0, id + FEE_TEXT_FREE);
            return Ok(false);
        }
        // The letter screen's two phone-number strings. Same shape as the tile
        // label below: our bytes, `r0` answered, straight back to the caller.
        if id_recipient() {
            let host = match id {
                LETTER_ASK_ID_ID => Some((&mut self.letter_ask_msg, LETTER_ASK_ID)),
                LETTER_NO_ID_ID => Some((&mut self.letter_none_msg, LETTER_NO_ID)),
                _ => None,
            };
            if let Some((slot, bytes)) = host {
                let p = match *slot {
                    Some(p) => p,
                    None => {
                        let p = self.alloc_cstr(bytes)?;
                        // Reborrow: `alloc_cstr` needs `&mut self`.
                        match id {
                            LETTER_ASK_ID_ID => self.letter_ask_msg = Some(p),
                            _ => self.letter_none_msg = Some(p),
                        }
                        p
                    }
                };
                self.cpu.reg_set(Mode::User, 0, p);
                let lr = self.cpu.reg_get(Mode::User, reg::LR);
                self.cpu.reg_set(Mode::User, reg::PC, lr);
                return Ok(true);
            }
        }
        // 펫 「스마트」 — the fifth 전투타입's label and description. Both are
        // ids the shipped four already use, so both are gated on the call
        // site: the label's id is `0x12f + value` and so identifies itself,
        // while the description's arm is shared with 단독 전투 and is told
        // apart by the dispatch hook that routed us there.
        if pet_smart() {
            let lr = self.cpu.reg_get(Mode::User, reg::LR);
            let ours = if id == PET_LABEL_ID {
                self.symbol("draw_char_pet").map(pet_label_lr) == Some(lr)
            } else if id == PET_DESC_ID && self.pet_smart_desc {
                self.symbol("draw_pet_info").map(pet_desc_lr) == Some(lr)
            } else {
                false
            };
            if std::env::var_os("CS_PET_DEBUG").is_some()
                && (id == PET_LABEL_ID || id == PET_DESC_ID)
            {
                log::info!(
                    "pet string: id {id:#x} lr {lr:#x} desc_flag {} ours {ours}",
                    self.pet_smart_desc
                );
            }
            if ours {
                let bytes = if id == PET_LABEL_ID { PET_SMART_LABEL } else { PET_SMART_DESC };
                let slot = if id == PET_LABEL_ID { self.pet_label } else { self.pet_desc_msg };
                let p = match slot {
                    Some(p) => p,
                    None => {
                        let p = self.alloc_cstr(bytes)?;
                        if id == PET_LABEL_ID {
                            self.pet_label = Some(p);
                        } else {
                            self.pet_desc_msg = Some(p);
                        }
                        p
                    }
                };
                self.cpu.reg_set(Mode::User, 0, p);
                self.cpu.reg_set(Mode::User, reg::PC, lr);
                return Ok(true);
            }
        }
        // 「레시피2」 — the fifth shop-category label. Every other caller of
        // this id wants 아이템갯수:, so the swap is gated on the draw loop's
        // own return address and not on the id alone.
        if self.recipe2_armed && id == RECIPE2_LABEL_ID {
            let lr = self.cpu.reg_get(Mode::User, reg::LR);
            if self.symbol("NpcUI_DrawMerChant_Main").map(recipe2_label_lr) == Some(lr) {
                let p = match self.recipe2_label {
                    Some(p) => p,
                    None => {
                        let p = self.alloc_cstr(RECIPE2_LABEL)?;
                        self.recipe2_label = Some(p);
                        p
                    }
                };
                self.cpu.reg_set(Mode::User, 0, p);
                self.cpu.reg_set(Mode::User, reg::PC, lr);
                return Ok(true);
            }
        }
        if my_id_tile() && id == MY_ID_LABEL_ID {
            let p = match self.my_id_label {
                Some(p) => p,
                None => {
                    let p = self.alloc_cstr(MY_ID_LABEL)?;
                    self.my_id_label = Some(p);
                    p
                }
            };
            self.cpu.reg_set(Mode::User, 0, p);
            let lr = self.cpu.reg_get(Mode::User, reg::LR);
            self.cpu.reg_set(Mode::User, reg::PC, lr);
            return Ok(true);
        }
        // 공용창고: tile 1's label, and — while the chest owns the letter frame
        // — the bag grid's heading, emptied so the two panel words can have
        // that band. The heading is only emptied for *that* call, because the
        // same id is the letter's own 보낼 아이템 선택.
        let vault_label = vault_on() && id == VAULT_LABEL_ID;
        let vault_title = self.vault_screen == crate::vault::Screen::Grid
            && id == VAULT_TITLE_ID
            && self.symbol("draw_CommonItembag").map(|b| b + VAULT_BAG_DRAW_STRING + 4)
                == Some(self.cpu.reg_get(Mode::User, reg::LR));
        if vault_label || vault_title {
            let cached = if vault_label { self.vault_label } else { self.vault_blank };
            let p = match cached {
                Some(p) => p,
                None => {
                    let p = self.alloc_cstr(if vault_label { VAULT_LABEL } else { b"" })?;
                    if vault_label {
                        self.vault_label = Some(p);
                    } else {
                        self.vault_blank = Some(p);
                    }
                    p
                }
            };
            self.cpu.reg_set(Mode::User, 0, p);
            let lr = self.cpu.reg_get(Mode::User, reg::LR);
            self.cpu.reg_set(Mode::User, reg::PC, lr);
            return Ok(true);
        }
        Ok(false)
    }

    /// Put one of the game's own popups on screen with our text.
    ///
    /// `setPopUp` sets `mainFrame[0x8c]`, and everything after that is the
    /// guest's: `MainMenuFrame_Render` draws `drawPopWindow` from a tail every
    /// state arm reaches, and `MainMenuFrame_KeyPressed+0x250` hands it every
    /// key while the byte is set. The one caller-side rule is that the text is
    /// a **format string** (`MC_knlSprintk`), which is why nothing here ever
    /// splices a value the player typed.
    ///
    /// The buffer is reused. These are answers to button presses, and a fresh
    /// guest heap block per press would leak one per press.
    pub(crate) fn popup(&mut self, text: &[u8]) -> Result<bool, String> {
        let Some(set_popup) = self.symbol("setPopUp") else { return Ok(false) };
        let p = match self.popup_msg {
            Some(p) if self.popup_len >= text.len() => {
                for (i, b) in text.iter().enumerate() {
                    self.mem.w8(p + i as u32, *b);
                }
                self.mem.w8(p + text.len() as u32, 0);
                p
            }
            _ => {
                // Grow once to the longest thing we can say, so the common case
                // never reallocates.
                let n = text.len().max(0x60);
                let p = self.heap.alloc(n as u32 + 1).ok_or("no guest heap for a popup")?;
                self.popup_msg = Some(p);
                self.popup_len = n;
                for (i, b) in text.iter().enumerate() {
                    self.mem.w8(p + i as u32, *b);
                }
                self.mem.w8(p + text.len() as u32, 0);
                p
            }
        };
        // **Any popup that is not the address one clears the flag.** One rule
        // in one place: `host_my_id_key` re-arms it right after raising its own,
        // so a claim result or a refusal can never be mistaken for it.
        self.my_id_popup = false;
        self.call(set_popup, &[p, 0])?;
        Ok(true)
    }

    /// Open the keyboard to type an ID.
    ///
    /// Same plumbing as `open_pet_rename`, and for the same reason: the guest's
    /// IME is self-sufficient once `InputInit` has run — `CUSTOM_grpFlushLcd`
    /// paints it over whatever is on screen and `clsidAuthHandle` reroutes
    /// events to it, so no new drawing or input path is needed.
    ///
    /// Two differences. The length cap goes from the shipped 11 to
    /// [`crate::identity::ID_MAX_BYTES`], because that is what the handshake
    /// field allows. And **no initial text**: `open_pet_rename` seeds the
    /// current name, but seeding the minted number here would offer the player
    /// an all-digit ID, which `identity::check_id` refuses on purpose — the
    /// minted numbers are the namespace every mailbox key already lives in.
    pub fn open_id_input(&mut self) -> Result<bool, String> {
        let (Some(input_init), Some(set_prop), Some(letter)) = (
            self.symbol("InputInit"),
            self.symbol("MX_imeSetProperty"),
            self.symbol("_mLetterState"),
        ) else {
            return Ok(false);
        };
        // Never over a letter being composed: `onCloseIME_MainFrame` would
        // route that text to `_mailBox` instead, and our hook would have taken
        // it first.
        let state = self.mem.r8(letter) as i8;
        if state == LETTER_STATE_RECIPIENT || state == LETTER_STATE_BODY {
            return Ok(false);
        }
        self.call(input_init, &[IME_MODE_PET])?;
        self.call(set_prop, &[IME_PROP_MAXLEN, crate::identity::ID_MAX_BYTES as u32])?;
        // **Clear the field.** `InputInit` seeds it with
        // `getInCodeString(0x7c)` = 가름, which is a sensible default for the
        // pet it was written for and a trap here: a player who just presses
        // 완료 would claim a name they never chose, and the server would agree.
        //
        // `open_pet_rename`'s comment says never to write an empty string
        // through property 2 — that is about the *guest's* own close handler,
        // which answers a zero-length field with 펫이름을 입력해 주세요 and
        // reopens. Our hook returns before that body runs, so an empty field
        // reaches `identity::check_id`, which calls it `Empty` and says so.
        //
        // Property 2 alone is not enough: it terminates the buffer without
        // clearing it, and the first inserted character brings the tail back
        // (see [`IME_BUF`]). Zero the whole thing, then let property 2 set the
        // length — that way both halves of the guest's own state agree.
        if let Some(ime) = self.symbol("_midas_internalImeInstance") {
            for i in 0..IME_BUF_LEN {
                self.mem.w8(ime + IME_BUF + i, 0);
            }
        }
        // **Seed with the name they already hold**, so changing an ID is an
        // edit rather than a retype. Only a claimed one: the minted number is
        // all digits, which `identity::check_id` refuses on purpose, and
        // offering it as a starting point would be offering a dead end.
        let seed = self.ident.is_claimed().then(|| self.ident.wire().to_vec());
        let buf = self.ime_buffer()?;
        match &seed {
            Some(name) if name.len() < IME_SCRATCH as usize => {
                for (i, b) in name.iter().enumerate() {
                    self.mem.w8(buf + i as u32, *b);
                }
                self.mem.w8(buf + name.len() as u32, 0);
            }
            _ => self.mem.w8(buf, 0),
        }
        self.call(set_prop, &[IME_PROP_TEXT, buf])?;
        self.id_pending = true;
        match &seed {
            Some(name) => log::info!("id: keyboard open, seeded with {name:?}"),
            None => log::info!("id: keyboard open for a new ID"),
        }
        Ok(true)
    }

    /// The reused guest buffer the IME reads into and writes out of.
    fn ime_buffer(&mut self) -> Result<u32, String> {
        match self.ime_scratch {
            Some(p) => Ok(p),
            None => {
                let p = self.heap.alloc(IME_SCRATCH).ok_or("no guest heap for the IME")?;
                self.ime_scratch = Some(p);
                Ok(p)
            }
        }
    }

    /// Keep the recipient the player already typed.
    ///
    /// `MainMenuLetterSendKey` erases `_mailBox+0x42..0x50` with eight `strh`
    /// **before** it calls `InputInit`, so by the time the input opens the old
    /// text is gone. Re-opening 받는사람 to fix a typo would hand back an empty
    /// box and make the player retype the whole thing.
    ///
    /// This reads it one instruction earlier — at the first of those stores —
    /// and then **declines**, so the guest still does its own erase and nothing
    /// about its state changes. `host_letter_recipient` seeds the keyboard with
    /// what was read a few instructions later.
    ///
    /// Nothing is remembered across screens: the copy is taken from the guest's
    /// own field every time, so when the guest clears it for a fresh letter the
    /// seed is empty too.
    pub(crate) fn host_letter_keep_recipient(&mut self) -> Result<bool, String> {
        self.letter_last_to.clear();
        if !id_recipient() {
            return Ok(false);
        }
        if let Some(mail) = self.symbol("_mailBox") {
            let to = self.mem.cstr_bytes(mail + MAILBOX_TO);
            if !to.is_empty() && to.len() < MAILBOX_TO_LEN as usize {
                self.letter_last_to = to;
            }
        }
        Ok(false) // the guest's own erase still runs
    }

    /// Say why a letter bounced.
    ///
    /// `cs_server` refuses a letter addressed to an ID nobody registered, and
    /// the guest turns any non-zero status into `PocErrorCode(status)`. Left
    /// alone that draws `<ERROR:9401>` — the fallback format, because
    /// `_ERROR_CNT` is 0 and the error-script table it would otherwise index is
    /// empty in this build.
    ///
    /// **The shipped body runs first.** It clears `netSocket[0x17]` and
    /// `[0x18]` before drawing anything, and leaving those set is precisely the
    /// shape of the cash shop's old "무한 로딩" — a screen waiting on a reply
    /// that already came. So this calls it through [`Vm::call`] and then puts
    /// its own text in the same popup; `setPopUp` just reformats
    /// `mainFrame+0x8f` and re-sets the flag, so the second call wins and no
    /// bookkeeping is skipped.
    pub(crate) fn host_send_error(&mut self) -> Result<bool, String> {
        if !id_recipient() || self.cpu.reg_get(Mode::User, 0) != ERR_NO_SUCH_ID {
            return Ok(false); // every other error is the guest's to explain
        }
        if self.cpu.reg_get(Mode::User, reg::LR) == crate::RETURN_MAGIC {
            return Ok(false); // our own `Vm::call` of the shipped body
        }
        let Some(poc) = self.symbol("PocErrorCode") else { return Ok(false) };
        self.call(poc, &[ERR_NO_SUCH_ID])?;
        self.popup(NO_SUCH_ID_MSG)?;
        log::info!("letter: refused — the recipient is not a registered ID");
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Let the letter's recipient line take an ID instead of a phone number.
    ///
    /// An entry hook on `InputInit`. The guest's own body runs first, through
    /// [`Vm::call`] — it registers the close callback, sets the mask, the cap
    /// and the initial text — and then three properties are set again over the
    /// top. Running it rather than reproducing it is the point: `MX_imeCall`
    /// wires `onCloseIME_MainFrame` and touches state this hook has no business
    /// knowing about.
    ///
    ///   * mask `8` (digits) -> `0xe`, the same set the pet name uses.
    ///   * cap `0xb` -> [`crate::identity::ID_MAX_BYTES`], the handshake bound.
    ///   * initial text `"010"` -> empty. That default is the one thing a
    ///     player cannot get rid of by typing, and it is not part of any ID.
    ///
    /// Scoped to the recipient: mode 0 **and** `_mLetterState == 2`. The letter
    /// body shares this entry with a different state and is left alone.
    ///
    /// Recursion is impossible by construction — the guest body is reached via
    /// `Vm::call`, which sets `LR = RETURN_MAGIC`, and the hook declines when it
    /// sees that.
    pub(crate) fn host_letter_recipient(&mut self) -> Result<bool, String> {
        if !id_recipient() || self.cpu.reg_get(Mode::User, 0) != IME_MODE_LETTER {
            return Ok(false);
        }
        if self.cpu.reg_get(Mode::User, reg::LR) == crate::RETURN_MAGIC {
            return Ok(false); // our own `Vm::call` of the shipped body
        }
        let (Some(input_init), Some(set_prop), Some(letter)) = (
            self.symbol("InputInit"),
            self.symbol("MX_imeSetProperty"),
            self.symbol("_mLetterState"),
        ) else {
            return Ok(false);
        };
        if self.mem.r8(letter) as i8 != LETTER_STATE_RECIPIENT {
            return Ok(false); // the body, or something else entirely
        }
        self.call(input_init, &[IME_MODE_LETTER])?;
        self.call(set_prop, &[IME_PROP_MASK, IME_MASK_NAME])?;
        self.call(set_prop, &[IME_PROP_MAXLEN, crate::identity::ID_MAX_BYTES as u32])?;
        // Zero the field the same way `open_id_input` does — property 2 alone
        // terminates without erasing, and the first character typed brings the
        // old tail back (see [`IME_BUF`]).
        if let Some(ime) = self.symbol("_midas_internalImeInstance") {
            for i in 0..IME_BUF_LEN {
                self.mem.w8(ime + IME_BUF + i, 0);
            }
        }
        // Then put back whatever was already there, captured by
        // `host_letter_keep_recipient` before the guest erased it. Empty on a
        // fresh letter, which is the point — the shipped `"010"` is gone either
        // way.
        let seed = std::mem::take(&mut self.letter_last_to);
        let buf = self.ime_buffer()?;
        for (i, b) in seed.iter().enumerate() {
            self.mem.w8(buf + i as u32, *b);
        }
        self.mem.w8(buf + seed.len() as u32, 0);
        self.call(set_prop, &[IME_PROP_TEXT, buf])?;
        if !seed.is_empty() {
            log::info!("id: the letter keeps {seed:?} for editing");
        }
        log::info!("id: the letter asks for an ID, not a number");
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Take the typed ID instead of letting the guest file it as a pet name.
    ///
    /// An entry hook on `onCloseIME_MainFrame`, live **only** while our own
    /// keyboard is up. When it fires it runs none of the guest's body — `PC` goes
    /// straight to `LR` — so `mainFrame+0xf3`, `nameTitleIdx`, `_mailBox` and
    /// `_mLetterState` are all left exactly as they were, and the pet-naming and
    /// letter paths are untouched.
    ///
    /// **`r0` decides.** The shipped prologue is `cmp r0, #1 / bls`, which
    /// accepts **0 (done) and 1 (cancelled) alike** — the cancel path has
    /// already restored the initial string by then, so a hook that ignored `r0`
    /// would confirm whatever the field started with. For the pet that is
    /// harmless (the old name); for an ID it would claim a name nobody typed.
    pub(crate) fn host_id_input_done(&mut self) -> Result<bool, String> {
        // 공용창고's amount keypad, opened by `open_vault_gold_input`. Same
        // contract as the ID below: none of the guest's body runs, and `r0`
        // decides whether anything was typed at all.
        if let Some(op) = self.vault_gold_input.take() {
            let cancelled = self.cpu.reg_get(Mode::User, 0) != 0;
            let lr = self.cpu.reg_get(Mode::User, reg::LR);
            self.cpu.reg_set(Mode::User, reg::PC, lr);
            if cancelled {
                log::info!("vault: keypad cancelled; no gold moved");
                return Ok(true);
            }
            let Some(get_str) = self.symbol("MX_imeGetString") else { return Ok(true) };
            let buf = self.ime_buffer()?;
            for i in 0..IME_SCRATCH {
                self.mem.w8(buf + i, 0);
            }
            self.call(get_str, &[buf])?;
            let typed = self.mem.cstr_bytes(buf);
            self.vault_gold_move(op, gold_amount(&typed))?;
            return Ok(true);
        }
        if !self.id_pending {
            return Ok(false);
        }
        self.id_pending = false;
        let cancelled = self.cpu.reg_get(Mode::User, 0) != 0;
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        if cancelled {
            log::info!("id: keyboard cancelled; nothing claimed");
            return Ok(true);
        }
        let Some(get_str) = self.symbol("MX_imeGetString") else { return Ok(true) };
        // 0x64, the size the guest's own `memset` uses before the same call. The
        // cap we set is 14, but sizing this to the cap would trust a property
        // whose unit (bytes or characters) we have not proved.
        let buf = self.ime_buffer()?;
        for i in 0..IME_SCRATCH {
            self.mem.w8(buf + i, 0);
        }
        self.call(get_str, &[buf])?;
        let typed = self.mem.cstr_bytes(buf);
        match self.ident.propose(&typed) {
            Ok(folded) => {
                let folded = folded.to_vec();
                let install = self.ident.install().to_owned();
                let was = self.wire_identity();
                let claimer = self.claimer.get_or_insert_with(crate::idclaim::Claimer::start);
                if claimer.submit(&install, &folded, &was) {
                    self.claim_since = self.wall_now_ms().or(Some(0));
                    log::info!("id: asking the server about {folded:?}");
                } else {
                    self.ident.withdraw();
                    self.popup(ID_OFFLINE_MSG)?;
                }
            }
            Err(why) => {
                // No round trip for something we can already tell is wrong, and
                // no popup either: the keyboard is the place to say it, and the
                // player has just closed it. A line in the log is for us.
                log::info!("id: {typed:?} refused ({why:?})");
                self.popup(ID_REFUSED_MSG)?;
            }
        }
        Ok(true)
    }

    /// A NUL-terminated copy of `bytes` on the guest heap.
    ///
    /// Kept by the caller and reused: these are answers to button presses, and
    /// allocating per press would leak a block a press.
    fn alloc_cstr(&mut self, bytes: &[u8]) -> Result<u32, String> {
        let p = self.heap.alloc(bytes.len() as u32 + 1).ok_or("no guest heap for a string")?;
        for (i, b) in bytes.iter().enumerate() {
            self.mem.w8(p + i as u32, *b);
        }
        self.mem.w8(p + bytes.len() as u32, 0);
        Ok(p)
    }

    /// Show this install's address instead of walking into a dead menu.
    ///
    /// Stands in for `*_menuState = (s8)_menuCursor` when the cursor is on tile
    /// 7. Skipping that one store leaves `_menuState` at 0xc (the 3x3 grid), and
    /// the guest's own next instruction — `cmp r3, #8`, with `r3` the same
    /// cursor — then fails and branches to the epilogue. **The guest unwinds its
    /// own stack**; nothing here touches it.
    ///
    /// The popup is the game's: `setPopUp` sets `mainFrame[0x8c]`, and
    /// `MainMenuFrame_Render` draws `drawPopWindow` from a tail that *every*
    /// state arm reaches, including the 3x3 grid's. Input is the game's too —
    /// `MainMenuFrame_KeyPressed+0x250` hands every key to `PopWindowKey` while
    /// that byte is set. Same plumbing as `host_data_manage_blocked`.
    ///
    /// What it shows is [`crate::identity::wire_number`] of the minted number,
    /// **not** the number itself: `getSKTPhoneNumber` rebuilds the string on the
    /// way to the handshake and eats leading zeros from index 3 on, so for about
    /// one install in ten the two differ — and the one the letters arrive at is
    /// the wire form. Showing the stored one would rebuild the exact bug this
    /// tile exists to close.
    pub(crate) fn host_my_id_key(&mut self) -> Result<bool, String> {
        // 편지, retired: let the store run, but with the number a press on a
        // shipped grey tile stores. `r2` is dead after the store (`cmp r3, #8`
        // reads the cursor from `r3`), so changing it and declining is the
        // whole of it — the guest then does exactly what it did for tile 7.
        if self.letter_retired && self.cpu.reg_get(Mode::User, 2) == LETTER_TILE {
            self.cpu.reg_set(Mode::User, 2, UNUSED_MENU_STATE);
            return Ok(false);
        }
        // 공용창고 is tile 1, the other one that shipped grey. Same
        // instruction, same reason it is enough: skipping the store leaves
        // `_menuState` for us to write and the guest's own `cmp r3, #8` then
        // takes it to the epilogue.
        if vault_on() && self.cpu.reg_get(Mode::User, 2) == VAULT_TILE {
            let pc = self.cpu.reg_get(Mode::User, reg::PC);
            self.cpu.reg_set(Mode::User, reg::PC, pc.wrapping_add(4));
            self.vault_enter()?;
            return Ok(true);
        }
        if self.cpu.reg_get(Mode::User, 2) != MY_ID_TILE {
            return Ok(false); // every other tile is the guest's
        }
        if !my_id_tile() {
            return Ok(false);
        }
        // Skip the store either way: the menu stays on the 3x3 grid whether we
        // put up a popup or a keyboard, and the guest's own `cmp r3, #8` then
        // takes it to the epilogue.
        let pc = self.cpu.reg_get(Mode::User, reg::PC);
        self.cpu.reg_set(Mode::User, reg::PC, pc.wrapping_add(4));

        // A claim is already in flight — say nothing rather than start a second.
        if self.claim_since.is_some() {
            return Ok(true);
        }
        let text = match self.my_id_text {
            Some(p) => p,
            None => {
                // **The address is the button.** A touch anywhere on this
                // text opens the keyboard (`host_my_id_popup_touch`), so there
                // is nothing to explain and no third line to fit.
                //
                // With no ID claimed there is no address to show — the minted
                // number is not one any more, because the server refuses it
                // (see [`MY_ID_NONE`]). Say that instead of printing something
                // a friend would copy down and get a bounce from.
                let msg = if self.ident.is_claimed() {
                    let mut msg = MY_ID_TITLE.to_vec();
                    // **`wire_identity`, not the stored value.** The tile and
                    // `host_phone_number` must answer the same question with
                    // the same string; see `Vm::wire_identity`.
                    msg.extend_from_slice(&self.wire_identity());
                    msg
                } else {
                    MY_ID_NONE.to_vec()
                };
                let p = self.alloc_cstr(&msg)?;
                self.my_id_text = Some(p);
                p
            }
        };
        let Some(set_popup) = self.symbol("setPopUp") else { return Ok(true) };
        self.call(set_popup, &[text, 0])?;
        // From here a touch on the text means "change it" — but only while IDs
        // are reported, or the name would go somewhere the server never hears.
        self.my_id_popup = id_as_phone();
        Ok(true)
    }

    /// Answer `getSKTPhoneNumber(out, len_out)` with the identity **verbatim**.
    ///
    /// The shipped function does not pass its property through — it rebuilds it:
    /// a string that does not already start with `'0'` gets one prepended, and
    /// leading zeros from index 3 on are eaten. For a phone number that is
    /// harmless bookkeeping. For a **name** it is not: 홍길동 reaches the server
    /// as `0홍길동`, which is what the player sees on the tile and what their
    /// letters are keyed by — a `'0'` nobody typed and nobody can remove.
    ///
    /// Worse, it splits the ID in two. The claim goes over the control channel
    /// carrying the bytes the player typed, so `ids.json` reserves 홍길동 while
    /// the handshake opens a mailbox called `0홍길동`. Measured on the live
    /// server before this hook existed.
    ///
    /// So the fix is the one `LETTER_ID.md` §3 settled on: hook the **entry**,
    /// write the string as-is, fill in the length, return. Two side effects and
    /// both are wanted — the handshake and the 데이터 관리 packet (which calls
    /// `MC_knlGetSystemProperty` directly, at `@0x305cc`) stop disagreeing, and
    /// the ~10 % of pre-fix minted numbers whose stored and wire forms differ
    /// stop differing.
    ///
    /// That last one is a **migration**: such an install's mailbox key changes.
    /// No live one is affected (every key on the server today survives the
    /// transform unchanged) and `mint` has not been able to produce a divergent
    /// number since 2026-08-16, but `Vm::boot` warns when it sees one.
    pub(crate) fn host_skt_phone_number(&mut self) -> Result<bool, String> {
        let (out, len_out) = (self.cpu.reg_get(Mode::User, 0), self.cpu.reg_get(Mode::User, 1));
        if out == 0 {
            return Ok(false);
        }
        let id = self.ident.wire().to_vec();
        // The field is 16 bytes including the NUL (`identity::HANDSHAKE_FIELD`),
        // and unlike the shipped path there is no `'0'` to make room for.
        if id.len() >= crate::identity::HANDSHAKE_FIELD {
            log::error!(
                "identity: {:?} is {} bytes, too long for the handshake field; \
                 letting the guest build it instead",
                String::from_utf8_lossy(&id),
                id.len()
            );
            return Ok(false);
        }
        for (i, b) in id.iter().enumerate() {
            self.mem.w8(out + i as u32, *b);
        }
        self.mem.w8(out + id.len() as u32, 0);
        if len_out != 0 {
            self.mem.w32(len_out, id.len() as u32);
        }
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Touch the address to change it.
    ///
    /// An entry hook on `PopWindowKey`, live only while the 「내 ID」 popup is the
    /// one on screen. The plumbing it borrows is two instructions long:
    ///
    /// ```text
    ///   MainMenuFrame_KeyPressed+0x250
    ///     bl PopWindowKey
    ///     cmp r0, #0
    ///     strbne r7, [r8, #0x8c]     ; nonzero -> mainFrame[0x8c] = 0, popup closes
    /// ```
    ///
    /// So answering 1 is exactly "this press dismissed the popup", which is what
    /// the OK button already does. We open the keyboard first and then say 1, and
    /// the guest tidies its own popup away underneath it.
    ///
    /// The target is **the whole message area** — the box minus the OK button —
    /// rather than a rectangle fitted to the address line. Two reasons: the text
    /// is centred by `HF_DrawTextAnchorH2` and its line advance is not a constant
    /// this codebase has measured, and the popup says exactly one thing, so there
    /// is nothing else in that area to hit by accident. It also makes the target
    /// large, which matters on a 520x240 logical screen.
    pub(crate) fn host_my_id_popup_touch(&mut self) -> Result<bool, String> {
        if !self.my_id_popup {
            return Ok(false); // every other popup is the guest's
        }
        // `PopWindowKey(event, x, y)`; 0x13 is the touch event, the same value
        // `MainMenuFrame_KeyPressed` dispatches on.
        let (event, x, y) = (
            self.cpu.reg_get(Mode::User, 0) as i32,
            self.cpu.reg_get(Mode::User, 1) as i32,
            self.cpu.reg_get(Mode::User, 2) as i32,
        );
        if event != TOUCH_EVENT {
            return Ok(false); // keys still close it the way they always did
        }
        let Some(lcd) = self.symbol("_lcdG") else { return Ok(false) };
        let (cx, cy) = (self.mem.r32(lcd + 0x4c) as i32 >> 1, self.mem.r32(lcd + 0x50) as i32 >> 1);
        let (left, top) = (cx + POP_BOX_DX, cy + POP_BOX_DY);
        let inside = (left..left + POP_BOX_W).contains(&x) && (top..cy + POP_OK_DY).contains(&y);
        if !inside {
            return Ok(false); // OK, or outside the box: the guest decides
        }
        if !self.open_id_input()? {
            return Ok(false);
        }
        self.my_id_popup = false;
        self.my_id_text = None; // the address may be about to change
        self.cpu.reg_set(Mode::User, 0, 1); // "handled" — the guest closes the popup
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Draw tile 7 like a live tile.
    ///
    /// `r7` is the tile index — the draw loop builds it two instructions earlier
    /// (`add r7, r4, r2`, column plus row*3) and hands it to `getImage` as the
    /// icon frame. Answering the grey test with 0 for that one tile and resuming
    /// at the `cmp` runs the shipped ordinary-tile path: icon, label, full
    /// alpha. Tile 1 still computes its own grey and stays grey.
    pub(crate) fn host_my_id_tile(&mut self) -> Result<bool, String> {
        // The tile grid is on screen, so the chest is not — whatever path the
        // player left it by. Asserting it here rather than on each exit is what
        // keeps `vault_screen` from outliving the screen and hijacking 편지,
        // which does not pass through the tile commit hook at all.
        self.vault_off();
        // The first drawing of the tile menu is also the earliest moment a
        // move a crash left half-done can be put right — `_saveSlot` is filled
        // in by now and the player has not yet started a game on the slot it
        // belongs to. Once per process; see `Vm::vault_resume`.
        if vault_on() && !self.vault_resumed {
            self.vault_resumed = true;
            if self.vault.in_flight() {
                match self.vault_resume(true) {
                    Ok(Some(what)) => log::info!("vault: the move left in flight was {what:?}"),
                    Ok(None) => {}
                    Err(why) => log::error!("vault: could not settle the move in flight: {why}"),
                }
            }
        }
        let tile = self.cpu.reg_get(Mode::User, 7);
        let Some(base) = self.symbol("MainMenuFrame_drawMainMenu") else { return Ok(false) };
        // 편지, retired: the shipped grey box, drawn in its own column (the
        // shipped path's `x` is column 1's), then back into the loop as grey.
        if self.letter_retired && tile == LETTER_TILE {
            return self.draw_retired_tile(base);
        }
        let live = (my_id_tile() && tile == MY_ID_TILE) || (vault_on() && tile == VAULT_TILE);
        if !live {
            return Ok(false);
        }
        self.cpu.reg_set(Mode::User, 5, 0);
        self.cpu.reg_set(Mode::User, reg::PC, base.wrapping_add(MY_ID_GREY_TEST));
        Ok(true)
    }

    /// The shipped grey box for a tile outside column 1.
    ///
    /// Same call, same size, same alpha, same `y` — only the `x` moves by the
    /// loop's own column offset (`r6`, which the shipped path never adds). Then
    /// `fp = r0` (the `mov` the hook stands in front of), `r5 = 1` (grey) and
    /// the loop resumes at `cmp r5, #0`, which is where the shipped grey path
    /// arrives after its own call; nothing between is skipped because nothing
    /// between is there. `letter_sites` has checked every word this relies on.
    fn draw_retired_tile(&mut self, base: u32) -> Result<bool, String> {
        let (Some(round), Some(lcd)) = (self.symbol("UI_RoundRect"), self.symbol("_lcdG")) else {
            return Ok(false);
        };
        let g = self.mem.r32(lcd + 4);
        let (w, h) = (self.mem.r32(lcd + 0x4c) as i32, self.mem.r32(lcd + 0x50) as i32);
        let col = self.cpu.reg_get(Mode::User, 6) as i32; // column * 0x2e
        let row = self.cpu.reg_get(Mode::User, 9) as i32; // row * 0x2e
        let x = (w >> 1) - MENU_GREY_DX - MENU_TILE_STRIDE + col;
        let y = (h >> 1) - MENU_GREY_DY + row;
        let icon = self.cpu.reg_get(Mode::User, 0);
        self.call(round, &[g, x as u32, y as u32, MENU_GREY_W, MENU_GREY_H, MENU_GREY_ALPHA])?;
        self.cpu.reg_set(Mode::User, 11, icon);
        self.cpu.reg_set(Mode::User, 5, 1);
        self.cpu.reg_set(Mode::User, reg::PC, base.wrapping_add(MENU_GREY_CONT));
        Ok(true)
    }

    /// Do not take the letter's fee branch: a press on the retired tile falls
    /// through to the commit like any other tile's. The branch has no side
    /// effect of its own and the next instruction starts with a fresh
    /// compare, so skipping it is the whole of it.
    pub(crate) fn host_letter_tile_branch(&mut self) -> Result<bool, String> {
        if !self.letter_retired {
            return Ok(false);
        }
        let pc = self.cpu.reg_get(Mode::User, reg::PC);
        self.cpu.reg_set(Mode::User, reg::PC, pc.wrapping_add(4));
        Ok(true)
    }

    /// Buy a cash item on one keypress instead of two.
    ///
    /// The shop's confirm is not a fee notice — it is `구입 하시겠습니까?` with
    /// 예/아니오, and it opens on 아니오. This runs the guest's own first pass
    /// early, so that by the time the shipped body executes it is already the
    /// second press and goes straight to the purchase; the popup is never drawn
    /// because both transitions happen inside one key event, before the next
    /// `NpcUI_Render`.
    ///
    /// Running guest code rather than poking `npcUI_bDeal`/`dealSel` by hand is
    /// the point: pass one is where the slot is validated (empty slot, item id
    /// 0x3e), and skipping that could arm `netSocket+0xb4` for a slot that holds
    /// nothing.
    ///
    /// Three things keep this off every other shop. The LR test pins it to the
    /// single `bl` in `NpcUI_merchantKeyPressed` — which also makes recursion
    /// impossible, since `Vm::call` enters with `LR = RETURN_MAGIC`.
    /// `merchantState == 3` is the cash shop specifically, so gold, recipe and
    /// sell keep their confirms. And if pass one declines to arm the deal — an
    /// empty slot, a popup already up — this declines too and the guest simply
    /// handles the press itself, exactly as shipped.
    pub(crate) fn host_cash_skip_confirm(&mut self) -> Result<bool, String> {
        if std::env::var("CS_CASH_CONFIRM").is_ok_and(|v| v != "0") {
            return Ok(false);
        }
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        let site = match self.symbol("NpcUI_merchantKeyPressed") {
            Some(a) => a.wrapping_add(MERCHANT_SELECT_RET),
            None => return Ok(false),
        };
        if lr != site {
            return Ok(false);
        }
        let (Some(state_p), Some(deal_p), Some(sock_p), Some(func)) = (
            self.symbol("merchantState"),
            self.symbol("npcUI_bDeal"),
            self.symbol("netSocket"),
            self.symbol("NpcUI_merchantSelectKeyPressed"),
        ) else {
            return Ok(false);
        };
        // Cash shop, first press, and no purchase already in flight — the guest
        // sets both socket bytes on the way to the charge and only clears them
        // when it resolves.
        if self.mem.r8(state_p) != 3
            || self.mem.r8(deal_p) != 0
            || self.mem.r8(sock_p.wrapping_add(0x17)) != 0
            || self.mem.r8(sock_p.wrapping_add(0x18)) != 0
        {
            return Ok(false);
        }
        self.call(func, &[])?;
        Ok(false)
    }

    /// Append the three 비약 to the cash shop's list.
    ///
    /// The guest's own parse runs first — `Vm::call` on the very function this
    /// is hooked at — and the rows are written behind the twelve it produced.
    /// That is the whole trick: everything the shop draws (name, description,
    /// price, quantity, grade colour) comes out of `_merchant`, having come out
    /// of `NPC/MERCHANT_2`, so an entry that was never in the file is
    /// indistinguishable from one that was. Only the icon comes from elsewhere,
    /// and it is chosen by item id alone, which is why all three reuse 0x23
    /// (고대비약) and are told apart by a sub-type the file never uses.
    ///
    /// Re-running it on every open rather than injecting once is deliberate:
    /// the array is shared with the town shops, which overwrite it wholesale.
    ///
    /// The terminator the guest wrote across `_merchant + 2 + count ..+0x66`
    /// is left alone past our rows, so the id array still ends in -1 where the
    /// shop expects it.
    pub(crate) fn host_cash_potion_list(&mut self) -> Result<bool, String> {
        // The other job this entry carries. `NpcUI_setMerchant` can hold
        // one patch kind, and 「레시피2」 needs the same address for a
        // different argument — the same arrangement `Patch::InCodeString`
        // already has.
        // Row 4 is ours and nothing below can want it.
        if self.recipe2_armed && self.cpu.reg_get(Mode::User, 0) == RECIPE2_ROW {
            return self.host_recipe2_load();
        }
        if !crate::potions::enabled() {
            return Ok(false);
        }
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        // The parse we re-issue below comes back through here.
        if lr == crate::RETURN_MAGIC {
            return Ok(false);
        }
        // `NPC/MERCHANT_2` and state 3: the cash shop, and not the town shops
        // that share this function.
        if self.cpu.reg_get(Mode::User, 0) != 2 {
            return Ok(false);
        }
        let (Some(func), Some(merchant), Some(state_p), Some(bag_max)) = (
            self.symbol("NpcUI_setMerchant"),
            self.symbol("_merchant"),
            self.symbol("merchantState"),
            self.symbol("NPCUI_BAG_MAX"),
        ) else {
            return Ok(false);
        };
        if self.mem.r8(state_p) != MERCH_STATE_CASH {
            return Ok(false);
        }

        self.call(func, &[2])?;

        // Take the shipped rows we do not sell out first, so the count the
        // append works from is already the real one.
        let dropped = if cash_bagslot() { 0 } else { self.merchant_drop(merchant)? };

        // 작명의 비약 is shelved only when there is a pet to rename, so the row
        // set is decided here, once, and the count below follows from it.
        //
        // Short-circuited on the knob so that `CS_POTION_RENAME=0` runs *no*
        // guest code here: `has_pet` calls `MP_getHero`, and a handful of guest
        // instructions per shop opening is enough to break a bit-exact A/B.
        let has_pet = crate::potions::rename_enabled() && self.has_pet()?;
        let rows: Vec<_> = crate::potions::shelf_for(has_pet).collect();

        let shipped = u32::from(self.mem.r16(merchant + MERCH_COUNT));
        let total = shipped + rows.len() as u32;
        // A file that failed to load leaves 0, and there is nothing to append
        // to; more than one page would need the paging UI checked on a device.
        // Either way the shipped shop is left exactly as the guest built it.
        if shipped == 0 || total > MERCH_PAGE {
            log::warn!("potions: shop has {shipped} rows, page holds {MERCH_PAGE}; not injecting");
            self.cpu.reg_set(Mode::User, reg::PC, lr);
            return Ok(true);
        }
        for (k, p) in rows.iter().enumerate() {
            let i = shipped + k as u32;
            self.mem.w8(merchant + MERCH_ID + i, p.id);
            self.mem.w8(merchant + MERCH_SUB + i, p.sub);
            self.mem.w32(merchant + MERCH_PRICE + i * 4, p.price as u32);
            // 1, not a bundle: the field feeds `dealCnt` and the quantity the
            // charge carries, and neither means anything for an item that is
            // never put in a bag. Two weeks is two purchases.
            self.mem.w8(merchant + MERCH_QTY + i, 1);
            self.mem.w8(merchant + MERCH_GRADE + i, p.grade);
            let name = merchant + MERCH_NAME + i * MERCH_NAME_STRIDE;
            let desc = merchant + MERCH_DESC + i * MERCH_DESC_STRIDE;
            self.write_shop_text(name, p.name, MERCH_NAME_STRIDE);
            self.write_shop_text(desc, p.desc, MERCH_DESC_STRIDE);
        }
        self.mem.w16(merchant + MERCH_COUNT, total as u16);
        // The guest writes `id[count] = 0xff` after its own parse; ours has to
        // move with the count, or a removed row leaves the old terminator
        // sitting one slot too far out.
        if (total as i32) < MERCH_SLOTS {
            self.mem.w8(merchant + MERCH_ID + total, MERCH_END);
        }
        self.mem.w8(bag_max, total.div_ceil(MERCH_PAGE) as u8);
        if dropped != 0 {
            log::info!("cash shop: {dropped} shipped row(s) taken off the shelf");
        }

        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Remove every [`MERCH_DROP`] row from the parsed shop, closing the gap.
    ///
    /// `_merchant` is **seven parallel arrays of a hundred**, each with its own
    /// stride, packed end to end with no slack — `0x1260 + 100*0xff = 0x75fc`
    /// is exactly where the next one starts. So a row cannot be blanked in
    /// place: the draw loop walks indices, and a hole would be an empty cell
    /// that still takes a slot and still answers a tap. Every array has to be
    /// pulled down over the gap, which is the work CLAUDE.md pointed at when it
    /// chose to fix 오토루팅's duplicate a different way.
    ///
    /// Returns how many rows went.
    fn merchant_drop(&mut self, merchant: u32) -> Result<u32, String> {
        let count = i32::from(self.mem.r16(merchant + MERCH_COUNT));
        if !(1..=MERCH_SLOTS).contains(&count) {
            return Ok(0);
        }
        let mut keep = 0i32;
        for i in 0..count {
            let id = self.mem.r8(merchant + MERCH_ID + i as u32);
            let sub = self.mem.r8(merchant + MERCH_SUB + i as u32);
            if MERCH_DROP.contains(&(id, sub)) {
                continue;
            }
            if keep != i {
                self.merchant_move(merchant, i as u32, keep as u32);
            }
            keep += 1;
        }
        let gone = (count - keep) as u32;
        if gone != 0 {
            self.mem.w16(merchant + MERCH_COUNT, keep as u16);
        }
        Ok(gone)
    }

    /// Copy shop row `from` over row `to`, in every one of the seven arrays.
    fn merchant_move(&mut self, merchant: u32, from: u32, to: u32) {
        for (base, stride) in [
            (MERCH_ID, 1),
            (MERCH_SUB, 1),
            (MERCH_QTY, 1),
            (MERCH_GRADE, 1),
            (MERCH_PRICE, 4),
            (MERCH_NAME, MERCH_NAME_STRIDE),
            (MERCH_DESC, MERCH_DESC_STRIDE),
        ] {
            let (src, dst) = (merchant + base + from * stride, merchant + base + to * stride);
            let bytes: Vec<u8> = (0..stride).map(|k| self.mem.r8(src + k)).collect();
            self.mem.write_bytes(dst, &bytes);
        }
    }

    /// Clear a `_merchant` string slot and write EUC-KR bytes into it with a
    /// NUL.
    ///
    /// The guest clears only the first 0x28 bytes of a description slot whose
    /// stride is 0xff and then `memcpy`s the file's length over it, so the
    /// shipped 66-byte descriptions work only because `.bss` starts zeroed and
    /// nothing longer ever occupied the slot. Clearing the whole stride and
    /// terminating explicitly means this cannot inherit that.
    fn write_shop_text(&mut self, at: u32, text: &[u8], stride: u32) {
        if text.len() as u32 >= stride {
            return;
        }
        if let Some(slot) = self.mem.slice_mut(at, stride) {
            slot.fill(0);
            slot[..text.len()].copy_from_slice(text);
        }
    }

    /// Write the NZCV an ARM `cmp a, b` would leave, and touch nothing else.
    ///
    /// Two of the 「레시피2」 hooks stand on a `cmp` whose only job is to feed a
    /// predicated instruction a few words later, so the honest substitution is
    /// the comparison itself against a different constant — not an arithmetic
    /// trick on the operand that happens to move the boundary. The mode bits
    /// are read back and left alone, so `RegFile::set` does not re-bank.
    fn set_cmp_flags(&mut self, a: u32, b: u32) {
        let (res, borrow) = a.overflowing_sub(b);
        let n = res >> 31;
        let z = u32::from(res == 0);
        // ARM's carry out of a subtract is "no borrow".
        let c = u32::from(!borrow);
        let v = (((a ^ b) & (a ^ res)) >> 31) & 1;
        let keep = self.cpu.reg_get(Mode::User, reg::CPSR) & !(0xf << 28);
        let flags = (n << 31) | (z << 30) | (c << 29) | (v << 28);
        self.cpu.reg_set(Mode::User, reg::CPSR, keep | flags);
    }

    /// Step over the instruction this hook stands on.
    fn step_over(&mut self) {
        let pc = self.cpu.reg_get(Mode::User, reg::PC);
        self.cpu.reg_set(Mode::User, reg::PC, pc.wrapping_add(4));
    }

    /// Draw one more category row.
    ///
    /// The hooked `mov ip, #0x2e8` is the id of the *last* label the loop
    /// draws; the guest's own `add ip, ip, #1` two instructions later turns it
    /// into the exclusive bound. Answering [`RECIPE2_LABEL_ID`] therefore adds
    /// exactly one iteration, and everything that iteration does — the rounded
    /// box, the text, the 0x1a step down the screen, the cached pointer the
    /// tail re-reads to highlight the selection — is the guest's own loop body
    /// running once more.
    pub(crate) fn host_recipe2_rows(&mut self) -> Result<bool, String> {
        self.cpu.reg_set(Mode::User, 12, RECIPE2_LABEL_ID);
        self.step_over();
        Ok(true)
    }

    /// Wrap the cursor up onto the new last row.
    ///
    /// `movne r2, #3`. The NE is the `tst r2, #0x80` two instructions back —
    /// "the decrement went negative" — so the condition is read from Z rather
    /// than guessed, and the guest's own `strbne` right after does the store.
    pub(crate) fn host_recipe2_wrap_up(&mut self) -> Result<bool, String> {
        const Z: u32 = 1 << 30;
        if self.cpu.reg_get(Mode::User, reg::CPSR) & Z == 0 {
            self.cpu.reg_set(Mode::User, 2, RECIPE2_LAST_ROW);
        }
        self.step_over();
        Ok(true)
    }

    /// Wrap the cursor down past the new last row.
    ///
    /// `cmp r1, #0x3000000`, where `r1` is the incremented cursor shifted up by
    /// 24 so the following `movgt` sees a signed byte. Same shift on our side.
    pub(crate) fn host_recipe2_wrap_down(&mut self) -> Result<bool, String> {
        let cursor = self.cpu.reg_get(Mode::User, 1);
        self.set_cmp_flags(cursor, RECIPE2_LAST_ROW << 24);
        self.step_over();
        Ok(true)
    }

    /// Hit-test one more row.
    ///
    /// `cmp sb, #4` is the touch loop's bound, and the loop's own
    /// `add sl, sl, #0x1a` walks the same 0x1a pitch the draw loop uses — so
    /// the fifth box is tested exactly where the fifth box was drawn.
    pub(crate) fn host_recipe2_hit_rows(&mut self) -> Result<bool, String> {
        let row = self.cpu.reg_get(Mode::User, 9);
        self.set_cmp_flags(row, RECIPE2_ROWS);
        self.step_over();
        Ok(true)
    }

    /// Enter the new row as the recipe shop.
    ///
    /// `merchantState = cursor + 1`, and the key handler dispatches that byte
    /// through a five-entry jump table (`+0x60`, `cmp r3,#4 / addls pc,...`).
    /// A sixth state would fall off the end into "do nothing", so the fifth row
    /// borrows state 2 — the recipe shop — and every arm downstream is the
    /// shipped one. The list it finds there is ours, put in place by
    /// `host_recipe2_load` from the `bl NpcUI_setMerchant` four words on.
    ///
    /// Every other row declines and the guest does its own store.
    pub(crate) fn host_recipe2_state(&mut self) -> Result<bool, String> {
        if self.cpu.reg_get(Mode::User, 2) & 0xff != u32::from(RECIPE2_STATE_RAW) {
            return Ok(false);
        }
        let at = self.cpu.reg_get(Mode::User, 3);
        self.mem.w8(at, RECIPE2_STATE);
        self.step_over();
        Ok(true)
    }

    /// Build the fifth row's shop list in `_merchant`.
    ///
    /// Reached from the `NpcUI_setMerchant` entry hook with `r0 == 4`, which
    /// the shipped function would turn into `NPC/MERCHANT_4` — a file that does
    /// not exist, so the shipped path is a load failure and nothing is lost by
    /// taking it over completely.
    ///
    /// What it writes is exactly what that function's parse loop writes for a
    /// file: the seven parallel arrays, the count, the 0xff run that terminates
    /// the id array (the guest writes it whenever the count is under 100), and
    /// `NPCUI_BAG_MAX`. Nothing else in the shipped body matters here — the rest
    /// of it is `UM_LoadRes`/`UM_Free` around the buffer we do not need.
    pub(crate) fn host_recipe2_load(&mut self) -> Result<bool, String> {
        let (Some(merchant), Some(bag_max)) =
            (self.symbol("_merchant"), self.symbol("NPCUI_BAG_MAX"))
        else {
            return Ok(false);
        };
        // Taken out and put back so the rows can be read while `self.mem` is
        // written. Moving a `Vec` twice is cheaper than cloning the strings.
        let recipes = std::mem::take(&mut self.recipe2);
        let n = recipes.rows().len() as u32;
        if n == 0 || n > MERCH_PAGE {
            self.recipe2 = recipes;
            return Ok(false);
        }
        let price = crate::patch::recipe2_price();
        for (i, row) in recipes.rows().iter().enumerate() {
            let i = i as u32;
            self.mem.w8(merchant + MERCH_ID + i, crate::recipe2::RECIPE_KIND);
            self.mem.w8(merchant + MERCH_SUB + i, row.sub);
            self.mem.w32(merchant + MERCH_PRICE + i * 4, price as u32);
            self.mem.w8(merchant + MERCH_QTY + i, RECIPE2_QTY);
            self.mem.w8(merchant + MERCH_GRADE + i, RECIPE2_GRADE);
            let name = merchant + MERCH_NAME + i * MERCH_NAME_STRIDE;
            let desc = merchant + MERCH_DESC + i * MERCH_DESC_STRIDE;
            self.write_shop_text(name, &row.name, MERCH_NAME_STRIDE);
            self.write_shop_text(desc, &row.desc, MERCH_DESC_STRIDE);
        }
        // `NpcUI_setMerchant+0x248`: 0xff from the last row to the end of the
        // id array. The grid stops at the first one of these.
        for k in n..MERCH_SLOTS as u32 {
            self.mem.w8(merchant + MERCH_ID + k, 0xff);
        }
        self.mem.w16(merchant + MERCH_COUNT, n as u16);
        self.mem.w8(bag_max, n.div_ceil(MERCH_PAGE) as u8);
        self.recipe2 = recipes;

        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Pick which shipped pet AI runs this tick.
    ///
    /// The hooked `ldrb r3, [r5, #0x656]` is the byte `Pet_Process`'s jump
    /// table dispatches on. For the shipped four this declines and the guest
    /// reads its own byte; for 스마트 it answers one of those four instead, so
    /// the table, the arm it lands in and everything the arm does are the
    /// guest's.
    ///
    /// `r5` is the pet (`MP_getHero(CUR_HERO + 1)`, set in the prologue) and
    /// `r6` is its index shifted up by 24, which is how the treasure arm four
    /// instructions later passes it to `HERO_Check_Treasure`.
    pub(crate) fn host_pet_smart_pick(&mut self) -> Result<bool, String> {
        let pet = self.cpu.reg_get(Mode::User, 5);
        if self.mem.r8(pet.wrapping_add(PET_COND)) != PET_SMART {
            if self.pet_dbg(pet, 0) {
                log::info!(
                    "pet guest: cond {} state {} sub {} target {}",
                    self.mem.r8(pet.wrapping_add(PET_COND)),
                    self.mem.r8(pet.wrapping_add(PET_STATE)),
                    self.mem.r8(pet.wrapping_add(PET_STATE + 1)),
                    self.mem.r8(pet.wrapping_add(PET_TARGET)) as i8,
                );
            }
            return Ok(false);
        }
        let idx = (self.cpu.reg_get(Mode::User, 6) as i32) >> 24;
        let pick = self.pet_smart_arm(pet, idx)?;
        if self.pet_dbg(pet, 1 + pick) {
            log::info!(
                "pet smart: state {} target {} -> arm {}",
                self.mem.r8(pet.wrapping_add(PET_STATE)),
                self.mem.r8(pet.wrapping_add(PET_TARGET)) as i8,
                pick
            );
        }
        self.pet_smart_last = pick;
        self.cpu.reg_set(Mode::User, 3, u32::from(pick));
        self.step_over();
        Ok(true)
    }

    /// Treasure when there is treasure, combat otherwise.
    ///
    /// `HERO_Check_Treasure` is the shipped search — it walks `_monsterList`
    /// and leaves the nearest candidate's index in `pet[0x288]`, or -1. The
    /// treasure arm runs it again a moment later; it is a search with no side
    /// effect beyond that byte and the autotarget scratch, so asking twice
    /// costs instructions and changes nothing.
    fn pet_smart_arm(&mut self, pet: u32, idx: i32) -> Result<u8, String> {
        let combat = pet_smart_combat();
        // Mid-action: keep whatever arm started the action. See the module
        // comment — answering combat here instead left the pet "attacking" a
        // chest it had just uncovered.
        if self.mem.r8(pet.wrapping_add(PET_STATE)) != 0 {
            return Ok(self.pet_smart_last);
        }
        let (Some(check), Some(is_object), Some(list)) = (
            self.symbol("HERO_Check_Treasure"),
            self.symbol("MONSTER_isObject"),
            self.symbol("_monsterList"),
        ) else {
            return Ok(combat);
        };
        self.call(check, &[idx as u32])?;
        let cand = self.mem.r8(pet.wrapping_add(PET_TARGET)) as i8;
        if cand < 0 || cand as u32 >= PET_LIST_SLOTS {
            return Ok(combat);
        }
        let monster = self.mem.r32(list + cand as u32 * 4);
        if monster == 0 {
            return Ok(combat);
        }
        Ok(match self.call(is_object, &[monster])? {
            0 => combat,
            _ => PET_TREASURE,
        })
    }

    /// Tell `Pet_move_process` the same story.
    ///
    /// Its two reads of the byte are `v > 3` (unsigned — 4 would stop the pet
    /// dead) and `v == 2` (the approach a treasure hunter makes). Both are
    /// answered with the arm this tick actually picked. `r7` is the pet, set
    /// from the function's own `MP_getHero`.
    pub(crate) fn host_pet_smart_move(&mut self) -> Result<bool, String> {
        let pet = self.cpu.reg_get(Mode::User, 7);
        if self.mem.r8(pet.wrapping_add(PET_COND)) != PET_SMART {
            return Ok(false);
        }
        if self.pet_dbg(pet, 100) {
            log::info!(
                "pet move@{:#x}: state {} sub {} target {} -> {}",
                self.cpu.reg_get(Mode::User, reg::PC) & 0xfff,
                self.mem.r8(pet.wrapping_add(PET_STATE)),
                self.mem.r8(pet.wrapping_add(PET_STATE + 1)),
                self.mem.r8(pet.wrapping_add(PET_TARGET)) as i8,
                self.pet_smart_last
            );
        }
        self.cpu.reg_set(Mode::User, 3, u32::from(self.pet_smart_last));
        self.step_over();
        Ok(true)
    }

    /// Let an uncovered chest past the `HP <= 0` refusal — and only it.
    ///
    /// `r5` is the monster, `r3` the HP just loaded. Everything except a
    /// visible type-5 object with no HP is declined, so the guest's own
    /// compare runs and every other candidate is judged exactly as before.
    /// When it is one, the flags are set as though the HP were 1: the `ble`
    /// falls through and the checks after it — dead, visible, object — are
    /// the guest's, unmodified.
    pub(crate) fn host_chest_break(&mut self) -> Result<bool, String> {
        let hp = self.cpu.reg_get(Mode::User, 3) as i32;
        let m = self.cpu.reg_get(Mode::User, 5);
        if hp > 0 || m == 0 {
            return Ok(false);
        }
        let object = self.mem.r8(m.wrapping_add(0xc6)) == 5;
        let uncovered = self.mem.r8(m.wrapping_add(0x20)) != 0;
        if !(object && uncovered) {
            return Ok(false);
        }
        self.set_cmp_flags(1, 0);
        self.step_over();
        Ok(true)
    }

    /// Should a pet line be logged now? On when `CS_PET_DEBUG` is set or the
    /// build baked `CS_BOX_DEBUG`, and then only when something the line would
    /// show has actually changed — the arm is picked every world tick.
    fn pet_dbg(&mut self, pet: u32, tag: u8) -> bool {
        if !(std::env::var_os("CS_PET_DEBUG").is_some() || box_debug()) {
            return false;
        }
        let key = (
            tag,
            self.mem.r8(pet.wrapping_add(PET_COND)),
            self.mem.r8(pet.wrapping_add(PET_STATE)),
            self.mem.r8(pet.wrapping_add(PET_STATE + 1)),
            self.mem.r8(pet.wrapping_add(PET_TARGET)),
        );
        if self.pet_dbg_last == Some(key) {
            return false;
        }
        self.pet_dbg_last = Some(key);
        true
    }

    /// Diagnostic (`CS_BOX_DEBUG`) — say what a treasure object looks like to
    /// the two gates that decide whether the hero may hit it.
    ///
    /// `r0` is the monster both functions are asked about. Objects are type 5
    /// (`MONSTER_getType` = `m[0xc6]`); everything else is passed over in
    /// silence. Declines always, and only reads, so the guest is unchanged.
    pub(crate) fn host_box_debug(&mut self) -> Result<bool, String> {
        let m = self.cpu.reg_get(Mode::User, 0);
        if m != 0 && self.mem.r8(m.wrapping_add(0xc6)) == 5 {
            let hp = self.mem.r32(m.wrapping_add(0x120)) as i32;
            let vis = self.mem.r8(m.wrapping_add(0x20));
            let st = self.mem.r8(m.wrapping_add(0xc0)) as i8;
            let x = self.mem.r32(m.wrapping_add(8)) as i32;
            let y = self.mem.r32(m.wrapping_add(0xc)) as i32;
            let pc = self.cpu.reg_get(Mode::User, reg::PC);
            // Keyed on the state alone, not on which object it belongs to:
            // the sweep asks about every chest several times a second and they
            // are all in the same state, so this is one line until something
            // actually changes — which is the moment worth seeing.
            let key = (0, hp, vis, st, 0);
            let _ = pc;
            if self.box_dbg_last != Some(key) {
                self.box_dbg_last = Some(key);
                log::info!(
                    "box {m:#x} @{pc:#x}: hp {hp} visible {vis} state {st} at ({x},{y})"
                );
            }
        }
        Ok(false)
    }

    /// Diagnostic (`CS_BOX_DEBUG`) — report what the OK press did before the
    /// attack. `r0` is the value just returned: non-zero at the first site
    /// means an event ran, non-zero at the second means something was picked
    /// up, and either way `HERO_Key_Press` is never reached.
    pub(crate) fn host_ok_debug(&mut self) -> Result<bool, String> {
        let r0 = self.cpu.reg_get(Mode::User, 0) as i32;
        let pc = self.cpu.reg_get(Mode::User, reg::PC);
        let which = if pc & 0xfff == 0x4bc { "event" } else { "pickup" };
        log::info!("ok {which}: returned {r0}");
        Ok(false)
    }

    /// Tell `HERO_Check_Autotarget` the same story. `r5` is the character it
    /// was called for; only a pet's byte can hold 스마트.
    pub(crate) fn host_pet_smart_target(&mut self) -> Result<bool, String> {
        let who = self.cpu.reg_get(Mode::User, 5);
        if self.mem.r8(who.wrapping_add(PET_COND)) != PET_SMART {
            return Ok(false);
        }
        self.cpu.reg_set(Mode::User, 3, u32::from(self.pet_smart_last));
        self.step_over();
        Ok(true)
    }

    /// ← past the first 전투타입 wraps onto 스마트 instead of 전투안함.
    ///
    /// `movle r2, #3`, predicated on the `cmp` two instructions back, so the
    /// condition is read from the flags rather than assumed and the guest's
    /// own `strble` right after does the store.
    pub(crate) fn host_pet_smart_wrap_up(&mut self) -> Result<bool, String> {
        const N: u32 = 1 << 31;
        const Z: u32 = 1 << 30;
        let cpsr = self.cpu.reg_get(Mode::User, reg::CPSR);
        // LE — the guest's own condition on this instruction.
        if cpsr & Z != 0 || (cpsr & N != 0) != (cpsr & (1 << 28) != 0) {
            self.cpu.reg_set(Mode::User, 2, u32::from(PET_SMART));
        }
        self.step_over();
        Ok(true)
    }

    /// → past 스마트 wraps to the first 전투타입.
    ///
    /// `cmp r1, #0x2000000`, where `r1` is the incremented value shifted up by
    /// 24 so the following `movgt` sees a signed byte. Same shift here.
    pub(crate) fn host_pet_smart_wrap_down(&mut self) -> Result<bool, String> {
        let value = self.cpu.reg_get(Mode::User, 1);
        self.set_cmp_flags(value, u32::from(PET_SMART - 1) << 24);
        self.step_over();
        Ok(true)
    }

    /// Give 스마트 the first description arm, whose text is then swapped.
    ///
    /// `draw_pet_info` picks the 전투타입 description with a second five-arm
    /// jump table, and its default arm writes no text at all — it hands the
    /// drawing code an untouched stack buffer. Answering 0 runs the arm that
    /// does `MC_knlSprintk(buf, getInCodeString(0x121))`, and the string hook
    /// (gated on that call's return address) puts ours there instead.
    /// The arm this answers is also the one a real 단독 전투 pet gets, so the
    /// string swap downstream cannot tell them apart from the id alone. It is
    /// told here — refreshed on every pass, including the ones that decline.
    pub(crate) fn host_pet_smart_desc(&mut self) -> Result<bool, String> {
        let menu = self.cpu.reg_get(Mode::User, 3);
        self.pet_smart_desc = self.mem.r8(menu.wrapping_add(1)) == PET_SMART;
        if !self.pet_smart_desc {
            return Ok(false);
        }
        // Three sites, two destination registers (`r3` twice, `lr` once):
        // take Rd from the instruction rather than keep a table of it.
        let pc = self.cpu.reg_get(Mode::User, reg::PC);
        let rd = (self.mem.r32(pc) >> 12) & 0xf;
        self.cpu.reg_set(Mode::User, rd as u8, 0);
        self.step_over();
        Ok(true)
    }

    /// Turn a 비약 purchase into a timed buff.
    ///
    /// `NpcUI_setBoughtItemOnCASH` is where every paid cash item is finally
    /// handed over, and its first act — `NpcUI_merchantGetCanPickup`, which is
    /// nothing but `ItemBag_checkAndSetItem` — is the only thing that puts
    /// anything in a bag. Intercepting at the entry therefore needs no item
    /// database entry, no icon and no use handler; the shipped tail is
    /// reproduced instead, byte for byte: `dealCnt = 1`, `errIdx = 6`
    /// (구입하였습니다), the socket's in-flight flag and both deal flags
    /// cleared, then `GameFrame_autoSave`.
    ///
    /// Anything that is not one of the three declines, so the other eleven
    /// cash items are still bought by the guest.
    ///
    /// The bag-full gate in `NpcUI_merchantSelectKeyPressed` runs *before*
    /// this and is left in place: a 비약 cannot be bought with a full bag even
    /// though it never enters one. Lifting it would mean hooking the key
    /// handler as well, for no gain worth that.
    pub(crate) fn host_cash_potion_buy(&mut self) -> Result<bool, String> {
        if !crate::potions::enabled() {
            return Ok(false);
        }
        let (
            Some(merchant),
            Some(sock),
            Some(err_p),
            Some(cnt_p),
            Some(sel_p),
            Some(deal_p),
            Some(autosave),
        ) = (
            self.symbol("_merchant"),
            self.symbol("netSocket"),
            self.symbol("errIdx"),
            self.symbol("dealCnt"),
            self.symbol("dealSel"),
            self.symbol("npcUI_bDeal"),
            self.symbol("GameFrame_autoSave"),
        )
        else {
            return Ok(false);
        };
        let slot = self.mem.r32(sock + SOCK_SLOT) as i32;
        if !(0..MERCH_SLOTS).contains(&slot) {
            return Ok(false);
        }
        let (id, sub) = (
            self.mem.r8(merchant + MERCH_ID + slot as u32),
            self.mem.r8(merchant + MERCH_SUB + slot as u32),
        );
        let Some(shelf) = crate::potions::SHELF.iter().find(|p| p.id == id && p.sub == sub) else {
            return Ok(false);
        };

        // Past this point the row is ours and the purchase is *always* served:
        // declining now would let the guest run its own body and put a real
        // 고대비약 — item id 0x23 — into the bag.
        let err = match shelf.effect {
            crate::potions::Effect::Timed(kind) => {
                let now = self.wall_now_ms();
                self.potions.grant(kind, now);
                let storage = self.cfg.storage.clone();
                self.potions.save(&storage);
                ERR_BOUGHT
            }
            // The keyboard coming up *is* the acknowledgement, so no message:
            // "구입하였습니다" would sit under the overlay, undismissable until
            // the naming is finished. `ERR_NONE` is the guest's own value for
            // "draw nothing".
            crate::potions::Effect::Rename => match self.open_pet_rename(true)? {
                true => ERR_NONE,
                // Unreachable while the row is shelved only with a pet, which is
                // exactly why it is not designed around: acknowledge the
                // purchase and say so in the log rather than invent a UI.
                false => {
                    log::warn!("potions: 작명 bought but the naming screen could not be opened");
                    ERR_BOUGHT
                }
            },
        };

        self.mem.w32(cnt_p, 1);
        self.mem.w8(err_p, err);
        self.mem.w8(sock + SOCK_FLAG, 0);
        self.mem.w8(sel_p, 0);
        self.mem.w8(deal_p, 0);

        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.call(autosave, &[])?;
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Does the player have a pet? `hero[0x62c]` on hero 0.
    ///
    /// `MP_getHero` is `base + 0xb90 * i` and touches nothing, so calling it is
    /// free of side effects — the same reason `host_cast_cancel` calls it.
    fn has_pet(&mut self) -> Result<bool, String> {
        let Some(get_hero) = self.symbol("MP_getHero") else {
            return Ok(false);
        };
        let hero = self.call(get_hero, &[0])?;
        Ok(hero != 0 && self.mem.r8(hero + HERO_HAS_PET) != 0)
    }

    /// Reopen the pet-naming screen — the whole of 작명의 비약.
    ///
    /// Two guest calls and a flag. `InputInit(1)` is the same call the naming
    /// opcode makes (`EVT_OnFunction+0x1068`, event opcode 0x15), and everything
    /// after it is the guest's: the keyboard, the Hangul composition, the
    /// five-character and no-space checks in `onCloseIME_MainFrame`, the
    /// `이 이름으로 결정 하시겠습니까?` popup, and the write into the pet. Since
    /// only the OK key (-5) releases the overlay and an empty or over-long name
    /// simply reopens it, there is no way to leave this half finished.
    ///
    /// **`_event[0xcfa]` is deliberately not set.** The opcode sets it, but only
    /// two places read it — `Event_isEndInputPetName`, which holds a *cutscene*
    /// still, and `Event_Draw_Event+0x7c`, which draws the prompt box and is
    /// unreachable outside one. Setting it here would buy nothing and would stall
    /// `EVENT_TickMovie` if an event happened to be running.
    ///
    /// The one deviation from the opcode: the initial string is the pet's current
    /// name rather than the default 가름. `InputInit` has already set the default
    /// through the same property, so this is the same call with a different
    /// pointer, and it is what makes the screen self-explanatory without the
    /// caption the cutscene draws.
    ///
    /// `require_pet` is false only for the probe's `--pet-rename`. A pet arrives
    /// from an event-script opcode partway through the story (`Event_getPet`,
    /// which both of its `_hardMode` branches end by setting `hero[0x62c]`), well
    /// past anything a scripted tap sequence reaches, so the desktop test opens
    /// the screen without one: the keyboard, the commit and the save need no live
    /// pet object, and faking the flag instead crashes `Pet_Process` on the next
    /// frame (measured — a null sprite in `SPR_getDirCnt`).
    pub fn open_pet_rename(&mut self, require_pet: bool) -> Result<bool, String> {
        let (Some(input_init), Some(set_prop), Some(get_hero), Some(letter)) = (
            self.symbol("InputInit"),
            self.symbol("MX_imeSetProperty"),
            self.symbol("MP_getHero"),
            self.symbol("_mLetterState"),
        ) else {
            return Ok(false);
        };
        let state = self.mem.r8(letter) as i8;
        if state == LETTER_STATE_RECIPIENT || state == LETTER_STATE_BODY {
            return Ok(false);
        }
        if require_pet && !self.has_pet()? {
            return Ok(false);
        }
        let pet = self.call(get_hero, &[PET_HERO])?;
        if pet == 0 {
            return Ok(false);
        }

        self.call(input_init, &[IME_MODE_PET])?;
        // Only when there is something there: property 2 on an empty string
        // would leave a zero-length field, and the guest would answer it with
        // 펫이름을 입력해 주세요 and reopen.
        let name = pet + PET_NAME;
        if self.mem.r8(name) != 0 {
            self.call(set_prop, &[IME_PROP_TEXT, name])?;
        }
        self.pet_rename_pending = true;
        log::info!("potions: 작명 — pet naming reopened");
        Ok(true)
    }

    /// Set the whole-screen zoom percent, so a replay can reach
    /// `HERO_Draw_Resize_Background`.
    ///
    /// The probe cannot otherwise: the one call site is gated on
    /// `(s16)hero[0x282] != 100`, and nothing a scripted tap sequence reaches
    /// moves that field. Faking it is safe in the same way `--boss-zoom` is —
    /// the field is read, never dereferenced, and its only reader is the gate
    /// and the argument it builds. `--resize` is how the numbers in the
    /// performance section were measured.
    pub fn arm_resize_background(&mut self, percent: i32) -> Result<bool, String> {
        let Some(get_hero) = self.symbol("MP_getHero") else { return Ok(false) };
        let hero = self.call(get_hero, &[0])?;
        if hero == 0 {
            return Ok(false);
        }
        self.mem.w16(hero.wrapping_add(HERO_RESIZE_SCALE), percent as u16);
        Ok(true)
    }

    /// Arm the boss-kill zoom, the way `HERO_Attack_Monster+0x10d8` does.
    ///
    /// One byte: `Map_drawMiddleObject+0x174` reads `hero[0x899]`, and finding
    /// it set is the whole of starting the effect — it arms the counter at
    /// `+0x210`, blurs, steps, and clears the byte itself when the counter
    /// passes the end. The guest's own two writers are a boss dying with
    /// `monster[0x1f5]` set, which is well past anything a scripted tap
    /// sequence reaches, so `--boss-zoom` sets the byte instead.
    ///
    /// Unlike the pet flag, this one is safe to fake: nothing dereferences it,
    /// and the fields the effect's first frame clears (`hero[0x290..0x29b]`,
    /// the cast and chain slots) are the same ones `HERO_Key_DirectKeyPress`
    /// clears on any interrupted cast.
    pub fn arm_boss_zoom(&mut self) -> Result<bool, String> {
        let Some(get_hero) = self.symbol("MP_getHero") else { return Ok(false) };
        let hero = self.call(get_hero, &[0])?;
        if hero == 0 {
            return Ok(false);
        }
        self.mem.w8(hero.wrapping_add(HERO_BOSS_ZOOM), 1);
        // Stand in for the boss the kill sites would have recorded. The hero's
        // own position is where a boss it just killed is standing, near enough,
        // and it means the probe exercises the real map-space-to-screen path
        // rather than falling back to the middle of the screen.
        self.zoom_boss = Some((
            self.mem.r32(hero.wrapping_add(CHAR_X)) as i32,
            self.mem.r32(hero.wrapping_add(CHAR_Y)) as i32,
        ));
        Ok(true)
    }

    /// Open the blacksmith the way event opcode 0x45 does (`EVT_OnFunction`):
    /// `_event[0xcfb] = 1` says an event opened an NPC screen, and
    /// `NpcUI_setNpc(1)` sets it up. No scripted tap sequence reaches a
    /// blacksmith; this is how the probe (`--smith <frame>`) looks at the
    /// cave's 외형변경 screens.
    pub fn open_blacksmith(&mut self) -> Result<bool, String> {
        let (Some(set_npc), Some(event)) = (self.symbol("NpcUI_setNpc"), self.symbol("_event"))
        else {
            return Ok(false);
        };
        self.mem.w8(event.wrapping_add(EVENT_NPC_UI_OPEN), 1);
        self.call(set_npc, &[NPC_BLACKSMITH])?;
        Ok(true)
    }

    /// Give the hero a pet the way the story does (`--pet <frame>`).
    ///
    /// `Event_getPet` is the whole of it and takes no arguments: `Pet_createPet`,
    /// the pet placed on the hero, `hero[0x62c] = 1`, the default name 가름,
    /// the object list entry and the first motion. Hard mode takes a longer arm
    /// of the same function (a `Pet_Release` first, plus `GameFrame_loadPetData`),
    /// and both end the same way.
    ///
    /// **Faking `hero[0x62c]` instead kills the next frame** — `Pet_Process`
    /// dereferences a sprite `Pet_createPet` never made (`SPR_getDirCnt+0x4`,
    /// measured). Calling the real thing is the only way to get a pet the
    /// probe can watch, which is what every 스마트모드 test needs.
    pub fn give_pet(&mut self, mode: Option<u32>) -> Result<bool, String> {
        let Some(f) = self.symbol("Event_getPet") else { return Ok(false) };
        self.call(f, &[])?;
        // The story's opcode also unlocks the in-game menu's pet cell
        // (`EVT_OnFunction+0x10cc`, `mainFrame[0x157]`); without it the cell
        // stays 잠김 and the command menu cannot be reached.
        if let Some(mf) = self.symbol("mainFrame") {
            self.mem.w8(mf.wrapping_add(0x157), 1);
        }
        // `--pet <frame>:<n>` also sets the 전투타입, which the menu is the
        // only other way to reach. `Pet_set_condition` is the menu's own
        // setter, so 4 lands in `pet[0x656]` exactly as a player's → would.
        if let Some(mode) = mode {
            let Some(set) = self.symbol("Pet_set_condition") else { return Ok(true) };
            self.call(set, &[mode])?;
        }
        Ok(true)
    }

    /// Probe diagnostics: the pet's `[0xc0..0xc4]` state bytes, its target and
    /// its 전투타입, or `None` without a pet. `MP_getHero` is arithmetic only.
    pub fn pet_state(&mut self) -> Result<Option<[i32; 6]>, String> {
        let (Some(get_hero), Some(cur)) = (self.symbol("MP_getHero"), self.symbol("CUR_HERO"))
        else {
            return Ok(None);
        };
        let cur_idx = u32::from(self.mem.r8(cur));
        let hero = self.call(get_hero, &[cur_idx])?;
        if hero == 0 || self.mem.r8(hero.wrapping_add(HERO_HAS_PET)) == 0 {
            return Ok(None);
        }
        let idx = u32::from(self.mem.r8(cur)).wrapping_add(1);
        let pet = self.call(get_hero, &[idx])?;
        let b = |m: &mut GuestMem, off: u32| m.r8(pet.wrapping_add(off));
        Ok(Some([
            b(&mut self.mem, PET_STATE) as i32,
            b(&mut self.mem, PET_STATE + 1) as i32,
            b(&mut self.mem, PET_STATE + 2) as i32,
            b(&mut self.mem, PET_STATE + 3) as i32,
            b(&mut self.mem, PET_TARGET) as i8 as i32,
            b(&mut self.mem, PET_COND) as i32,
        ]))
    }

    /// The same thing for the town merchant (`--merchant <frame>`), whose
    /// category menu is where 「레시피2」 adds its row. No scripted tap
    /// sequence reaches a town NPC either.
    pub fn open_merchant(&mut self) -> Result<bool, String> {
        let (Some(set_npc), Some(event)) = (self.symbol("NpcUI_setNpc"), self.symbol("_event"))
        else {
            return Ok(false);
        };
        self.mem.w8(event.wrapping_add(EVENT_NPC_UI_OPEN), 1);
        self.call(set_npc, &[NPC_MERCHANT])?;
        Ok(true)
    }

    /// Hand one input straight to the NPC screen: `NpcUI_KeyPressed(type, a,
    /// b, 0)` with a key press (`2`, the key in `a`) or a tap (`0x13`, WIPI
    /// coordinates). An NPC screen a script opened only receives input through
    /// a running event (`GameFrame_gamekey` → `Event_Key`), which
    /// `open_blacksmith` does not start; the probe delivers its `--keys` and
    /// `--taps` this way instead once `--smith` has fired.
    pub fn npc_ui_input(&mut self, ty: u32, a: i32, b: i32) -> Result<bool, String> {
        let Some(f) = self.symbol("NpcUI_KeyPressed") else { return Ok(false) };
        self.call(f, &[ty, a as u32, b as u32, 0])?;
        Ok(true)
    }

    /// Put a fresh item of `(kind, idx)` in the bag, the way a shop purchase
    /// does (`ItemBag_checkAndSetItem` on the storage the guest is using —
    /// the cave's ten-bag copy when that feature is in). `kind` may be given
    /// as `worn:<slot>` to mean "the kind of what the hero wears in that slot",
    /// which is how the probe makes a transmog donor without knowing the
    /// class's weapon kind. Returns whether it fit.
    pub fn give_item(&mut self, kind: &str, idx: u32, count: u32) -> Result<bool, String> {
        let kind = match kind.strip_prefix("worn:") {
            Some(slot) => {
                let slot: u32 = slot.parse().map_err(|_| "worn:<slot>".to_string())?;
                let Some(get_equip) = self.symbol("MP_getEquip") else { return Ok(false) };
                let item = self.call(get_equip, &[0, slot])?;
                if item == 0 {
                    return Ok(false);
                }
                self.mem.r8(item.wrapping_add(0x96)) as u32
            }
            None => kind.parse().map_err(|_| "kind".to_string())?,
        };
        let storage = if self.cave_installed.contains(&"bag10") {
            self.symbol("storage10")
        } else {
            self.symbol("_storage")
        };
        let (Some(storage), Some(set)) = (storage, self.symbol("ItemBag_checkAndSetItem")) else {
            return Ok(false);
        };
        Ok(self.call(set, &[storage, kind, idx, count, 1])? != 0)
    }

    // ---- 공용창고 -------------------------------------------------------
    //
    // The chest itself is `vault.rs`, which knows nothing about the guest.
    // These are the other half: reading a live bag, serialising one entry into
    // the game's own record, and putting a record back. Everything that
    // changes a bag here goes through a shipped function — `Item_saveStorage`
    // and `Item_loadStorage` are the specification and the oracle.

    /// The storage struct the guest is actually using: the cave's ten-bag copy
    /// when `bag10` is installed, the shipped three-tab one otherwise. Same
    /// choice `give_item` makes, and the same reason — the GOT slot the guest
    /// reads is pointed at whichever of the two is live.
    pub(crate) fn bag_storage(&mut self) -> Option<u32> {
        if self.cave_installed.contains(&"bag10") {
            self.symbol("storage10")
        } else {
            self.symbol("_storage")
        }
    }

    /// How many tabs this bag has — `_storage[2]`, which is what
    /// `Item_saveStorage` and `ItemBag_checkAndSetItem` both loop over, so it
    /// is right for three tabs and for ten.
    pub(crate) fn bag_tabs(&mut self) -> i32 {
        match self.bag_storage() {
            Some(s) => self.mem.r8(s.wrapping_add(2)) as i8 as i32,
            None => 0,
        }
    }

    /// The `LinkedList` for one tab: `storage + 4 + 12*tab`, the arithmetic
    /// `ITEMSTORAGE_getList` does.
    fn bag_list(&mut self, tab: i32) -> Option<u32> {
        let s = self.bag_storage()?;
        (tab >= 0 && tab < self.bag_tabs()).then(|| s.wrapping_add(4 + 12 * tab as u32))
    }

    /// How many entries are in a tab — the list's own count byte at +8.
    pub(crate) fn bag_count(&mut self, tab: i32) -> i32 {
        match self.bag_list(tab) {
            Some(l) => self.mem.r8(l.wrapping_add(8)) as i8 as i32,
            None => 0,
        }
    }

    /// The item at `(tab, index)`, through the guest's own walk.
    pub(crate) fn bag_item(&mut self, tab: i32, index: i32) -> Result<u32, String> {
        let (Some(list), Some(get)) = (self.bag_list(tab), self.symbol("ITEM_getItemFrom")) else {
            return Ok(0);
        };
        self.call(get, &[list, index as u32])
    }

    /// `(tab, index, item)` for everything in the bag.
    pub(crate) fn bag_walk(&mut self) -> Result<Vec<(i32, i32, u32)>, String> {
        let mut out = Vec::new();
        for tab in 0..self.bag_tabs() {
            for i in 0..self.bag_count(tab) {
                let item = self.bag_item(tab, i)?;
                if item != 0 {
                    out.push((tab, i, item));
                }
            }
        }
        Ok(out)
    }

    /// Find the bag entry carrying this stamp.
    ///
    /// **The one question a resume asks.** The stamp is `item + 0xd0`, the
    /// same 64-bit value `Item_loadStorage` matches equipment against, so
    /// "the bag has it" means exactly what the save means by it.
    pub(crate) fn bag_find(&mut self, stamp: i64) -> Result<Option<(i32, i32, u32)>, String> {
        for (tab, i, item) in self.bag_walk()? {
            if self.item_stamp(item) == stamp {
                return Ok(Some((tab, i, item)));
            }
        }
        Ok(None)
    }

    /// How many bag entries carry this stamp. More than one means the number
    /// does not identify an item, which is the one thing the transfer protocol
    /// needs it to do; see [`Vm::vault_deposit`].
    pub(crate) fn bag_stamp_twins(&mut self, stamp: i64) -> Result<usize, String> {
        let mut n = 0;
        for (_, _, item) in self.bag_walk()? {
            if self.item_stamp(item) == stamp {
                n += 1;
            }
        }
        Ok(n)
    }

    pub(crate) fn item_stamp(&mut self, item: u32) -> i64 {
        let lo = u64::from(self.mem.r32(item.wrapping_add(ITEM_STAMP)));
        let hi = u64::from(self.mem.r32(item.wrapping_add(ITEM_STAMP + 4)));
        ((hi << 32) | lo) as i64
    }

    /// Serialise a live item into the game's own 36-byte save record.
    ///
    /// Field for field what `Item_saveStorage` writes ([`ITEM_FIELDS`]), so a
    /// chest record *is* a save record and
    /// `the_chest_serialises_an_item_the_way_the_save_does` can compare the
    /// two byte for byte.
    pub(crate) fn item_record(&mut self, item: u32, tab: i32) -> crate::vault::Record {
        let mut rec = [0u8; crate::vault::REC];
        for &(at, off, width) in ITEM_FIELDS {
            let v: u64 = match width {
                1 => u64::from(self.mem.r8(item.wrapping_add(off))),
                2 => u64::from(self.mem.r16(item.wrapping_add(off))),
                _ => self.item_stamp(item) as u64,
            };
            rec[at..at + width].copy_from_slice(&v.to_le_bytes()[..width]);
        }
        rec[crate::vault::rec::TAB] = tab as u8;
        rec
    }

    /// Build a live item from a record and link it into the bag, the way
    /// `Item_loadStorage` does: `ITEM_CreateItem` from the first three bytes,
    /// `ITEM_SetItemName`, then every per-instance field written over the
    /// template's — **including the stamp**, which is how the withdrawn item
    /// comes out carrying the identity [`crate::vault::Vault::arm_withdraw`]
    /// promised it would.
    ///
    /// Returns the tab it landed in. The tab is chosen the way
    /// `ItemBag_checkAndSetItem` chooses one — ask each tab in turn — rather
    /// than trusting the record's, because the record may have been written by
    /// a ten-bag build and read by a three-tab one.
    pub(crate) fn bag_put(&mut self, rec: &crate::vault::Record) -> Result<i32, String> {
        use crate::vault::rec as f;
        let kind = rec[f::KIND] as u32;
        let idx = rec[f::IDX] as i8 as i32;
        let count = rec[f::COUNT] as i8 as i32;
        let Some(tab) = self.bag_room_for(kind, idx, count)? else {
            return Err("no bag tab will take it".into());
        };
        self.bag_put_in(rec, tab)
    }

    /// The same, into a tab already chosen.
    ///
    /// The chest view ([`Vm::vault_build_view`]) uses this: it is one tab by
    /// construction and its entries must stay one row each, which is what
    /// `ITEMSTORAGE_addItem` does — it links a new entry rather than merging
    /// into a stack, and [`Vm::bag_put`] relies on the same thing.
    pub(crate) fn bag_put_in(
        &mut self,
        rec: &crate::vault::Record,
        tab: i32,
    ) -> Result<i32, String> {
        use crate::vault::rec as f;
        let kind = rec[f::KIND] as u32;
        let idx = rec[f::IDX] as i8 as i32;
        let count = rec[f::COUNT] as i8 as i32;
        let (Some(create), Some(name), Some(add)) = (
            self.symbol("ITEM_CreateItem"),
            self.symbol("ITEM_SetItemName"),
            self.symbol("ITEMSTORAGE_addItem"),
        ) else {
            return Err("the item constructors are missing".into());
        };
        let item = self.call(create, &[kind, idx as u32, count as u32])?;
        if item == 0 {
            return Err("the item pool is empty".into());
        }
        self.call(name, &[item])?;
        for &(at, off, width) in ITEM_FIELDS {
            // 0..2 are `ITEM_CreateItem`'s own arguments, which it has already
            // applied; the loader writes only the per-instance tail.
            if at <= crate::vault::rec::COUNT {
                continue;
            }
            let mut v = [0u8; 8];
            v[..width].copy_from_slice(&rec[at..at + width]);
            let v = u64::from_le_bytes(v);
            match width {
                1 => self.mem.w8(item.wrapping_add(off), v as u8),
                2 => self.mem.w16(item.wrapping_add(off), v as u16),
                _ => {
                    self.mem.w32(item.wrapping_add(off), v as u32);
                    self.mem.w32(item.wrapping_add(off + 4), (v >> 32) as u32);
                }
            }
        }
        let before = self.bag_count(tab);
        self.call(add, &[item, tab as u32])?;
        if self.bag_count(tab) != before + 1 {
            return Err(format!("tab {tab} would not take the item"));
        }
        Ok(tab)
    }

    /// Which tab will take one of `(kind, idx)`, asking the guest's own
    /// `Item_canSaveWithCnt` per tab exactly as `ItemBag_checkAndSetItem` does
    /// — and requiring a free *slot*, because this host always links a new
    /// entry rather than merging into a stack.
    fn bag_room_for(&mut self, kind: u32, idx: i32, count: i32) -> Result<Option<i32>, String> {
        let (Some(storage), Some(left), Some(countable), Some(can)) = (
            self.bag_storage(),
            self.symbol("ItemBag_leftSlotCnt"),
            self.symbol("Item_isCountable"),
            self.symbol("Item_canSaveWithCnt"),
        ) else {
            return Ok(None);
        };
        let stack = if self.call(countable, &[kind])? != 0 { 0x63 } else { 1 };
        for tab in 0..self.bag_tabs() {
            let free = self.call(left, &[storage, tab as u32])? as u8 as i8 as i32;
            if free <= 0 || self.bag_count(tab) >= BAG_TAB_SLOTS {
                continue;
            }
            let Some(list) = self.bag_list(tab) else { continue };
            let fits = self
                .call(can, &[list, kind, idx as u32, count as u32, stack, free as u32, 1])?
                as u8 as i8;
            if fits > 0 {
                return Ok(Some(tab));
            }
        }
        Ok(None)
    }

    /// Take the whole stack at `(tab, index)` out of the bag, through the
    /// shipped `ITEMSTORAGE_removeFocusedItem` — which drops the count,
    /// unlinks the entry, returns the object to the pool (`LinkedList_remove`
    /// calls `MP_return`) and pulls the cursor back if it was the last one.
    /// 외형변경 uses the same call to consume its donor.
    pub(crate) fn bag_take(&mut self, tab: i32, index: i32, count: i32) -> Result<bool, String> {
        let Some(remove) = self.symbol("ITEMSTORAGE_removeFocusedItem") else {
            return Err("ITEMSTORAGE_removeFocusedItem is missing".into());
        };
        let before = self.bag_count(tab);
        self.call(remove, &[tab as u32, index as u32, count as u32])?;
        Ok(self.bag_count(tab) == before - 1)
    }

    /// A stamp no item in the destination bag — or in the chest, or the
    /// guest's own last-issued one — already has.
    ///
    /// `ITEM_CreateItem` mints from `MC_knlCurrentTime()` and only dedupes
    /// against the immediately previous value (`_menuOpt`), so it is unique
    /// within a run and no further. A chest hands items between runs and
    /// between slots, which is precisely the case that breaks; CLAUDE.md's
    /// 「인벤의 둔기가 주무기 자리를 뺏던 것」 is what the collision looks
    /// like. So take the maximum of everything in reach and step past it, and
    /// **push the guest's own counter up too**, so its next mint cannot land
    /// on ours either.
    pub(crate) fn next_stamp(&mut self) -> Result<i64, String> {
        let mut top = self.wall_now_ms().unwrap_or(0);
        for (_, _, item) in self.bag_walk()? {
            top = top.max(self.item_stamp(item));
        }
        if let Some(m) = self.vault.max_stamp() {
            top = top.max(m);
        }
        let last = self.symbol("_menuOpt").map(|g| {
            let lo = u64::from(self.mem.r32(g));
            let hi = u64::from(self.mem.r32(g + 4));
            ((hi << 32) | lo) as i64
        });
        if let Some(l) = last {
            top = top.max(l);
        }
        let next = top.saturating_add(1);
        if let Some(g) = self.symbol("_menuOpt") {
            self.mem.w32(g, next as u32);
            self.mem.w32(g + 4, (next >> 32) as u32);
        }
        Ok(next)
    }

    /// Step 2, both directions: write the slot the way the letter screen does
    /// after it takes the attached item out of the bag
    /// (`MainMenuFrame_afterSendLetterWithItem`) — point `_selectSlotNumber`
    /// at the slot, `GameFrame_save_hero_data(1)`, `GameFrame_saveGame()`.
    ///
    /// Not `GameFrame_autoSave`: that one passes 0, also writes the pet file
    /// and puts a failure message on screen. The letter path is the closer
    /// analogue — a main-menu edit to a loaded slot's bag — and it is the only
    /// shipped code that does this from here.
    pub(crate) fn save_slot(&mut self, slot: u8) -> Result<(), String> {
        let (Some(sel), Some(hero), Some(game)) = (
            self.symbol("_selectSlotNumber"),
            self.symbol("GameFrame_save_hero_data"),
            self.symbol("GameFrame_saveGame"),
        ) else {
            return Err("the save entry points are missing".into());
        };
        self.mem.w8(sel, slot);
        self.call(hero, &[1])?;
        self.call(game, &[])?;
        Ok(())
    }

    /// One line per bag entry and one per chest entry, for the probe: this is
    /// how a transfer is checked without a screen to look at, and the stamps
    /// are on it because "in exactly one place" is what the protocol promises
    /// and the stamp is what identifies the place.
    pub fn vault_describe(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        for h in self.vault.items() {
            out.push(format!(
                "chest {}: kind {} idx {} x{} stamp {}",
                h.id,
                h.rec[crate::vault::rec::KIND],
                h.rec[crate::vault::rec::IDX] as i8,
                h.rec[crate::vault::rec::COUNT] as i8,
                crate::vault::stamp_of(&h.rec),
            ));
        }
        if let Some(p) = self.vault.pending() {
            out.push(format!(
                "chest pending: {:?} slot {} id {} stamp {}",
                p.op,
                p.slot,
                p.id,
                p.stamp()
            ));
        }
        out.push(format!("chest gold {}", self.vault.gold()));
        if let Some(g) = self.vault.gold_pending() {
            out.push(format!(
                "chest pending gold: {:?} slot {} amount {} before {}",
                g.op, g.slot, g.amount, g.before
            ));
        }
        if let Ok((_, gold)) = self.hero_gold() {
            out.push(format!("hero gold {gold}"));
        }
        if let Some(why) = self.vault.broken() {
            out.push(format!("chest is out of order: {why}"));
        }
        let slots: Vec<String> = (1..=3).map(|s| format!("{s}:{:?}", self.slot_class(s))).collect();
        out.push(format!("save slots {}", slots.join(" ")));
        for (tab, i, item) in self.bag_walk().unwrap_or_default() {
            let kind = self.mem.r8(item.wrapping_add(0x96));
            let idx = self.mem.r8(item) as i8;
            let n = self.mem.r8(item.wrapping_add(0x97)) as i8;
            let worn = self.mem.r8(item.wrapping_add(ITEM_EQUIPPED)) as i8;
            out.push(format!(
                "bag {tab}/{i}: kind {kind} idx {idx} x{n} worn {worn} stamp {}",
                self.item_stamp(item)
            ));
        }
        out
    }

    /// Deposit the bag entry at `(tab, index)` and take it straight back out,
    /// then compare the record the bag holds now with the one it held before.
    ///
    /// Everything has to match except the stamp, which a withdraw deliberately
    /// re-mints ([`Vm::next_stamp`]), and the tab, which belongs to the bag.
    /// This is the check that the chest does not quietly eat an enchant level
    /// or a stat halfword on the way through — the failure a player would only
    /// notice a week later, with the old item gone.
    pub fn vault_round_trip(&mut self, slot: u8, tab: i32, index: i32) -> Result<String, String> {
        let item = self.bag_item(tab, index)?;
        if item == 0 {
            return Err(format!("nothing at tab {tab} slot {index}"));
        }
        let before = self.item_record(item, tab);
        self.vault_deposit(slot, tab, index)?;
        let Some(id) = self.vault.items().last().map(|h| h.id) else {
            return Err("the deposit did not land".into());
        };
        let stamp = self.vault_withdraw(slot, id)?;
        let Some((tab2, i2, back)) = self.bag_find(stamp)? else {
            return Err("the withdrawn item is not in the bag".into());
        };
        let after = self.item_record(back, tab2);
        let mut bad = Vec::new();
        for &(at, off, width) in ITEM_FIELDS {
            if off == ITEM_STAMP {
                continue; // deliberately re-minted
            }
            if before[at..at + width] != after[at..at + width] {
                bad.push(format!("item[{off:#x}]"));
            }
        }
        if bad.is_empty() {
            Ok(format!("round trip kept every field (now at tab {tab2} slot {i2})"))
        } else {
            Err(format!("round trip lost {}", bad.join(", ")))
        }
    }

    /// Which save slot is loaded, 1-based, as `_selectSlotNumber` has it.
    pub fn current_slot(&mut self) -> u8 {
        self.symbol("_selectSlotNumber").map_or(0, |s| self.mem.r8(s))
    }

    /// The class in a save slot, or `None` for an empty one. `_saveSlot` is
    /// three bytes per slot and the first is the class, `-1` when there is no
    /// save — the same test `MainMenuFrame_KeyPressed+0x2760` makes before it
    /// will load one.
    pub(crate) fn slot_class(&mut self, slot: u8) -> Option<u8> {
        let table = self.symbol("_saveSlot")?;
        if slot == 0 || slot > 3 {
            return None;
        }
        let c = self.mem.r8(table.wrapping_add(3 * u32::from(slot - 1)));
        (c as i8 != -1).then_some(c)
    }

    /// Bring a save slot into memory, verbatim the recipe the letter screen
    /// runs at `MainMenuFrame_KeyPressed+0x2760` when the player picks a slot
    /// to attach an item from — talents, class, shortcut slots, skills, stats,
    /// then the hero file, then the game file, then the pet file if there is a
    /// pet. Nothing here is ours; the order is the guest's.
    pub fn load_slot(&mut self, slot: u8) -> Result<bool, String> {
        let Some(class) = self.slot_class(slot) else { return Ok(false) };
        let (
            Some(talent),
            Some(get_hero),
            Some(shortcut),
            Some(skills),
            Some(stats),
            Some(load_hero),
            Some(load_game),
            Some(sel),
        ) = (
            self.symbol("Load_TalentData"),
            self.symbol("MP_getHero"),
            self.symbol("Init_Shortcut_Slot"),
            self.symbol("HERO_Load_SkillData"),
            self.symbol("HERO_InitHeroStatus"),
            self.symbol("GameFrame_load_hero_data"),
            self.symbol("GameFrame_loadGame"),
            self.symbol("_selectSlotNumber"),
        )
        else {
            return Err("the slot loader's entry points are missing".into());
        };
        self.call(talent, &[u32::from(class)])?;
        let hero = self.call(get_hero, &[0])?;
        self.mem.w8(hero.wrapping_add(HERO_CLASS), class);
        self.call(shortcut, &[])?;
        self.call(skills, &[0])?;
        self.call(shortcut, &[])?;
        self.call(stats, &[0])?;
        self.call(load_hero, &[u32::from(slot)])?;
        self.mem.w8(sel, slot);
        self.call(load_game, &[u32::from(slot)])?;
        let hero = self.call(get_hero, &[0])?;
        if self.mem.r8(hero.wrapping_add(HERO_HAS_PET)) != 0 {
            if let Some(pet) = self.symbol("GameFrame_loadPetData") {
                self.call(pet, &[u32::from(slot)])?;
            }
        }
        Ok(true)
    }

    /// Step 1..3 of a deposit: the bag entry at `(tab, index)` moves into the
    /// chest.
    ///
    /// The three writes are in the order the module docs set out, and **step 3
    /// asks the bag rather than trusting step 2**: the happy path settles on
    /// the same evidence a resume would, so there is one answer to "did it
    /// land" and not two.
    pub fn vault_deposit(&mut self, slot: u8, tab: i32, index: i32) -> Result<(), String> {
        if !vault_on() {
            return Err("the chest is switched off".into());
        }
        if let Some(why) = self.vault.broken() {
            return Err(why.to_string());
        }
        if self.vault.in_flight() {
            return Err("a move is already in flight".into());
        }
        if !self.vault.has_room() {
            return Err("the chest is full".into());
        }
        let item = self.bag_item(tab, index)?;
        if item == 0 {
            return Err(format!("nothing in the bag at tab {tab} slot {index}"));
        }
        // Equipment that is being worn is reachable from `hero + 0x65c` and
        // from the bag at once; taking it out from under the hero would leave
        // a dangling pointer for `Item_loadStorage` to fail to match.
        if self.mem.r8(item.wrapping_add(ITEM_EQUIPPED)) as i8 >= 0 {
            return Err("that one is being worn".into());
        }
        // **The stamp has to identify one item, and the game's does not.**
        //
        // `ITEM_CreateItem` mints from `MC_knlCurrentTime()` and dedupes only
        // against the immediately previous value, so two items made in the same
        // millisecond a mint apart carry the same number — the four starting
        // pieces are two such pairs, kind 0 with kind 12 and kind 5 with kind
        // 15, whatever numbers a given run mints for them. A resume asks "is
        // this stamp in the bag?", so with a
        // twin still there the answer after a successful step 2 is *yes*, the
        // verdict is "step 2 never landed", and the deposit rolls back onto an
        // item that is already gone. That destroyed a hammer in the probe.
        //
        // So make it unique first, and **save that before arming**: a crash
        // between the re-stamp and the save leaves an item with one stamp or
        // the other and no record pointing at it, which is nothing at all,
        // whereas arming first would leave a record naming a stamp the saved
        // bag does not carry — a duplicate. The item is not worn (refused
        // above), so nothing else points at it, and the worn twin keeping the
        // old number is how `host_equip_restore` wanted it in the first place.
        let had = self.item_stamp(item);
        if self.bag_stamp_twins(had)? > 1 {
            let fresh = self.next_stamp()?;
            log::info!("vault: the item's stamp is shared in this bag; re-minting it as {fresh}");
            self.mem.w32(item.wrapping_add(ITEM_STAMP), fresh as u32);
            self.mem.w32(item.wrapping_add(ITEM_STAMP + 4), (fresh >> 32) as u32);
            self.save_slot(slot)?;
        }
        let rec = self.item_record(item, tab);
        let count = rec[crate::vault::rec::COUNT] as i8 as i32;
        let stamp = crate::vault::stamp_of(&rec);

        let id = self.vault.arm_deposit(slot, rec)?; // 1
        if self.vault_stop_after == Some(1) {
            return Err("stopped after step 1".into());
        }
        self.bag_take(tab, index, count)?; // 2
        self.save_slot(slot)?;
        if self.vault_stop_after == Some(2) {
            return Err("stopped after step 2".into());
        }
        let in_bag = self.bag_find(stamp)?.is_some(); // 3
        let what = self.vault.settle(in_bag)?;
        log::info!("vault: deposit of item {id} from slot {slot} -> {what:?}");
        Ok(())
    }

    /// Step 1..3 of a withdraw: chest entry `id` becomes a bag entry in `slot`.
    ///
    /// The stamp is picked **before** step 1 ([`Vm::next_stamp`]) and written
    /// into the record, because the item does not exist yet and a resume has
    /// nothing else to recognise it by.
    /// Returns the stamp the withdrawn item carries, so a caller can find it
    /// in the bag.
    pub fn vault_withdraw(&mut self, slot: u8, id: u64) -> Result<i64, String> {
        if !vault_on() {
            return Err("the chest is switched off".into());
        }
        if let Some(why) = self.vault.broken() {
            return Err(why.to_string());
        }
        if self.vault.in_flight() {
            return Err("a move is already in flight".into());
        }
        let stamp = self.next_stamp()?;
        let rec = self.vault.arm_withdraw(slot, id, stamp)?; // 1
        if self.vault_stop_after == Some(1) {
            return Err("stopped after step 1".into());
        }
        let put = self.bag_put(&rec); // 2
        if put.is_ok() {
            self.save_slot(slot)?;
        }
        if self.vault_stop_after == Some(2) {
            return Err("stopped after step 2".into());
        }
        let in_bag = self.bag_find(stamp)?.is_some(); // 3
        let what = self.vault.settle(in_bag)?;
        match put {
            // The bag would not take it, so nothing moved and the chest kept
            // it: that is a refusal to report, not a failure to recover from.
            Err(why) if !in_bag => Err(why),
            _ => {
                log::info!("vault: withdraw of item {id} into slot {slot} -> {what:?}");
                Ok(stamp)
            }
        }
    }

    /// Finish a move that a crash left half-done, **loading whatever slot it
    /// belongs to** to ask its bag — or, for gold, its hero — the question.
    ///
    /// Runs at the first drawing of the 3x3 tile menu (`host_my_id_tile`) and
    /// again on the way into the chest, so a half-done move is put right
    /// before the player can start a game on that slot and change what the
    /// question would find. `at_menu` says the caller is on the main menu,
    /// where two things hold that hold nowhere else: `_saveSlot` is filled in
    /// (only `MainMenuFrame_Initialize` fills it), and the hero in memory is
    /// whichever slot that initialiser loaded last, whatever
    /// `_selectSlotNumber` says — so the slot is **always** loaded there.
    /// Off the menu (the probe's `--vault resume`, in a running game) the
    /// loaded slot is trusted when it is the one.
    ///
    /// **Nothing settles without the bag** — with one exception. On the menu,
    /// a slot the slot list calls empty is a character that was deleted, and
    /// the other side of the move went with it; [`Vm::vault_settle_gone`] is
    /// the table for that. Off the menu an unloadable slot is not evidence of
    /// anything (`_saveSlot` reads as three empty slots while a game is
    /// running), so the move stays in flight and the next attempt tries again.
    /// Guessing is the one thing that can duplicate or destroy the item.
    pub fn vault_resume(
        &mut self,
        at_menu: bool,
    ) -> Result<Option<crate::vault::Settled>, String> {
        if !vault_on() {
            return Ok(None);
        }
        let item = self.vault.pending().map(|p| (p.slot, p.stamp()));
        let gold = self.vault.gold_pending();
        let slot = match (item, gold) {
            (Some((slot, _)), _) => slot,
            (None, Some(g)) => g.slot,
            (None, None) => return Ok(None),
        };
        if at_menu || slot != self.current_slot() {
            log::info!("vault: a move for slot {slot} is in flight; loading it to settle");
            if !self.load_slot(slot)? {
                if at_menu {
                    log::warn!(
                        "vault: slot {slot} is empty, so the character the move was for is \
                         gone; settling it as such"
                    );
                    return self.vault_settle_gone();
                }
                log::warn!(
                    "vault: slot {slot} will not load, so the move stays in flight — \
                     the item is still wherever it was, and opening that slot settles it"
                );
                return Ok(None);
            }
        }
        if let Some((_, stamp)) = item {
            let in_bag = self.bag_find(stamp)?.is_some();
            return Ok(Some(self.vault.settle(in_bag)?));
        }
        let (_, now) = self.hero_gold()?;
        Ok(Some(self.vault.settle_gold(now)?))
    }

    /// Settle a move whose slot the slot list says is empty.
    ///
    /// The character is gone, and with it the bag or the hero that was the
    /// other side. Something on its way **in** is committed: it either left
    /// the bag before the character did or went down with it, and the chest
    /// is the only place that can still hold it. Something on its way **out**
    /// is rolled back: it either never arrived or arrived in a bag that no
    /// longer exists, and the chest's copy is the only one. Neither cell
    /// duplicates anything — it is `verdict(op, in_bag = false)`, the row the
    /// protocol already has for "the bag does not have it".
    fn vault_settle_gone(&mut self) -> Result<Option<crate::vault::Settled>, String> {
        if let Some(g) = self.vault.gold_pending() {
            let commit = g.op == crate::vault::Op::Deposit;
            return Ok(Some(self.vault.settle_gold_as(commit)?));
        }
        if self.vault.pending().is_some() {
            return Ok(Some(self.vault.settle(false)?));
        }
        Ok(None)
    }

    // ---- the chest as a bag -------------------------------------------------

    /// Where the selected-slot byte sits in the storage object, and how big
    /// that object is.
    ///
    /// Both follow from the same layout — a tab byte, a tab count, `BAG_MAX`
    /// twelve-byte `LinkedList` headers from +4, then the cursor — so the
    /// cursor is at `4 + 12 * BAG_MAX` and the struct is four bytes longer:
    /// 0x40/0x44 for the shipped five bags, 0x7c/0x80 for the cave's ten
    /// (`cave/src/guest.h`'s `BAGSLOT_OFF`, and the offset every one of the 112
    /// rewritten immediates carries). `_storage`'s `st_size` is checked against
    /// it by `the_storage_object_is_the_shape_the_chest_view_copies`; the
    /// cave's array has no `st_size` to check, which is why this is written out
    /// rather than read.
    fn bag_cursor_off(&mut self) -> u32 {
        if self.cave_installed.contains(&"bag10") {
            BAG_CURSOR_CAVE
        } else {
            BAG_CURSOR
        }
    }

    /// How many bytes [`Vm::with_chest`] copies in and out. A short count would
    /// leave half of one view inside the other.
    fn bag_storage_size(&mut self) -> Option<usize> {
        Some(self.bag_cursor_off() as usize + 4)
    }

    /// Run `f` with the chest's storage image in the guest's own storage
    /// object, and the bag's back afterwards.
    ///
    /// This is the whole trick behind the chest panel. `drawItemBag`,
    /// `ItemBagKey`, `ITEM_getItemFrom`, `ITEMSTORAGE_addItem` and
    /// `ITEMSTORAGE_removeFocusedItem` all reach the bag through that one
    /// address, and the struct holds no pointer to itself — a tab byte, a tab
    /// count, a 12-byte `LinkedList` header per tab and a cursor byte — so
    /// swapping its bytes swaps which list every one of them means.
    ///
    /// Two things make it safe rather than clever. The window is one call
    /// long, so nothing can observe the swap except the shipped function that
    /// is meant to; and in particular `Item_saveStorage`, which reads the same
    /// object during step 2 of every transfer, runs from
    /// [`Vm::vault_deposit`]/[`Vm::vault_withdraw`] — outside every window.
    /// The image is written back on the way out, so a cursor or tab the player
    /// moved inside the chest is kept.
    fn with_chest<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, String>,
    ) -> Result<T, String> {
        let (Some(at), Some(n)) = (self.bag_storage(), self.bag_storage_size()) else {
            return Err("the storage object is missing".into());
        };
        if self.vault_view.len() != n {
            return Err("the chest view is not the shape of a bag".into());
        }
        let bag: Vec<u8> = (0..n as u32).map(|i| self.mem.r8(at + i)).collect();
        self.mem.write_bytes(at, &self.vault_view.clone());
        let r = f(self);
        self.vault_view = (0..n as u32).map(|i| self.mem.r8(at + i)).collect();
        self.mem.write_bytes(at, &bag);
        r
    }

    /// Build the chest's storage image: one tab, one live item per entry, in
    /// the order the file has them.
    ///
    /// The items are real — `ITEM_CreateItem` plus the loader's own field
    /// restore, exactly what [`Vm::bag_put`] does for a withdraw — because that
    /// is what the panel draws: icon, stack count, name, grade colour and the
    /// equip check all come off a live object. They are handed back to the pool
    /// by [`Vm::vault_drop_view`] when the screen closes.
    ///
    /// One tab, because the chest holds [`crate::vault::slots`] entries and the
    /// default 20 is one page of the panel's 4x5 grid.
    fn vault_build_view(&mut self) -> Result<(), String> {
        self.vault_drop_view()?;
        let (Some(n), Some(init)) = (self.bag_storage_size(), self.symbol("ItemBag_init")) else {
            return Err("the storage object is missing".into());
        };
        self.vault_view = vec![0u8; n];
        let held: Vec<crate::vault::Held> = self.vault.items().to_vec();
        let cursor = self.bag_cursor_off();
        self.with_chest(|vm| {
            let at = vm.bag_storage().ok_or("the storage object is missing")?;
            vm.call(init, &[at, 0])?;
            vm.mem.w8(at + 1, 0); // tab 0
            vm.mem.w8(at + 2, 1); // one tab
            vm.mem.w8(at + cursor, 0);
            for h in &held {
                // The chest's own order is the list's order, so a cursor
                // position maps to an entry; the transfer still re-reads the
                // stamp off the item rather than trusting that.
                vm.bag_put_in(&h.rec, 0)?;
            }
            Ok(())
        })?;
        self.vault_ids = held.iter().map(|h| h.id).collect();
        Ok(())
    }

    /// Hand every materialised chest item back to the pool.
    ///
    /// Through `ITEMSTORAGE_removeFocusedItem`, which is the call
    /// [`Vm::bag_take`] already relies on to unlink an entry and `MP_return`
    /// its object — 외형변경 consumes its donor the same way. Leaving them
    /// linked would leak one pool slot per chest entry per visit, and the pool
    /// is what the bag itself is drawn from.
    fn vault_drop_view(&mut self) -> Result<(), String> {
        if self.vault_view.is_empty() {
            return Ok(());
        }
        let r = self.with_chest(|vm| {
            while vm.bag_count(0) > 0 {
                let item = vm.bag_item(0, 0)?;
                let count = if item == 0 {
                    1
                } else {
                    i32::from(vm.mem.r8(item.wrapping_add(0x97)) as i8).max(1)
                };
                if !vm.bag_take(0, 0, count)? {
                    return Err("the chest view will not empty".into());
                }
            }
            Ok(())
        });
        self.vault_view = Vec::new();
        self.vault_ids = Vec::new();
        r
    }

    // ---- the screen ---------------------------------------------------------

    /// Open the chest: put the letter frame on its slot list.
    ///
    /// Called from the tile, in place of the `*_menuState = _menuCursor` that
    /// would have sent tile 1 to 「사용되지 않는 메뉴입니다」. `_menuDepthState`
    /// and the slot cursor have already been zeroed by the guest four
    /// instructions earlier, so the only state to write is which frame.
    ///
    /// [`Vm::vault_resume`] runs **here**, before anything is shown. A move
    /// left in flight by a crash belongs to whichever slot it was made from,
    /// which is not necessarily the one the player is about to pick, so it has
    /// to be settled before the chest is listed — and the main menu is the one
    /// place `_saveSlot` is populated, which is what makes the question
    /// answerable at all.
    pub(crate) fn vault_enter(&mut self) -> Result<(), String> {
        if let Some(why) = self.vault.broken() {
            log::warn!("vault: {why}");
            self.popup(VAULT_BROKEN)?;
            return Ok(());
        }
        match self.vault_resume(true) {
            Ok(Some(what)) => log::info!("vault: the move left in flight was {what:?}"),
            Ok(None) => {}
            Err(why) => log::error!("vault: could not settle the move in flight: {why}"),
        }
        let (Some(state), Some(letter)) = (self.symbol("_menuState"), self.symbol("_mLetterState"))
        else {
            return Ok(());
        };
        self.mem.w32(state, LETTER_FRAME);
        self.mem.w8(letter, 0);
        self.vault_screen = crate::vault::Screen::Slots;
        self.vault_panel = crate::vault::Panel::Bag;
        Ok(())
    }

    /// Leave the grid: back to the slot list, with the chest's items returned
    /// to the pool.
    fn vault_leave_grid(&mut self) -> Result<(), String> {
        self.vault_drop_view()?;
        if let (Some(depth), Some(letter)) =
            (self.symbol("_menuDepthState"), self.symbol("_mLetterState"))
        {
            self.mem.w8(depth, 0);
            self.mem.w8(letter, 0);
        }
        self.vault_screen = crate::vault::Screen::Slots;
        Ok(())
    }

    /// The letter frame is no longer ours. Called by the thing that draws the
    /// 3x3 tile menu, which is the one screen the chest can never be on top of,
    /// so no exit path has to remember to do this.
    pub(crate) fn vault_off(&mut self) {
        if self.vault_screen == crate::vault::Screen::Off {
            return;
        }
        self.vault_screen = crate::vault::Screen::Off;
        self.vault_gold_input = None;
        // The view's items are pool objects; dropping them here rather than on
        // each exit path is the same reason.
        if let Err(why) = self.vault_drop_view() {
            log::error!("vault: the chest view would not close: {why}");
        }
    }

    /// The slot list answered: load that save and show its bag.
    ///
    /// Stands in for `MainMenuFrame_KeyPressed+0x2760`, whose nine `bl` are
    /// [`Vm::load_slot`] instruction for instruction. Substituting rather than
    /// letting the shipped block run is what keeps the frame — the shipped tail
    /// falls out to the epilogue with the letter still at depth 0, and the
    /// chest wants depth 2.
    pub(crate) fn host_vault_slot_ok(&mut self) -> Result<bool, String> {
        if self.vault_screen != crate::vault::Screen::Slots {
            return Ok(false); // 편지's own slot list is the guest's
        }
        let Some(base) = self.symbol("MainMenuFrame_KeyPressed") else { return Ok(false) };
        self.cpu.reg_set(Mode::User, reg::PC, base.wrapping_add(VAULT_KEY_EXIT));
        let Some(cur) = self.symbol("_menuDepthCur") else { return Ok(true) };
        let slot = self.mem.r8(cur) as i8;
        if !(0..3).contains(&slot) {
            return Ok(true);
        }
        let slot = slot as u8 + 1;
        if self.slot_class(slot).is_none() {
            self.popup(VAULT_EMPTY_SLOT)?;
            return Ok(true);
        }
        if !self.load_slot(slot)? {
            self.popup(VAULT_EMPTY_SLOT)?;
            return Ok(true);
        }
        self.vault_slot = slot;
        self.vault_panel = crate::vault::Panel::Bag;
        // Start both panels at the top left. The bag's cursor and tab come out
        // of the save the load just read, which is not where the player left
        // this screen.
        let cursor = self.bag_cursor_off();
        if let (Some(at), Some(strip)) = (self.bag_storage(), self.symbol("_isSelectBag")) {
            self.mem.w8(at + 1, 0);
            self.mem.w8(at + cursor, 0);
            self.mem.w8(strip, 0);
        }
        // A chest that cannot be laid out is not an empty chest. The items are
        // pool objects and the pool is finite — a full five-bag save on a build
        // without the cave's bigger one leaves fewer than twenty free — so a
        // failure here has to keep the player on the slot list rather than open
        // a grid that would show the chest as emptier than it is.
        if let Err(why) = self.vault_build_view() {
            log::error!("vault: the chest will not open: {why}");
            self.vault_drop_view()?;
            self.popup(VAULT_BROKEN)?;
            return Ok(true);
        }
        if let (Some(depth), Some(letter)) =
            (self.symbol("_menuDepthState"), self.symbol("_mLetterState"))
        {
            self.mem.w8(depth, LETTER_DEPTH_INNER);
            self.mem.w8(letter, LETTER_STATE_BAG);
        }
        self.vault_screen = crate::vault::Screen::Grid;
        log::info!("vault: slot {slot} loaded; {} in the chest", self.vault_ids.len());
        Ok(true)
    }

    /// The grid's keys.
    ///
    /// Stands in for `MainMenuFrame_ItemBagKeyPressed`, and re-uses the half of
    /// it that matters: `ItemBagKey` does the cursor, the tab strip, the touch
    /// hit-testing and the paging, and answers [`BAG_KEY_BACK`] or
    /// [`BAG_KEY_PICK`]. What is replaced is the tail, which files the choice
    /// in `_mailBox+0xb6`/`+0xb8` — the letter's attachment slots, and writing
    /// a chest transfer into them would corrupt a letter being composed.
    ///
    /// The panel switch is read **before** the body, because the two title
    /// words sit above the grid where `ItemBagKey` hit-tests nothing.
    pub(crate) fn host_vault_bag_key(&mut self) -> Result<bool, String> {
        if self.vault_screen != crate::vault::Screen::Grid {
            return Ok(false); // the letter's own bag grid is the guest's
        }
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        if lr == crate::RETURN_MAGIC {
            return Ok(false);
        }
        let (event, a, b) = (
            self.cpu.reg_get(Mode::User, 0) as i32,
            self.cpu.reg_get(Mode::User, 1),
            self.cpu.reg_get(Mode::User, 2),
        );
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        if event == TOUCH_EVENT {
            if let Some(op) = self.vault_gold_hit(a as i32, b as i32)? {
                self.open_vault_gold_input(op)?;
                return Ok(true);
            }
            if let Some(panel) = self.vault_title_hit(a as i32, b as i32)? {
                if panel != self.vault_panel {
                    self.vault_panel = panel;
                }
                return Ok(true);
            }
        }
        let Some(keys) = self.symbol("ItemBagKey") else { return Ok(true) };
        let chest = self.vault_panel == crate::vault::Panel::Chest;
        let picked = |vm: &mut Self| -> Result<Picked, String> {
            let r = vm.call(keys, &[event as u32, a, b, 0])? & 0xff;
            if r != BAG_KEY_PICK {
                return Ok((r, None));
            }
            let cursor = vm.bag_cursor_off();
            let Some(at) = vm.bag_storage() else { return Ok((r, None)) };
            let tab = i32::from(vm.mem.r8(at + 1) as i8);
            let index = i32::from(vm.mem.r8(at + cursor) as i8);
            let item = vm.bag_item(tab, index)?;
            Ok((r, (item != 0).then_some((tab, index, item))))
        };
        let (answer, pick) = if chest { self.with_chest(picked)? } else { picked(self)? };
        match answer {
            BAG_KEY_BACK => self.vault_leave_grid()?,
            BAG_KEY_PICK => {
                if let Some((tab, index, item)) = pick {
                    self.vault_move(chest, tab, index, item)?;
                }
            }
            _ => {}
        }
        Ok(true)
    }

    /// Move the item the grid just picked, and say why not when it cannot.
    ///
    /// Everything that can refuse does so **before** step 1, so a refusal
    /// leaves the item exactly where it was and the message is the whole
    /// outcome.
    ///
    /// The worn check is here as well as in [`Vm::vault_deposit`] because only
    /// here is there a screen to say it on. Measured, the shipped grid will not
    /// even hand a worn entry over to a *touch* — tapping either equipped piece
    /// does nothing at all, which is the same rule the letter screen relies on
    /// — but `MainMenuFrame_ItemBagKeyPressed` still tests `item[0x98]` after
    /// the pick, so the key and pad path can get one here. Without this the
    /// refusal would come back wearing the chest-is-full wording.
    fn vault_move(&mut self, chest: bool, tab: i32, index: i32, item: u32) -> Result<(), String> {
        let slot = self.vault_slot;
        let outcome = if chest {
            // The view's order is the file's order, but the **stamp** is what
            // identifies an entry, so the id comes off the item the cursor is
            // actually on and the position is only the fallback.
            let stamp = self.item_stamp(item);
            let by_stamp =
                self.vault.items().iter().find(|h| crate::vault::stamp_of(&h.rec) == stamp);
            let Some(id) =
                by_stamp.map(|h| h.id).or_else(|| self.vault_ids.get(index as usize).copied())
            else {
                return Ok(());
            };
            self.vault_withdraw(slot, id).map(|_| ()).map_err(|why| (why, VAULT_NO_ROOM))
        } else {
            if self.mem.r8(item.wrapping_add(ITEM_EQUIPPED)) as i8 >= 0 {
                self.popup(VAULT_WORN)?;
                return Ok(());
            }
            if !self.vault.has_room() {
                self.popup(VAULT_FULL)?;
                return Ok(());
            }
            self.vault_deposit(slot, tab, index).map_err(|why| (why, VAULT_FULL))
        };
        match outcome {
            Ok(()) => {
                // The chest changed, so the view has to be rebuilt — and if it
                // cannot be, the grid must not stay up showing a stale one.
                if let Err(why) = self.vault_build_view() {
                    log::error!("vault: the chest will not re-open: {why}");
                    self.vault_leave_grid()?;
                    self.popup(VAULT_BROKEN)?;
                    return Ok(());
                }
                self.vault_close_info();
            }
            Err((why, msg)) => {
                log::info!("vault: the move was refused: {why}");
                let msg = if self.vault.broken().is_some() { VAULT_BROKEN } else { msg };
                self.popup(msg)?;
            }
        }
        Ok(())
    }

    /// Put the item-info panel away.
    ///
    /// The second tap on a cell is both "picked" and "show me this one", so a
    /// transfer always leaves that panel open over the grid — describing
    /// whatever has slid into the cursor's place, which is not what was asked
    /// for and which covers the half of the screen the player is about to use.
    /// `_ItemInfoPop` is the byte `drawItemBag` draws it from and `ItemBagKey`
    /// raises.
    fn vault_close_info(&mut self) {
        if let Some(at) = self.symbol("_ItemInfoPop") {
            self.mem.w8(at, 0);
        }
    }

    /// The grid's drawing: the shipped panel, then the two panel words.
    ///
    /// `draw_CommonItembag` is run through [`Vm::call`] so the chest panel can
    /// have the chest's storage under it for exactly that call. Its own heading
    /// is emptied by [`Vm::host_in_code_string`] rather than skipped, so no
    /// shipped instruction is left out.
    pub(crate) fn host_vault_bag_draw(&mut self) -> Result<bool, String> {
        if self.vault_screen != crate::vault::Screen::Grid {
            return Ok(false);
        }
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        if lr == crate::RETURN_MAGIC {
            return Ok(false); // our own re-issue
        }
        let Some(pc) = self.symbol("draw_CommonItembag") else { return Ok(false) };
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        if self.vault_panel == crate::vault::Panel::Chest {
            self.with_chest(|vm| vm.call(pc, &[]))?;
        } else {
            self.call(pc, &[])?;
        }
        self.draw_vault_title()?;
        self.draw_vault_gold()?;
        Ok(true)
    }

    /// The slot list's heading, so the three boxes say which screen they are
    /// standing in for. `MainMenuFrame_drawMenuSlot` draws none of its own.
    pub(crate) fn host_vault_slot_draw(&mut self) -> Result<bool, String> {
        if self.vault_screen != crate::vault::Screen::Slots {
            return Ok(false); // 캐릭관리, 편지 and 시작 share this widget
        }
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        if lr == crate::RETURN_MAGIC {
            return Ok(false);
        }
        let Some(pc) = self.symbol("MainMenuFrame_drawMenuSlot") else { return Ok(false) };
        let arg = self.cpu.reg_get(Mode::User, 0);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        self.call(pc, &[arg])?;
        let Some((cx, cy)) = self.screen_middle() else { return Ok(true) };
        let lit = match self.symbol("MC_grpGetPixelFromRGB") {
            Some(f) => self.call(f, &[VAULT_LIT.0, VAULT_LIT.1, VAULT_LIT.2])?,
            None => HUD_C1,
        };
        self.draw_line(cx + VAULT_SLOT_TITLE_DX, cy + VAULT_SLOT_TITLE_DY, VAULT_LABEL, lit)?;
        Ok(true)
    }

    /// The two words that name the panels, in the band the shipped heading
    /// had. The one being shown is white; the other is dimmed, and touching it
    /// switches.
    fn draw_vault_title(&mut self) -> Result<(), String> {
        let Some((cx, cy)) = self.screen_middle() else { return Ok(()) };
        let Some(rgb) = self.symbol("MC_grpGetPixelFromRGB") else { return Ok(()) };
        let lit = self.call(rgb, &[VAULT_LIT.0, VAULT_LIT.1, VAULT_LIT.2])?;
        // The keypad is up for an amount: say which way the gold is going, in
        // the band the panel words have — they are not buttons while it is.
        if let Some(op) = self.vault_gold_input {
            let ask = match op {
                crate::vault::Op::Deposit => GOLD_ASK_PUT,
                crate::vault::Op::Withdraw => GOLD_ASK_TAKE,
            };
            self.draw_line(cx + GOLD_ASK_DX, cy + VAULT_TITLE_DY, ask, lit)?;
            return Ok(());
        }
        let dim = self.call(rgb, &[VAULT_DIM.0, VAULT_DIM.1, VAULT_DIM.2])?;
        let y = cy + VAULT_TITLE_DY;
        for (i, (panel, text)) in
            [(crate::vault::Panel::Bag, VAULT_BAG), (crate::vault::Panel::Chest, VAULT_CHEST)]
                .into_iter()
                .enumerate()
        {
            let x = cx + VAULT_TITLE_DX + i as i32 * (VAULT_TITLE_W / 2);
            let colour = if panel == self.vault_panel { lit } else { dim };
            self.draw_line(x, y, text, colour)?;
        }
        Ok(())
    }

    /// Which panel word a touch landed on, if either.
    ///
    /// Through the guest's own `Check_Rect`, so the target is hit-tested the
    /// way every other button on this screen is — including its haptics.
    fn vault_title_hit(&mut self, x: i32, y: i32) -> Result<Option<crate::vault::Panel>, String> {
        let (Some(check), Some((cx, cy))) = (self.symbol("Check_Rect"), self.screen_middle())
        else {
            return Ok(None);
        };
        let half = VAULT_TITLE_W / 2;
        for (i, panel) in
            [crate::vault::Panel::Bag, crate::vault::Panel::Chest].into_iter().enumerate()
        {
            let rx = cx + VAULT_TITLE_DX + i as i32 * half;
            let ry = cy + VAULT_TITLE_DY;
            let hit = self.call(
                check,
                &[x as u32, y as u32, rx as u32, ry as u32, half as u32, VAULT_TITLE_H as u32],
            )?;
            if hit != 0 {
                return Ok(Some(panel));
            }
        }
        Ok(None)
    }

    // ---- gold ---------------------------------------------------------------

    /// The loaded hero and the gold it holds — `hero + 0x89c`, the word
    /// `drawItemBag` prints as 소지금 at the top right of the grid and
    /// `MainMenuFrame_afterSendLetterWithItem` debits when a letter carries
    /// gold. `the_gold_field_and_cap_are_the_guests_own` pins both.
    pub(crate) fn hero_gold(&mut self) -> Result<(u32, u32), String> {
        let Some(get_hero) = self.symbol("MP_getHero") else {
            return Err("MP_getHero is missing".into());
        };
        let hero = self.call(get_hero, &[0])?;
        if hero == 0 {
            return Err("no hero is loaded".into());
        }
        Ok((hero, self.mem.r32(hero.wrapping_add(HERO_GOLD))))
    }

    /// Probe only: give the loaded hero this much gold, so a transfer has
    /// something to move — a fresh save starts with none. Capped the way the
    /// game caps a credited letter.
    pub fn set_hero_gold(&mut self, gold: u32) -> Result<(), String> {
        let (hero, _) = self.hero_gold()?;
        self.mem.w32(hero.wrapping_add(HERO_GOLD), gold.min(crate::vault::GOLD_CAP));
        Ok(())
    }

    /// Step 1..3 of a gold deposit: `amount` leaves the loaded hero for the
    /// chest. Same three writes as an item, with the hero's gold **before**
    /// recorded in step 1 so that step 3 — and a resume — can tell whether
    /// step 2 landed; see `vault::gold_verdict`.
    pub fn vault_gold_deposit(&mut self, slot: u8, amount: u32) -> Result<(), String> {
        self.vault_gold_transfer(crate::vault::Op::Deposit, slot, amount)
    }

    /// Step 1..3 of a gold withdraw: `amount` leaves the chest for the loaded
    /// hero.
    pub fn vault_gold_withdraw(&mut self, slot: u8, amount: u32) -> Result<(), String> {
        self.vault_gold_transfer(crate::vault::Op::Withdraw, slot, amount)
    }

    fn vault_gold_transfer(
        &mut self,
        op: crate::vault::Op,
        slot: u8,
        amount: u32,
    ) -> Result<(), String> {
        use crate::vault::Op;
        if !vault_on() {
            return Err("the chest is switched off".into());
        }
        if let Some(why) = self.vault.broken() {
            return Err(why.to_string());
        }
        if self.vault.in_flight() {
            return Err("a move is already in flight".into());
        }
        let (hero, before) = self.hero_gold()?;
        // Every refusal is in here, before the file changes: the hero's side
        // and the chest's side, and both caps.
        self.vault.arm_gold(op, slot, amount, before)?; // 1
        if self.vault_stop_after == Some(1) {
            return Err("stopped after step 1".into());
        }
        let after = match op {
            Op::Deposit => before - amount,
            Op::Withdraw => before + amount,
        };
        self.mem.w32(hero.wrapping_add(HERO_GOLD), after); // 2
        self.save_slot(slot)?;
        if self.vault_stop_after == Some(2) {
            return Err("stopped after step 2".into());
        }
        // 3 — ask the hero, on the same evidence a resume would use.
        let (_, now) = self.hero_gold()?;
        let what = self.vault.settle_gold(now)?;
        log::info!("vault: gold {op:?} of {amount} for slot {slot} -> {what:?}");
        Ok(())
    }

    /// The chest's gold and the two words that move gold, along the top edge
    /// beside the 소지금 the shipped grid prints at the top right — clear of
    /// the back button's box (`w - 0x30..`, `0..0x36`) and of the first row of
    /// cells (`h/2 - 0x4b` down); `the_gold_words_and_targets_clear_the_grids_own`
    /// pins both against the guest's own rectangles.
    fn draw_vault_gold(&mut self) -> Result<(), String> {
        let Some(rgb) = self.symbol("MC_grpGetPixelFromRGB") else { return Ok(()) };
        let lit = self.call(rgb, &[VAULT_LIT.0, VAULT_LIT.1, VAULT_LIT.2])?;
        let mut line = GOLD_CHEST_LINE.to_vec();
        line.extend_from_slice(self.vault.gold().to_string().as_bytes());
        // The colours come from the two escapes in the line itself — the same
        // `o` and `&` the shipped 소지금 carries — and `-1` lets them stand.
        self.draw_line(GOLD_LINE_X, GOLD_LINE_Y, &line, HUD_C1)?;
        // The two words are buttons; while the keypad is up they are not, so
        // they are not shown either.
        if self.vault_gold_input.is_none() {
            self.draw_line(GOLD_PUT_X, GOLD_LINE_Y, GOLD_PUT, lit)?;
            self.draw_line(GOLD_TAKE_X, GOLD_LINE_Y, GOLD_TAKE, lit)?;
        }
        Ok(())
    }

    /// Which of the two gold words a touch landed on, if either — through the
    /// guest's own `Check_Rect`, like the panel words.
    fn vault_gold_hit(&mut self, x: i32, y: i32) -> Result<Option<crate::vault::Op>, String> {
        let Some(check) = self.symbol("Check_Rect") else { return Ok(None) };
        for (op, rx, w) in [
            (crate::vault::Op::Deposit, GOLD_PUT_HIT_X, GOLD_PUT_HIT_W),
            (crate::vault::Op::Withdraw, GOLD_TAKE_HIT_X, GOLD_TAKE_HIT_W),
        ] {
            let hit = self.call(
                check,
                &[x as u32, y as u32, rx as u32, GOLD_HIT_Y as u32, w as u32, GOLD_HIT_H as u32],
            )?;
            if hit != 0 {
                return Ok(Some(op));
            }
        }
        Ok(None)
    }

    /// Open the game's numeric keypad for an amount.
    ///
    /// `InputInit(0)` is the letter's recipient field: `MX_imeCall` with
    /// `onCloseIME_MainFrame`, the digits-only mask (property 0 = 8), a cap of
    /// eleven and `"010"` seeded. The cap becomes [`GOLD_DIGITS`] — the width
    /// of `vault::GOLD_CAP` — and the field is cleared the way `open_id_input`
    /// clears it: zeroed, not merely terminated, or the first digit typed would
    /// bring the `010` back (see [`IME_BUF`]).
    ///
    /// The close callback then belongs to the chest: `vault_gold_input` is
    /// what `host_id_input_done` looks at first. `←` on an empty field is the
    /// way out — the guest reports that as cancelled, and nothing moves.
    fn open_vault_gold_input(&mut self, op: crate::vault::Op) -> Result<(), String> {
        if let Some(why) = self.vault.broken() {
            log::warn!("vault: {why}");
            self.popup(VAULT_BROKEN)?;
            return Ok(());
        }
        if self.vault.in_flight() {
            self.popup(GOLD_BUSY)?;
            return Ok(());
        }
        let (Some(input_init), Some(set_prop)) =
            (self.symbol("InputInit"), self.symbol("MX_imeSetProperty"))
        else {
            return Ok(());
        };
        self.call(input_init, &[IME_MODE_LETTER])?;
        if let Some(ime) = self.symbol("_midas_internalImeInstance") {
            for i in 0..IME_BUF_LEN {
                self.mem.w8(ime + IME_BUF + i, 0);
            }
        }
        let buf = self.ime_buffer()?;
        self.mem.w8(buf, 0);
        self.call(set_prop, &[IME_PROP_TEXT, buf])?;
        self.call(set_prop, &[IME_PROP_MAXLEN, GOLD_DIGITS])?;
        self.vault_gold_input = Some(op);
        log::info!("vault: keypad open for a gold {op:?}");
        Ok(())
    }

    /// Move the amount the keypad just closed on, and say why not when it
    /// cannot. Every refusal is before step 1, so nothing has moved when the
    /// message is up — and the message names the side that is short, which
    /// `arm_gold`'s own refusal (the log line) does not need to.
    fn vault_gold_move(&mut self, op: crate::vault::Op, amount: u32) -> Result<(), String> {
        use crate::vault::{Op, GOLD_CAP};
        if amount == 0 {
            self.popup(GOLD_NONE)?;
            return Ok(());
        }
        let (_, hero) = self.hero_gold()?;
        let chest = self.vault.gold();
        let cap = u64::from(GOLD_CAP);
        let refusal = match op {
            Op::Deposit if hero < amount => Some(GOLD_SHORT_HERO),
            Op::Deposit if u64::from(chest) + u64::from(amount) > cap => Some(GOLD_CAP_PUT),
            Op::Withdraw if chest < amount => Some(GOLD_SHORT_CHEST),
            Op::Withdraw if u64::from(hero) + u64::from(amount) > cap => Some(GOLD_CAP_TAKE),
            _ => None,
        };
        if let Some(msg) = refusal {
            self.popup(msg)?;
            return Ok(());
        }
        let slot = self.vault_slot;
        let r = match op {
            Op::Deposit => self.vault_gold_deposit(slot, amount),
            Op::Withdraw => self.vault_gold_withdraw(slot, amount),
        };
        if let Err(why) = r {
            log::info!("vault: the gold move was refused: {why}");
            let msg = if self.vault.broken().is_some() { VAULT_BROKEN } else { GOLD_BUSY };
            self.popup(msg)?;
        }
        Ok(())
    }

    /// The middle of the logical screen, `_lcdG+0x4c`/`+0x50` halved — the
    /// origin every widget on the main menu is placed against.
    fn screen_middle(&mut self) -> Option<(i32, i32)> {
        let lcd = self.symbol("_lcdG")?;
        let (w, h) = (self.mem.r32(lcd + 0x4c) as i32, self.mem.r32(lcd + 0x50) as i32);
        (w > 0 && h > 0).then_some((w >> 1, h >> 1))
    }

    /// One line of outlined text through the game's own renderer, in a guest
    /// buffer the EUC-KR bytes are copied into untouched.
    fn draw_line(&mut self, x: i32, y: i32, text: &[u8], c1: u32) -> Result<(), String> {
        let (Some(draw), Some(lcd), Some(buf)) =
            (self.symbol("HF_DrawTextOutLine"), self.symbol("_lcdG"), self.potion_scratch())
        else {
            return Ok(());
        };
        let g = self.mem.r32(lcd + 4);
        if g == 0 || text.len() >= HUD_SCRATCH {
            return Ok(());
        }
        self.mem.write_bytes(buf, text);
        self.mem.w8(buf + text.len() as u32, 0);
        self.call(draw, &[g, x as u32, y as u32, buf, text.len() as u32, c1, HUD_C2, HUD_FLAGS])?;
        Ok(())
    }
}

impl Vm {
    // ---- 장비 탭의 외형 칸 ---------------------------------------------

    /// Draw the applied 외형 beside the gear, on the 장비 tab.
    ///
    /// An entry hook that runs the shipped body first and then adds to it, so
    /// the cells land on top of a finished screen and the tab itself is
    /// untouched — the same shape as `host_hud`. The one call site is checked
    /// by `LR`, and the state by `_GameMenuState`, so the hook cannot reach
    /// the tab's sub-screens, which put their own panels across this band.
    pub(crate) fn host_equip_skins(&mut self) -> Result<bool, String> {
        let pc = self.cpu.reg_get(Mode::User, reg::PC);
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        if self.symbol("GameMenuFrame_Render").map(|a| a.wrapping_add(EQUIP_DRAW_RET)) != Some(lr) {
            return Ok(false);
        }
        let args: Vec<u32> = (0..4).map(|i| self.cpu.reg_get(Mode::User, i)).collect();
        let r0 = self.call(pc, &args)?;
        let tab = self.symbol("_GameMenuState").map(|a| self.mem.r32(a));
        if tab == Some(EQUIP_TAB_STATE) {
            self.draw_equip_skins()?;
        }
        self.cpu.reg_set(Mode::User, 0, r0);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Five cells and two headings, all through the tab's own two draw calls.
    fn draw_equip_skins(&mut self) -> Result<(), String> {
        let (Some(round), Some(icon), Some(equip), Some(hero_of), Some(lcd)) = (
            self.symbol("UI_RoundRect"),
            self.symbol("drawEqip_ItemIcon"),
            self.symbol("MP_getEquip"),
            self.symbol("MP_getHero"),
            self.symbol("_lcdG"),
        ) else {
            return Ok(());
        };
        let Some((cx, cy)) = self.screen_middle() else { return Ok(()) };
        let g = self.mem.r32(lcd + 4);
        if g == 0 {
            return Ok(());
        }
        // `drawEqip_ItemIcon`'s second argument, which only its empty-cell arm
        // reads. Taken the way the shipped loop takes it.
        let hero = self.call(hero_of, &[0])?;
        let class = self.mem.r8(hero.wrapping_add(0xc6)) as i8 as i32 as u32;

        for &(slot, dx, row) in SKIN_CELLS.iter() {
            let x = cx + EQ_X0 + dx;
            let y = cy + EQ_Y0 + row * EQ_ROW;
            self.call(round, &[g, x as u32, y as u32, EQ_W as u32, EQ_H as u32, 0xff])?;
            let worn = self.call(equip, &[0, slot])?;
            let donor = if worn == 0 { None } else { self.skin_donor_kind(worn)? };
            // NULL here is not a failure: it is the guest's own dim
            // placeholder for this slot, which is exactly what "no 외형" is.
            let stand_in = match donor.map(|(kind, _)| kind) {
                Some(kind) => match self.skin_cell_scratch() {
                    Some(buf) => {
                        self.mem.w8(buf + ITEM_KIND, kind);
                        buf
                    }
                    None => 0,
                },
                None => 0,
            };
            let (ix, iy) = ((x + EQ_ICON_DX) as u32, (y + EQ_ICON_DY) as u32);
            self.call(icon, &[stand_in, class, slot, ix, iy])?;
        }
        self.draw_equip_caret(cx, cy, g)?;
        self.draw_equip_skin_labels(cx, cy)
    }

    /// 「외형」 over each group. Explicit colours, because `HF_DrawTextOutLine`
    /// reads `-1` as "leave the colour alone" and would inherit whatever the
    /// line before it left.
    fn draw_equip_skin_labels(&mut self, cx: i32, cy: i32) -> Result<(), String> {
        let Some(rgb) = self.symbol("MC_grpGetPixelFromRGB") else { return Ok(()) };
        let lit = self.call(rgb, &[VAULT_LIT.0, VAULT_LIT.1, VAULT_LIT.2])?;
        let y = cy + EQ_Y0 + SKIN_LABEL_DY;
        // One heading per group, over the group's own first column: the left
        // group is the cells outside column 0, the right group the ones
        // outside the last column.
        let cols = |left: bool| {
            SKIN_CELLS
                .iter()
                .map(|&(_, dx, _)| dx)
                .filter(|&dx| if left { dx < 0 } else { dx > EQ_COLS[3] })
                .min()
        };
        for dx in [cols(true), cols(false)].into_iter().flatten() {
            self.draw_line(cx + EQ_X0 + dx + SKIN_LABEL_DX, y, SKIN_CELLS_LABEL, lit)?;
        }
        Ok(())
    }

    /// The kind whose icon stands for this worn piece's 외형, or `None` when
    /// it has none.
    ///
    /// The lookup goes through the item templates and is cached on the way
    /// back, keyed by the look itself — the answer only changes when a player
    /// changes a piece, and this runs on every frame the tab is up.
    fn skin_donor_kind(&mut self, worn: u32) -> Result<Option<(u8, u8)>, String> {
        let Some((sprite, pal)) = self.skin_look(worn)? else { return Ok(None) };
        let kind = self.mem.r8(worn.wrapping_add(ITEM_KIND));
        let key = (kind, sprite, pal);
        if let Some(&(_, found)) = self.skin_kinds.iter().find(|&&(k, _)| k == key) {
            return Ok(Some(found));
        }
        let vfs = self.cfg.vfs.clone();
        let kinds = skin_donor_kinds(kind);
        // Falling back to the worn kind and index 0xff is right for a weapon
        // or a shield, which can only have taken their own kind's look; for
        // armour it still reports that a look is applied. 0xff is no index,
        // so the panel then shows nothing rather than the wrong piece.
        let found = crate::netitem::kind_with_look(&vfs, &self.vfs_index, &kinds, sprite, pal)
            .unwrap_or((kind, 0xff));
        self.skin_kinds.push((key, found));
        Ok(Some(found))
    }

    /// The look the cave recorded for this worn item, if any.
    fn skin_look(&mut self, worn: u32) -> Result<Option<(u8, i16)>, String> {
        let Some(tab) = self.symbol("skin_tab") else { return Ok(None) };
        self.skin_table_ready()?;
        let lo = self.mem.r32(worn.wrapping_add(ITEM_STAMP));
        let hi = skin_stamp_norm(lo, self.mem.r32(worn.wrapping_add(ITEM_STAMP + 4)));
        // `skin_slot()` in the cave: the loaded save slot, +1.
        let slot = match self.symbol("_selectSlotNumber") {
            Some(a) => (self.mem.r8(a) as i8).wrapping_add(1) as u8,
            None => return Ok(None),
        };
        for i in 0..SKIN_RECS {
            let r = tab + i * SKIN_REC;
            if self.mem.r32(r + SKIN_REC_MAGIC) != SKIN_MAGIC {
                continue;
            }
            let rslot = self.mem.r8(r + SKIN_REC_SLOT);
            if rslot != 0 && rslot != slot {
                continue;
            }
            let (rlo, rhi) = (self.mem.r32(r), self.mem.r32(r + 4));
            if rlo == lo && skin_stamp_norm(rlo, rhi) == hi {
                let sprite = self.mem.r8(r + SKIN_REC_SPRITE);
                let pal = self.mem.r16(r + SKIN_REC_PAL) as i16;
                return Ok(Some((sprite, pal)));
            }
        }
        Ok(None)
    }

    /// The cave reads its table off the save container lazily, on the first
    /// sprite load. Ask it to through its own entry rather than opening the
    /// container here — `skin_getEquip` returns early for an empty slot, so
    /// this walks the worn slots until one of them gets that far.
    fn skin_table_ready(&mut self) -> Result<(), String> {
        let (Some(flag), Some(get)) = (self.symbol("skin_loaded"), self.symbol("skin_getEquip"))
        else {
            return Ok(());
        };
        for &(slot, _, _) in SKIN_CELLS.iter() {
            if self.mem.r8(flag) != 0 {
                return Ok(());
            }
            self.call(get, &[0, slot])?;
        }
        Ok(())
    }

    // ---- 외형 칸의 포커스와 정보 패널 --------------------------------

    /// Which look cell the tab's cursor is on, if it is on one.
    ///
    /// `_IGM_EQKey` is the shipped cursor and 0..7 are still its gear cells;
    /// 8..12 exist only because [`EQ_STEP_SITES`] walks the cells that are
    /// drawn instead of the 4x2 grid the shipped arms step over.
    fn skin_cursor(&mut self) -> Option<usize> {
        let at = self.symbol("_IGM_EQKey")?;
        let i = self.mem.r8(at) as i8 as i32 - SKIN_CURSOR;
        (i >= 0 && (i as usize) < SKIN_CELLS.len()).then_some(i as usize)
    }

    /// `getEqipNum` for a look cell: there is no equipment slot behind one.
    ///
    /// -1 is not a slot, and [`Vm::host_equip_worn`] turns it into "nothing
    /// worn", which is the branch every consumer already has — so the OK key
    /// does nothing on a look cell without a single hook of its own.
    pub(crate) fn host_equip_slot_num(&mut self) -> Result<bool, String> {
        if (self.cpu.reg_get(Mode::User, 0) as i32) < SKIN_CURSOR {
            return Ok(false);
        }
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, 0, -1i32 as u32);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// `MP_getEquip` outside the eight slots is nothing worn.
    ///
    /// The shipped body is `hero + slot*4` with no range check, so a slot it
    /// was never meant to see reads some other field of the hero and hands it
    /// back as an item pointer. Nothing in the shipped game reaches that;
    /// widening the cursor does, through `getEqipNum` above.
    pub(crate) fn host_equip_worn(&mut self) -> Result<bool, String> {
        let slot = self.cpu.reg_get(Mode::User, 1) as i32;
        if (0..EQUIP_SLOTS).contains(&slot) {
            return Ok(false);
        }
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, 0, 0);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// The equipment the tab's info panel is about — the look's donor while a
    /// look cell is selected.
    ///
    /// Three instructions read it and all three are answered, so the popup,
    /// the name, the stats and the line count are the shipped ones drawn
    /// about a different item. Declines on a gear cell, which is every frame
    /// the player is not on one of ours.
    pub(crate) fn host_equip_panel_item(&mut self) -> Result<bool, String> {
        let Some(i) = self.skin_cursor() else { return Ok(false) };
        let item = self.skin_panel_item(i)?;
        let pc = self.cpu.reg_get(Mode::User, reg::PC);
        let rd = ((self.mem.r32(pc) >> 12) & 0xf) as u8;
        self.cpu.reg_set(Mode::User, rd, item);
        self.cpu.reg_set(Mode::User, reg::PC, pc.wrapping_add(4));
        Ok(true)
    }

    /// The donor as a whole item, for the info panel.
    ///
    /// `ITEM_Load(kind, index, item)` is the shipped loader filling a buffer
    /// of ours — no pool slot is taken, so nothing has to be given back, and
    /// the panel reads a piece the game built. Held until the look changes,
    /// because this runs three times a frame while the cell is selected.
    fn skin_panel_item(&mut self, i: usize) -> Result<u32, String> {
        let (slot, _, _) = SKIN_CELLS[i];
        let Some(equip) = self.symbol("MP_getEquip") else { return Ok(0) };
        let worn = self.call(equip, &[0, slot])?;
        if worn == 0 {
            return Ok(0);
        }
        let Some((kind, idx)) = self.skin_donor_kind(worn)? else { return Ok(0) };
        if idx == 0xff {
            return Ok(0);
        }
        let (Some(load), Some(name), Some(buf)) =
            (self.symbol("ITEM_Load"), self.symbol("ITEM_SetItemName"), self.skin_item_scratch())
        else {
            return Ok(0);
        };
        if self.skin_item_of != Some((kind, idx)) {
            // `ITEM_Load` fills the template's fields but not the name — the
            // shipped constructor calls `ITEM_SetItemName` after it, and
            // without that the panel's title is whatever the buffer held.
            self.mem.write_bytes(buf, &vec![0u8; ITEM_SIZE as usize]);
            self.call(load, &[kind as u32, idx as u32, buf])?;
            self.call(name, &[buf])?;
            self.skin_item_of = Some((kind, idx));
        }
        Ok(buf)
    }

    /// Hit-test the look cells, at the head of the block that hit-tests the
    /// gear ones.
    ///
    /// Declines either way: the shipped block then runs as it always did and
    /// finds nothing, because no look cell overlaps a gear cell. That is the
    /// whole reason this can sit in front of it rather than replace it.
    pub(crate) fn host_equip_touch(&mut self) -> Result<bool, String> {
        let (Some((cx, cy)), Some(at)) = (self.screen_middle(), self.symbol("_IGM_EQKey")) else {
            return Ok(false);
        };
        let (x, y) = (self.cpu.reg_get(Mode::User, 7) as i32, self.cpu.reg_get(Mode::User, 8) as i32);
        for (i, &(_, dx, row)) in SKIN_CELLS.iter().enumerate() {
            let cell_x = cx + EQ_X0 + dx;
            let cell_y = cy + EQ_Y0 + row * EQ_ROW;
            if (cell_x..cell_x + EQ_W).contains(&x) && (cell_y..cell_y + EQ_H).contains(&y) {
                self.mem.w8(at, (SKIN_CURSOR + i as i32) as u8);
                break;
            }
        }
        Ok(false)
    }

    /// One of the tab's four arrows.
    ///
    /// The hook is on the `ldrb` that reads the cursor and it returns past the
    /// arm's own arithmetic, so only the step is ours: the guard the arm has
    /// already passed and the bytes it resets on the way out are the guest's,
    /// and so is every key and touch that got here. See [`EQ_STEP_SITES`].
    ///
    /// Declines on a cursor value no cell owns. The shipped ±1/±4 then runs and
    /// brings it back inside 0..7, which is better than a cursor that cannot be
    /// moved at all.
    pub(crate) fn host_equip_step(&mut self) -> Result<bool, String> {
        let pc = self.cpu.reg_get(Mode::User, reg::PC);
        let (Some(key), Some(at)) =
            (self.symbol("GameMenuFrame_KeyPressed"), self.symbol("_IGM_EQKey"))
        else {
            return Ok(false);
        };
        let Some(&(_, resume, along, between)) =
            EQ_STEP_SITES.iter().find(|&&(off, ..)| key.wrapping_add(off) == pc)
        else {
            return Ok(false);
        };
        let cells = equip_cells();
        let cur = self.mem.r8(at) as i8 as i32;
        if !cells.iter().any(|&(c, _, _)| c == cur) {
            return Ok(false);
        }
        if let Some(next) = equip_step(&cells, cur, along, between) {
            self.mem.w8(at, next as u8);
        }
        self.cpu.reg_set(Mode::User, reg::PC, key.wrapping_add(resume));
        Ok(true)
    }

    /// OK on a look cell does nothing.
    ///
    /// Nothing is equipped behind one, and the shipped arm reads that as an
    /// empty gear slot and opens the picker for it — with the slot number
    /// `getEqipNum` could not give, which is the one way a look cell could
    /// reach code that changes the player's equipment. Sent instead to the
    /// continuation the sibling arms use when they find nothing.
    pub(crate) fn host_equip_ok_empty(&mut self) -> Result<bool, String> {
        if self.skin_cursor().is_none() {
            return Ok(false);
        }
        let Some(key) = self.symbol("GameMenuFrame_KeyPressed") else { return Ok(false) };
        self.cpu.reg_set(Mode::User, reg::PC, key.wrapping_add(EQ_OK_NOTHING));
        Ok(true)
    }

    /// The caret, on top of the cells, for whichever cell the cursor is on.
    ///
    /// Two things need it there. The tab's own caret loop compares the cursor
    /// against 0..7, so for a look cell it draws none. And for a gear cell in
    /// the **first column** it draws one inside the look cell two columns to
    /// its left — the caret sits `EQ_CARET_DX` = -0x12 from its cell against a
    /// column pitch of 0x1d — which the cells, drawn after the tab, then paint
    /// over. That is 무기 장착 and 장신구 장착, the two the finger vanished on.
    ///
    /// Drawing it again is not a second caret: the same frame at the same place
    /// at scale 100 with alpha 0xff writes the same pixels. Everything about
    /// the call is the tab's own, read off `IGM_DrawEquip+0x894`.
    fn draw_equip_caret(&mut self, cx: i32, cy: i32, g: u32) -> Result<(), String> {
        let (Some(get), Some(draw), Some(sheet), Some(at)) = (
            self.symbol("getImage"),
            self.symbol("DM_DrawImgRotateEx"),
            self.symbol("_imgUI_IGM"),
            self.symbol("_IGM_EQKey"),
        ) else {
            return Ok(());
        };
        let cur = self.mem.r8(at) as i8 as i32;
        let Some(&(_, dx, row)) = equip_cells().iter().find(|&&(c, _, _)| c == cur) else {
            return Ok(());
        };
        let sheet = self.mem.r32(sheet);
        let img = self.call(get, &[sheet, EQ_CARET_FRAME])?;
        if img == 0 {
            return Ok(());
        }
        let x = cx + EQ_X0 + dx + EQ_CARET_DX;
        let y = cy + EQ_Y0 + row * EQ_ROW + EQ_CARET_DY;
        let (w, h) = (self.mem.r32(img + 8), self.mem.r32(img + 4));
        self.call(
            draw,
            &[g, x as u32, y as u32, w, h, img, 0, 0, 0, 0, 100, 100, 0, 0xff, 9],
        )?;
        Ok(())
    }

    /// A guest item the shipped loader fills, for the info panel.
    fn skin_item_scratch(&mut self) -> Option<u32> {
        if self.skin_item == 0 {
            self.skin_item = self.heap.alloc(ITEM_SIZE)?;
        }
        Some(self.skin_item)
    }

    /// A guest item just large enough for the one byte `drawEqip_ItemIcon`
    /// reads. Nothing else ever sees it.
    fn skin_cell_scratch(&mut self) -> Option<u32> {
        if self.skin_buf == 0 {
            self.skin_buf = self.heap.alloc(ITEM_KIND + 4)?;
        }
        Some(self.skin_buf)
    }
}

/// What one key event did on the grid: the shipped `ItemBagKey` answer, and
/// the cell it picked if it picked one.
type Picked = (u32, Option<(i32, i32, u32)>);

/// `_menuState` for the letter frame, and the two sub-states the chest borrows.
///
/// `MainMenuFrame_Render`'s table sends 2 to `MainMenuFrame_drawLetter`, which
/// dispatches on `_menuDepthState` (0 = the slot list, 2 = the inner screens)
/// and then on `_mLetterState` (4 = the bag grid).
const LETTER_FRAME: u32 = 2;
const LETTER_DEPTH_INNER: u8 = 2;
const LETTER_STATE_BAG: u8 = 4;

/// Where `Item_saveStorage` reads each field of a live item and where
/// `Item_loadStorage` writes it back: `(record offset, item offset, bytes)`.
///
/// Halfwords and the stamp are little-endian — `UM_Int16ToByte` and
/// `UM_Int64ToByte` store low byte first.
///
/// Record offset 3 is **not here**: it is the tab the entry was found in,
/// which belongs to the bag and not to the item, so [`Vm::item_record`] fills
/// it separately. Offsets 0..2 are `ITEM_CreateItem`'s three arguments, which
/// is why [`Vm::bag_put`] skips them when restoring — the constructor has
/// already put those values where they go.
pub(crate) const ITEM_FIELDS: &[(usize, u32, usize)] = &[
    (0, 0x96, 1),
    (1, 0x00, 1),
    (2, 0x97, 1),
    (4, 0x95, 1),
    (5, 0x98, 1),
    (6, 0x0d, 1),
    (7, 0x20, 2),
    (9, 0x22, 2),
    (11, 0x24, 2),
    (13, 0x0e, 1),
    (14, 0x26, 2),
    (16, 0x28, 2),
    (18, 0x2a, 2),
    (20, 0x32, 1),
    (21, 0x2c, 2),
    (23, 0x2e, 2),
    (25, 0x30, 2),
    (27, 0xd8, 1),
    (28, 0xd0, 8),
];

/// `item + 0xd0` — the 64-bit stamp `ITEM_CreateItem` mints and
/// `Item_loadStorage` matches equipment against.
pub(crate) const ITEM_STAMP: u32 = 0xd0;

/// `item + 0x96` — the kind. `drawEqip_ItemIcon` reads this one byte and
/// nothing else off an item, which is why a look cell can be drawn by the
/// guest's own icon code with a scratch buffer standing in for the donor.
pub(crate) const ITEM_KIND: u32 = 0x96;

/// How much of an item the game itself allocates (`cave/src/guest.h`), which
/// is how much `ITEM_Load` may fill.
const ITEM_SIZE: u32 = 0xe0;

// ---- 장비 탭의 외형 칸 — `CS_SKIN_CELLS` --------------------------------
//
// The 외형 (transmog) a piece is wearing lives in the cave's record table and
// shows up on the hero's sprite, and nowhere else: the equipment tab still
// draws the *item* in every cell, so a player who has changed six pieces has
// no way to see what they changed them to. These five cells say it.
//
// Nothing new is drawn. A cell is the shipped pair — `UI_RoundRect` for the
// frame and `drawEqip_ItemIcon` for the icon — issued with the same geometry
// the tab's own loop uses, and the icon code is handed either a scratch item
// carrying the donor's kind or NULL, which draws that slot's dim placeholder.
// So an empty look cell looks exactly like an empty gear cell, because it is
// the same code.
//
// The donor's *kind* has to be recovered, because the record keeps only what
// the sprite loader needs (`item[1]` and `item[2..3]`) while the icon is
// chosen by kind. `netitem::kind_with_look` finds it in the game's own item
// templates — see there for why that works.
//
// **What the weapon and shield cells can and cannot say.** The icon is picked
// by kind alone, and the transmog rule (`cave/src/skin.c::kinds_compatible`)
// makes a weapon or shield take only its own kind's look. Those two cells
// therefore always carry the same icon as the gear cell beside them; what
// they report is that a look *is* applied. The three armour cells do better —
// a piece may take any of the three kinds for its body part, and those have
// different icons.

/// `_GameMenuState` for the 장비 tab. `GameMenuFrame_Render`'s table sends 2
/// to `IGM_DrawEquip`; 0x10..0x14 reach it too, but those are its sub-screens
/// and they put their own panels across this band.
const EQUIP_TAB_STATE: u32 = 2;

/// The return address of the one `bl IGM_DrawEquip` in the image
/// (`GameMenuFrame_Render+0x604`), so nothing else can reach the hook.
const EQUIP_DRAW_RET: u32 = 0x608;

/// The cell grid, read out of `IGM_DrawEquip`'s own loop (`+0x58..+0x298`):
/// column 0 at `W/2 - 0x47`, row 0 at `H/2 - 0x59`, a cell `0x1c x 0x1a` with
/// its icon inset `(+7, +3)`, columns `0x1d` apart and rows `0x1e` apart.
const EQ_X0: i32 = -0x47;
const EQ_Y0: i32 = -0x59;
const EQ_W: i32 = 0x1c;
const EQ_H: i32 = 0x1a;
const EQ_ICON_DX: i32 = 7;
const EQ_ICON_DY: i32 = 3;
#[cfg(test)]
const EQ_COL: i32 = 0x1d;
const EQ_ROW: i32 = 0x1e;

/// The four shipped columns' offsets from column 0 — `r8 + r5` on the loop's
/// four passes. The `0x37` step in the middle is the gap the character
/// sprite is drawn in.
const EQ_COLS: [i32; 4] = [0, 0x1d, 0x54, 0x71];

/// The eight cells' slots, `getEqipNum`'s table: the gear reads
/// `무기 투구 | 갑옷 방패` across the top and `· 신발 | 장갑 ·` under it, so
/// the occupied cells make a ㄱ on the left and its mirror on the right.
#[cfg(test)]
const EQ_SLOTS: [u32; 8] = [0, 2, 3, 1, 6, 4, 5, 7];

/// The look cells, `(slot, x offset from column 0, row)`.
///
/// Each sits **two columns outward** from the gear cell it belongs to, so the
/// left three repeat the gear's ㄱ (무기·투구 across, 신발 under 투구) and the
/// right two repeat the mirror of it. 장갑 is not here: the transmog only
/// reaches the five slots that have a sprite layer, so its cell could never
/// be anything but empty, and an empty cell says the opposite of the truth.
const SKIN_CELLS: [(u32, i32, i32); 5] =
    [(0, -0x3a, 0), (2, -0x1d, 0), (4, -0x1d, 1), (3, 0x8e, 0), (1, 0xab, 0)];

/// 「외형」, EUC-KR. Drawn once over each group, because two more columns of
/// the same cells in the same rows would otherwise read as more gear.
const SKIN_CELLS_LABEL: &[u8] = b"\xbf\xdc\xc7\xfc";

/// Where that word goes, relative to the group's first column and row 0.
const SKIN_LABEL_DX: i32 = 2;
const SKIN_LABEL_DY: i32 = -11;

/// `cave/src/skin.c`'s table: 256 records of `{u32 lo, u32 hi, u8 sprite,
/// u8 slot, s16 pal, u32 magic}`, keyed by the worn item's stamp **and** the
/// save slot that owns it — one table serves every slot, and an item's stamp
/// is a creation timestamp the game does not keep unique. A record whose slot
/// byte is 0 predates that key and matches any slot.
const SKIN_REC: u32 = 16;
const SKIN_RECS: u32 = 256;
const SKIN_MAGIC: u32 = 0x314e_4b53; // "SKN1"
const SKIN_REC_SPRITE: u32 = 8;
const SKIN_REC_SLOT: u32 = 9;
const SKIN_REC_PAL: u32 = 10;
const SKIN_REC_MAGIC: u32 = 12;

/// The eight equipment slots. `MP_getEquip` is `hero + slot*4` with no range
/// check of its own, so this is also what makes an out-of-range slot answerable.
const EQUIP_SLOTS: i32 = 8;

/// The first cursor value that means a look cell. The tab's cursor
/// (`_IGM_EQKey`) is the shipped one and 0..7 are still its gear cells, so the
/// five look cells are 8..12 — one cursor, not a second one.
const SKIN_CURSOR: i32 = 8;

/// The four arrow arms that step that cursor, hooked at the `ldrb` that reads
/// it: `(that offset, where the arm goes on, along the row, between the rows)`.
///
/// The shipped arms are ±1 over 0..7 with a wrap at each end, and ±4 with no
/// wrap — which is exactly walking a **4x2** grid. With the look cells on
/// screen the grid is eight columns on the top row and five on the bottom, so
/// ±1 crosses from 투구외형 to 신발외형 and ±4 lands on whatever the
/// arithmetic reaches. [`equip_step`] walks what is drawn instead.
///
/// Returning past the arm's own arithmetic skips only the step: each arm goes
/// on to a block that loads every register it uses from the GOT, and the next
/// conditional instruction after that is a `cmp` of its own, so neither a flag
/// nor a register crosses the hook. Everything else the arm does stays the
/// guest's — the `_gCell_OK` guard it has already passed on the way in, and
/// the two bytes it resets on the way out.
pub const EQ_STEP_SITES: [(u32, u32, i32, i32); 4] = [
    (0x1998, 0x19ac, 0, -1), // ↑
    (0x19f0, 0x1a04, 0, 1),  // ↓
    (0x1a48, 0x1a64, -1, 0), // ←
    (0x1aac, 0x1ac8, 1, 0),  // →
];

/// Every word of those four arms: the `ldrb` the hook sits on, the arithmetic
/// it answers instead of, and the instruction it returns to.
///
/// The arithmetic is checked although it is never executed again, because it
/// is what says these four arms are that 4x2 walk and not something else that
/// happens to read the same byte. The return words are checked because landing
/// one instruction off would either step the cursor a second time or skip the
/// tab's own bookkeeping.
const EQ_STEP_WORDS: [(u32, u32); 18] = [
    (0x1998, 0xe5d3_2000), // ldrb  r2,[r3]          ↑, the hook
    (0x199c, 0xe1a0_1c02), // lsl   r1,r2,#0x18
    (0x19a0, 0xe351_0403), // cmp   r1,#3<<24        the bottom row only
    (0x19a4, 0xc242_2004), // subgt r2,r2,#4
    (0x19ac, 0xe51f_3dd8), // ldr   r3,[pc,#-0xdd8]  where it goes on
    (0x19f0, 0xe5d3_2000), // ldrb  r2,[r3]          ↓
    (0x19f4, 0xe1a0_1c02), // lsl   r1,r2,#0x18
    (0x19f8, 0xe351_0403), // cmp   r1,#3<<24        the top row only
    (0x19fc, 0xd282_2004), // addle r2,r2,#4
    (0x1a04, 0xe51f_3e30), // ldr   r3,[pc,#-0xe30]
    (0x1a48, 0xe5d3_2000), // ldrb  r2,[r3]          ←
    (0x1a4c, 0xe242_2001), // sub   r2,r2,#1
    (0x1a5c, 0x13a0_2007), // movne r2,#7            wraps at the eighth cell
    (0x1a64, 0xe51f_3e90), // ldr   r3,[pc,#-0xe90]
    (0x1aac, 0xe5d2_1000), // ldrb  r1,[r2]          →
    (0x1ab0, 0xe281_1001), // add   r1,r1,#1
    (0x1abc, 0xe350_0407), // cmp   r0,#7<<24        and past the eighth
    (0x1ac8, 0xe51f_3ef4), // ldr   r3,[pc,#-0xef4]
];

/// `ldr Rd,[Rn,#0x65c]` — the three reads of "the equipment the cursor points
/// at" inside `IGM_DrawEquip`: the gate that decides whether a panel is drawn
/// at all, `drawItemInfo`'s argument, and `getItemInfo_lineCnt`'s. Answering
/// them with the look's donor is the whole of the info panel: the popup, the
/// name, the stats and the scroll are then the shipped ones.
pub const EQ_PANEL_ITEM: [u32; 3] = [0xc38, 0xca8, 0xcd4];
const EQ_PANEL_WORDS: [u32; 3] = [0xe593_365c, 0xe593_065c, 0xe593_065c];

/// The head of the block that hit-tests the eight cells
/// (`GameMenuFrame_KeyPressed+0x4fa4`), where `r7`/`r8` still hold the tap.
/// The look cells are tested here and the block then runs as shipped — it
/// finds nothing, because no look cell overlaps a gear one.
pub const EQ_TOUCH: u32 = 0x4fa4;
pub const EQ_OK_EMPTY_AT: u32 = EQ_OK_EMPTY;
const EQ_TOUCH_WORDS: [(u32, u32); 4] = [
    (0x4fa4, 0xe51f_958c), // ldr sb,[pc,#-0x58c]   the block head
    (0x4fc8, 0xe1a0_a007), // mov sl,r7             x, into Check_Rect's r0
    (0x5020, 0xe1a0_000a), // mov r0,sl
    (0x502c, 0xe1a0_1008), // mov r1,r8             y
];

/// Where the OK arm decides an empty slot means "open the picker for it"
/// (`GameMenuFrame_KeyPressed+0x3e8c`, the `cmp r0,#0` after `MP_getEquip`),
/// and the continuation its sibling arms use when nothing is there.
///
/// This is the one place a look cell could do harm: with no equipment behind
/// it the arm would take the empty-slot branch and hand slot -1 to
/// `serch_EquipItem`. The other two arms that ask the same question bail to
/// `+0x1970` instead, so that is where this one is sent.
const EQ_OK_EMPTY: u32 = 0x3e8c;
const EQ_OK_EMPTY_WORDS: [(u32, u32); 3] = [
    (0x3e88, 0xeb01_d73e), // bl MP_getEquip
    (0x3e8c, 0xe350_0000), // cmp r0,#0
    (0x3e90, 0x0a00_10f8), // beq +0x8278, the picker
];
const EQ_OK_NOTHING: u32 = 0x1970;

/// The caret the tab draws beside the selected cell — frame 0xe of
/// `_imgUI_IGM`, drawn from `IGM_DrawEquip+0x894` at the cell's left. Its
/// own origin is `W/2-0x59`, `H/2-0x58` against the cell's `-0x47`, `-0x59`.
const EQ_CARET_FRAME: u32 = 0xe;
const EQ_CARET_DX: i32 = -0x12;
const EQ_CARET_DY: i32 = 1;

/// Words `host_equip_skins` reads the grid out of, so a build whose equipment
/// tab has moved draws nothing instead of drawing five cells somewhere else.
/// The two `bl` are checked by target, which is the part a word cannot say.
const EQUIP_GRID_WORDS: [(u32, u32); 10] = [
    (0x68, 0xe241_1047),  // sub r1, r1, #0x47   column 0
    (0x78, 0xe242_2059),  // sub r2, r2, #0x59   row 0
    (0x7c, 0xe3a0_c01a),  // mov ip, #0x1a       cell h
    (0x88, 0xe3a0_301c),  // mov r3, #0x1c       cell w
    (0xdc, 0xe243_3040),  // sub r3, r3, #0x40   icon x
    (0xec, 0xe24c_c056),  // sub ip, ip, #0x56   icon y
    (0x118, 0xe356_0004), // cmp r6, #4          four columns
    (0x130, 0xe285_501d), // add r5, r5, #0x1d   column pitch
    (0x28c, 0xe28b_b01e), // add fp, fp, #0x1e   row pitch
    (0x294, 0xe35e_0008), // cmp lr, #8          eight cells
];
const EQUIP_ROUND_RECT: u32 = 0x9c;
const EQUIP_ITEM_ICON: u32 = 0x108;

/// 장비 탭의 외형 칸. Rides the cave's `skin` feature — with no transmog
/// there is nothing to report, so `0` is the shipped screen exactly.
pub fn skin_cells() -> bool {
    knob("CS_SKIN_CELLS", 1) != 0
}

/// The high word of an item's stamp as it looks once it has been through a
/// save — `cave/src/skin.c::stamp_norm`, which the look table is keyed on.
///
/// `UM_ByteToInt64` (0xeef24) ends with `orr sb, sb, r3, asr #31`: it ORs the
/// *low* word's sign into the high word, so half of all stamps come back off
/// `Item_loadStorage` with a high word of -1. Both sides of the comparison go
/// through this, and it is idempotent, so a record written either side of a
/// reload is found.
fn skin_stamp_norm(lo: u32, hi: u32) -> u32 {
    if lo >> 31 != 0 {
        u32::MAX
    } else {
        hi
    }
}

/// The kinds a piece of this kind may have taken its look from —
/// `cave/src/skin.c::kinds_compatible` read the other way round. Armour
/// (7..0x12) is three kinds per body part; weapons and shields only their own.
///
/// **The worn kind comes first**, because a look is not always unique inside
/// a group: all three kinds in each armour group carry `(sprite 1, palette
/// -1)` at index 0 — the class-neutral starting piece — so that one look
/// cannot say which kind it came from. Searching the worn kind first makes
/// the ambiguous case report the piece's own kind, which is the answer that
/// claims the least. Every other look in the shipped archive is unique to a
/// kind, and for a weapon or a shield the group is the worn kind alone.
fn skin_donor_kinds(worn: u8) -> Vec<u8> {
    if worn >= 0x13 {
        return Vec::new();
    }
    if worn < 7 {
        return vec![worn];
    }
    let first = 7 + 3 * ((worn - 7) / 3);
    std::iter::once(worn).chain((first..first + 3).filter(|&k| k != worn)).collect()
}

/// True when `IGM_DrawEquip` still has the grid the look cells are placed
/// against, and its two draw calls still go where they did.
pub fn equip_grid_site(draw: u32, round: u32, icon: u32, mem: &mut GuestMem) -> bool {
    EQUIP_GRID_WORDS.iter().all(|&(off, w)| mem.r32(draw.wrapping_add(off)) == w)
        && branch_target(draw.wrapping_add(EQUIP_ROUND_RECT), mem, true) == Some(round)
        && branch_target(draw.wrapping_add(EQUIP_ITEM_ICON), mem, true) == Some(icon)
        && EQ_PANEL_ITEM
            .iter()
            .zip(EQ_PANEL_WORDS)
            .all(|(&off, w)| mem.r32(draw.wrapping_add(off)) == w)
}

/// True when the four arrow arms are still the 4x2 walk the look cells replace,
/// the touch block still holds the tap in the registers they are tested
/// against, and the OK arm still decides an empty slot the way it did.
///
/// All of it or none: a cursor that can reach a value nothing answers for is
/// worse than no look cells at all, and an arrow whose step is neither the
/// guest's nor ours would leave the cursor where it was.
pub fn equip_cursor_site(key: u32, mem: &mut GuestMem) -> bool {
    EQ_STEP_WORDS.iter().all(|&(off, w)| mem.r32(key.wrapping_add(off)) == w)
        && EQ_TOUCH_WORDS.iter().all(|&(off, w)| mem.r32(key.wrapping_add(off)) == w)
        && EQ_OK_EMPTY_WORDS.iter().all(|&(off, w)| mem.r32(key.wrapping_add(off)) == w)
}

/// Every cell on the tab, as `(cursor, x offset from column 0, row)`.
///
/// The gear's eight come out of its own loop: cell `i` is column `i % 4` of row
/// `i / 4`, and the cursor *is* that cell's number. The look cells follow from
/// [`SKIN_CURSOR`] in [`SKIN_CELLS`] order. Nothing is written down twice, so a
/// cell that moves moves in the drawing, the touch and the arrows together.
fn equip_cells() -> Vec<(i32, i32, i32)> {
    (0..EQUIP_SLOTS)
        .map(|i| (i, EQ_COLS[(i % 4) as usize], i / 4))
        .chain(
            SKIN_CELLS
                .iter()
                .enumerate()
                .map(|(i, &(_, dx, row))| (SKIN_CURSOR + i as i32, dx, row)),
        )
        .collect()
}

/// Where an arrow takes the cursor, or `None` for nowhere.
///
/// `along` walks the row the cursor is in, in the order its cells are drawn
/// across the screen, and wraps inside it — 방패외형 goes back to 무기외형
/// rather than down into a row that starts three columns further in.
///
/// `between` goes to the other row and lands on the cell nearest the column
/// left behind; with no other row it gives nothing, which is what the shipped
/// ↑ on the top row and ↓ on the bottom one did.
///
/// For the eight gear cells `between` is the shipped ±4 exactly — each of their
/// four columns exists in both rows, so "nearest" is the same column. `along`
/// is where they differ, and have to: ±1 over 0..7 is the reading order of a
/// 4x2 grid, and the grid on screen is 8 columns by 5.
fn equip_step(cells: &[(i32, i32, i32)], cur: i32, along: i32, between: i32) -> Option<i32> {
    let &(_, x, row) = cells.iter().find(|&&(c, _, _)| c == cur)?;
    if between != 0 {
        return cells
            .iter()
            .filter(|&&(_, _, r)| r == row + between)
            .min_by_key(|&&(_, cx, _)| ((cx - x).abs(), cx))
            .map(|&(c, _, _)| c);
    }
    let mut across: Vec<(i32, i32)> =
        cells.iter().filter(|&&(_, _, r)| r == row).map(|&(c, cx, _)| (cx, c)).collect();
    across.sort_unstable();
    let i = across.iter().position(|&(_, c)| c == cur)? as i32;
    let n = across.len() as i32;
    Some(across[(i + along).rem_euclid(n) as usize].1)
}

/// 공용창고 on. `0` takes every transfer out of reach and leaves a chest file
/// that already exists untouched, which is what makes it a real A/B switch
/// rather than a way to lose the contents.
pub fn vault_on() -> bool {
    knob("CS_VAULT", 1) != 0
}

/// Entries per bag tab. `ITEMSTORAGE_addItem` refuses above `0x13`, so twenty.
pub(crate) const BAG_TAB_SLOTS: i32 = 0x14;

/// The selected-slot byte inside the storage object: `4 + 12 * BAG_MAX`, with
/// `BAG_MAX` five as shipped and ten with the cave's `bag10`. Everything that
/// reads a bag cursor uses one of these two — the cave rewrites 112 immediates
/// from the first to the second.
pub(crate) const BAG_CURSOR: u32 = 0x40;
pub(crate) const BAG_CURSOR_CAVE: u32 = 0x7c;

// ---- 공용창고의 화면 -------------------------------------------------------
//
// The protocol half of the chest has been here since 2026-09-21; this is the
// screen that reaches it. **It draws almost nothing.** The letter frame already
// owns both widgets the chest needs, and they are generic:
//
//   `_menuState == 2`, `_menuDepthState == 0`
//        -> `MainMenuFrame_drawLetter+0x284` -> `MainMenuFrame_drawMenuSlot`,
//           the same three-slot list 캐릭관리 and 시작 use
//   `_menuState == 2`, `_menuDepthState == 2`, `_mLetterState == 4`
//        -> `MainMenuFrame_drawLetter+0x558` -> `draw_CommonItembag`
//           -> `drawItemBag(0, -1, 1, 0)`, and its keys tail-branch from
//           `MainMenuFrame_KeyPressed+0x135c` into
//           `MainMenuFrame_ItemBagKeyPressed`
//
// So the six hooks below only *redirect*: the entry tile, the slot list's OK,
// the grid's keys and the grid's draw, plus two labels. Every pixel is the
// guest's.
//
// The chest side of the grid is the same widget pointed at a different list.
// `drawItemBag`, `ItemBagKey` and everything under them reach the bag through
// one address — the `_storage` object — and that struct is **entirely
// self-contained**: a tab byte, a tab count, one 12-byte `LinkedList` header
// per tab and a cursor byte. So the chest is shown by swapping 0x44 (or the
// cave's 0x80) bytes of image into that address for exactly as long as one
// shipped call runs, and swapping the bag's back afterwards
// ([`Vm::with_chest`]). No GOT is touched, so `Item_saveStorage` — which reads
// the same object and runs during step 2 of every transfer — can never see the
// chest.
//
// `CS_VAULT=0` installs none of this and leaves tile 1 grey.

/// Main-menu tile 1 — 패치, whose feature does not exist in this build. Its
/// neighbour tile 7 became 「내 ID」 the same way; the grey is computed for both
/// by one instruction, and [`Vm::host_my_id_tile`] now answers for either.
const VAULT_TILE: u32 = 1;

/// Tile 1's label id in the `getInCodeString` table (`0x1c2 + tile`), and what
/// to say instead of 패치.
///
/// `공용창고` in EUC-KR. Four syllables, the same width as 캐릭관리 and
/// 환경설정 two rows down, so the tile needs no new layout.
const VAULT_LABEL_ID: u32 = 0x1c3;
const VAULT_LABEL: &[u8] = &[0xB0, 0xF8, 0xBF, 0xEB, 0xC3, 0xA2, 0xB0, 0xED];

/// The bag grid's heading, `보낼 아이템 선택` — the letter's wording, because in
/// the letter that is what the grid is for.
///
/// The chest replaces it with **two** words, one per panel, so the screen says
/// what it is showing and what the other half is at the same time. Swapping the
/// id to an empty string is how the shipped heading gets out of the way:
/// `draw_CommonItembag` runs `MC_knlSprintk(buf, s)` and then draws `buf`, so an
/// empty format draws nothing and no shipped instruction is skipped.
const VAULT_TITLE_ID: u32 = 0x1f0;

/// `가방` / `공용창고` — the two halves of the title band.
const VAULT_BAG: &[u8] = &[0xB0, 0xA1, 0xB9, 0xE6];
const VAULT_CHEST: &[u8] = &[0xB0, 0xF8, 0xBF, 0xEB, 0xC3, 0xA2, 0xB0, 0xED];

/// What the screen says when a transfer is refused. All of them are refusals
/// **before** anything moved — the chest either had no room, or the bag had
/// none, or the item is being worn — so none of them describes a half-done
/// move. `;` is the renderer's line break and none contains `%`, which
/// [`Vm::popup`] requires (`setPopUp` passes its argument as a format).
///
/// `창고가 가득 찼습니다`
const VAULT_FULL: &[u8] = &[
    0xC3, 0xA2, 0xB0, 0xED, 0xB0, 0xA1, 0x20, 0xB0, 0xA1, 0xB5, 0xE6, 0x20, 0xC3, 0xA1, 0xBD, 0xC0,
    0xB4, 0xCF, 0xB4, 0xD9,
];
/// `착용 중인 장비는;넣을 수 없습니다`
const VAULT_WORN: &[u8] = &[
    0xC2, 0xF8, 0xBF, 0xEB, 0x20, 0xC1, 0xDF, 0xC0, 0xCE, 0x20, 0xC0, 0xE5, 0xBA, 0xF1, 0xB4, 0xC2,
    0x3B, 0xB3, 0xD6, 0xC0, 0xBB, 0x20, 0xBC, 0xF6, 0x20, 0xBE, 0xF8, 0xBD, 0xC0, 0xB4, 0xCF, 0xB4,
    0xD9,
];
/// `가방에 자리가 없습니다`
const VAULT_NO_ROOM: &[u8] = &[
    0xB0, 0xA1, 0xB9, 0xE6, 0xBF, 0xA1, 0x20, 0xC0, 0xDA, 0xB8, 0xAE, 0xB0, 0xA1, 0x20, 0xBE, 0xF8,
    0xBD, 0xC0, 0xB4, 0xCF, 0xB4, 0xD9,
];
/// `창고를 열 수 없습니다;파일이 손상되었습니다`
///
/// The one message that is not about a single item: [`crate::vault::Vault`]
/// refuses to touch a file it could not parse, so the screen must say so rather
/// than show an empty chest — an empty chest is exactly the lie that would make
/// the next write permanent.
const VAULT_BROKEN: &[u8] = &[
    0xC3, 0xA2, 0xB0, 0xED, 0xB8, 0xA6, 0x20, 0xBF, 0xAD, 0x20, 0xBC, 0xF6, 0x20, 0xBE, 0xF8, 0xBD,
    0xC0, 0xB4, 0xCF, 0xB4, 0xD9, 0x3B, 0xC6, 0xC4, 0xC0, 0xCF, 0xC0, 0xCC, 0x20, 0xBC, 0xD5, 0xBB,
    0xF3, 0xB5, 0xC7, 0xBE, 0xFA, 0xBD, 0xC0, 0xB4, 0xCF, 0xB4, 0xD9,
];
/// `비어 있는 슬롯입니다`
const VAULT_EMPTY_SLOT: &[u8] = &[
    0xBA, 0xF1, 0xBE, 0xEE, 0x20, 0xC0, 0xD6, 0xB4, 0xC2, 0x20, 0xBD, 0xBD, 0xB7, 0xD4, 0xC0, 0xD4,
    0xB4, 0xCF, 0xB4, 0xD9,
];

/// `MainMenuFrame_KeyPressed+0x2768` — where the block that loads a save slot
/// from the letter's slot list becomes common to both ways in.
///
/// **Not its head.** The head is `+0x2760`, but only the OK key reaches that:
/// the touch path fills `r5` and `sl` itself and branches to `+0x2768`
/// (`+0x2a44`, the second tap on the slot the cursor is already on). Hooking
/// the two instructions later is what makes one hook answer both, and it is a
/// measured fact — a hook on `+0x2760` let a touched slot fall through into the
/// letter's own menu.
///
/// The nine `bl` the block runs are already ported verbatim as
/// [`Vm::load_slot`], so the hook substitutes the whole thing rather than
/// reproducing it; the checked branches are what say it is still that recipe.
const VAULT_SLOT_OK: u32 = 0x2768;
const VAULT_SLOT_OK_WORD: u32 = 0xe794_6005; // ldr r6, [r4, r5]   -> _menuDepthCur
/// The two instructions above it, which the key path runs on its way in.
const VAULT_SLOT_OK_HEAD: u32 = 0x2760;
const VAULT_SLOT_OK_HEAD_WORD: u32 = 0xe59f_55e8; // ldr r5, [pc, #0x5e8]
const VAULT_SLOT_OK_NEXT: u32 = 0x2764;
const VAULT_SLOT_OK_NEXT_WORD: u32 = 0xe3a0_a000; // mov sl, #0
/// And the touch path's `b +0x2768`, checked by target so that "both ways in
/// pass here" is a fact about this build and not about the last one.
const VAULT_SLOT_TOUCH: u32 = 0x2a44;
/// The three `bl` inside it, checked by target: the talent table, the hero file
/// and the game file. Between them they pin the block to "load a save slot".
const VAULT_SLOT_TALENT: u32 = 0x2788;
const VAULT_SLOT_HERO: u32 = 0x27cc;
const VAULT_SLOT_GAME: u32 = 0x27f0;
/// And the key that reaches it, `cmn r1, #5` at `+0x2838` — the OK code.
const VAULT_SLOT_KEY: u32 = 0x2838;
const VAULT_SLOT_KEY_WORD: u32 = 0xe371_0005;

/// `MainMenuFrame_KeyPressed+0x248` — the shared epilogue every arm of the
/// function branches to, and where the slot hook has to leave.
///
/// **Not `LR`.** This hook stands in for a block in the middle of a function
/// that has already pushed nine registers and taken 0x24 bytes of frame, so
/// returning through `LR` would leave the stack 0x48 bytes out and everything
/// downstream running on garbage — measured: the screen came back as the
/// letter's own menu. The guest's own way out of this block is `b +0x4d0` ->
/// `cmp sl, #0` -> `beq +0x248`, and `sl` is zero on both ways in, so `+0x248`
/// is that path with the constant folded.
const VAULT_KEY_EXIT: u32 = 0x248;
const VAULT_KEY_EXIT_WORD: u32 = 0xe28d_d024; // add sp, sp, #0x24
const VAULT_KEY_EXIT_POP: u32 = 0x24c;
const VAULT_KEY_EXIT_POP_WORD: u32 = 0xe8bd_8ff0; // pop {r4..fp, pc}

/// `MainMenuFrame_KeyPressed+0x1348` — `cmp r2, #4`, the test that sends the
/// letter frame's keys to the bag grid, and the tail branch four words later.
/// Checking both is what proves `_mLetterState == 4` **is** the grid.
const VAULT_GRID_STATE: u32 = 0x1348;
const VAULT_GRID_STATE_WORD: u32 = 0xe352_0004;
const VAULT_GRID_KEY_TAIL: u32 = 0x135c;

/// `MainMenuFrame_drawLetter+0x558` — `bl draw_CommonItembag`, the other half of
/// the same fact on the drawing side.
const VAULT_GRID_DRAW_CALL: u32 = 0x558;

/// `MainMenuFrame_ItemBagKeyPressed` — the shipped wrapper, checked at its own
/// entry because the hook stands in for the whole 0xc4-byte function.
///
/// It is four facts: it calls `ItemBagKey` with a zero fourth argument, and it
/// reads `0x64` as "back" and `0x65` as "picked". The hook re-uses the first
/// (the movement, the tab strip, the touch hit-testing and the cursor are all
/// `ItemBagKey`'s) and replaces the tail, because the shipped tail files the
/// choice in `_mailBox+0xb6`/`+0xb8` — the letter's attachment slots, which a
/// chest transfer must not clobber.
const VAULT_BAG_KEY_PUSH: u32 = 0xe92d_4070; // push {r4, r5, r6, lr}
const VAULT_BAG_KEY_ARG: u32 = 0x4; // mov  r3, #0
const VAULT_BAG_KEY_ARG_WORD: u32 = 0xe3a0_3000;
const VAULT_BAG_KEY_CALL: u32 = 0x8; // bl   ItemBagKey
const VAULT_BAG_KEY_BACK: u32 = 0x18; // cmp  r0, #0x64
const VAULT_BAG_KEY_BACK_WORD: u32 = 0xe350_0064;
const VAULT_BAG_KEY_PICK: u32 = 0x24; // cmp  r0, #0x65
const VAULT_BAG_KEY_PICK_WORD: u32 = 0xe350_0065;
/// What those two comparisons are worth.
const BAG_KEY_BACK: u32 = 0x64;
const BAG_KEY_PICK: u32 = 0x65;

/// `draw_CommonItembag` — heading then grid, 0xbc bytes and nothing else.
///
/// The hook runs the shipped body through [`Vm::call`] and then draws the two
/// panel words over the (emptied) heading, so the only thing it adds is text.
const VAULT_BAG_DRAW_PUSH: u32 = 0xe92d_41f0; // push {r4, r5, r6, r7, r8, lr}
const VAULT_BAG_DRAW_ID: u32 = 0x1c; // mov  r0, #0x1f0
const VAULT_BAG_DRAW_ID_WORD: u32 = 0xe3a0_0e1f;
const VAULT_BAG_DRAW_STRING: u32 = 0x20; // bl   getInCodeString
const VAULT_BAG_DRAW_GRID: u32 = 0xa8; // bl   drawItemBag

/// `MainMenuFrame_drawMenuSlot` — the generic three-slot list, hooked only to
/// put a heading over it saying which screen it belongs to. Three callers
/// (캐릭관리, 편지, 시작) share it, so the hook is gated on the chest owning the
/// frame and on nothing else.
const VAULT_SLOT_DRAW_PUSH: u32 = 0xe92d_4ff0; // push {r4..fp, lr}

/// Where the shipped heading sits, off the middle of the screen:
/// `draw_CommonItembag` computes `(w/2 - 0x32, h/2 - 0x5c)` and passes a length
/// cap of 0x64. The two panel words take that band in halves, so the hit target
/// is exactly the text and nothing else on this screen is near it.
const VAULT_TITLE_DX: i32 = -0x32;
const VAULT_TITLE_DY: i32 = -0x5c;
const VAULT_TITLE_W: i32 = 0x64;
const VAULT_TITLE_H: i32 = 0x10;
/// The slot list's own heading goes above its first box, which
/// `MainMenuFrame_drawMenuSlot` puts at `(w/2 - 0x41, h/2 - 0x46)`.
const VAULT_SLOT_TITLE_DX: i32 = -0x41;
const VAULT_SLOT_TITLE_DY: i32 = -0x5a;

/// The two colours the panel words are drawn in, as `MC_grpGetPixelFromRGB`
/// takes them: the panel being shown, and the one a touch would switch to.
///
/// **Both are explicit, and that is the point.** `HF_DrawTextOutLine`'s `c1` of
/// `-1` (which [`HUD_C1`] is, and which the potion HUD passes) does not mean
/// white — it means *leave the colour alone*, and the renderer keeps whatever
/// was last set. Drawing the active word with `-1` therefore inherited the dim
/// colour as soon as the dim one came first: measured, on the chest panel both
/// words came out at `(99, 89, 74)` while on the bag panel only the second did.
const VAULT_LIT: (u32, u32, u32) = (0xff, 0xff, 0xff);
const VAULT_DIM: (u32, u32, u32) = (0x60, 0x58, 0x4c);

/// `hero + 0x89c` — gold. Read by `drawItemBag+0xd8` for the 소지금 line,
/// credited by `MONSTER_dropMoney`, debited by every shop and by
/// `MainMenuFrame_afterSendLetterWithItem`; 52 sites in all, every one a
/// plain word load or store.
const HERO_GOLD: u32 = 0x89c;
/// The keypad's cap: nine digits, the width of `vault::GOLD_CAP`.
const GOLD_DIGITS: u32 = 9;

/// Where the chest's gold line goes — the top left, the same `y` the shipped
/// grid prints 소지금 at on the right (`drawItemBag`: `(w - 0xd, 5)`) — and
/// where the two words that move gold follow it. Logical pixels.
const GOLD_LINE_X: i32 = 0xd;
const GOLD_LINE_Y: i32 = 5;
const GOLD_PUT_X: i32 = 0x6e;
const GOLD_TAKE_X: i32 = 0x96;
/// The two words' touch targets: the text plus a little, and nothing else on
/// this screen is near them — the back button's box starts at `w - 0x30` and
/// the first row of cells at `h/2 - 0x4b`.
const GOLD_PUT_HIT_X: i32 = 0x68;
const GOLD_PUT_HIT_W: i32 = 0x28;
const GOLD_TAKE_HIT_X: i32 = 0x92;
const GOLD_TAKE_HIT_W: i32 = 0x3a;
const GOLD_HIT_Y: i32 = 0;
const GOLD_HIT_H: i32 = 0x12;
/// Where the amount prompt starts, off the middle: the longer of the two is
/// thirteen syllables and three spaces, about 145 px wide.
const GOLD_ASK_DX: i32 = -0x48;

/// `o창고 &` — the amount follows. `o` and `&` are the two colour escapes the
/// shipped 소지금 line uses (`drawItemBag` formats `o%s &%d`), so the two lines
/// match without this side knowing what colour either is.
const GOLD_CHEST_LINE: &[u8] = &[b'o', 0xC3, 0xA2, 0xB0, 0xED, b' ', b'&'];
/// `넣기` / `꺼내기` — the two words that open the keypad.
const GOLD_PUT: &[u8] = &[0xB3, 0xD6, 0xB1, 0xE2];
const GOLD_TAKE: &[u8] = &[0xB2, 0xA8, 0xB3, 0xBB, 0xB1, 0xE2];
/// `창고에 넣을 금액을 입력하세요` / `창고에서 꺼낼 금액을 입력하세요` — over
/// the grid while the keypad is up.
const GOLD_ASK_PUT: &[u8] = &[
    0xC3, 0xA2, 0xB0, 0xED, 0xBF, 0xA1, 0x20, 0xB3, 0xD6, 0xC0, 0xBB, 0x20, 0xB1, 0xDD, 0xBE, 0xD7,
    0xC0, 0xBB, 0x20, 0xC0, 0xD4, 0xB7, 0xC2, 0xC7, 0xCF, 0xBC, 0xBC, 0xBF, 0xE4,
];
const GOLD_ASK_TAKE: &[u8] = &[
    0xC3, 0xA2, 0xB0, 0xED, 0xBF, 0xA1, 0xBC, 0xAD, 0x20, 0xB2, 0xA8, 0xB3, 0xBE, 0x20, 0xB1, 0xDD,
    0xBE, 0xD7, 0xC0, 0xBB, 0x20, 0xC0, 0xD4, 0xB7, 0xC2, 0xC7, 0xCF, 0xBC, 0xBC, 0xBF, 0xE4,
];
/// The refusals, all before step 1. `금액을 입력하세요` / `소지금이 부족합니다` /
/// `창고의 골드가 부족합니다` / `더 넣을 수 없습니다` / `더 꺼낼 수 없습니다` /
/// `지금은 옮길 수 없습니다`.
const GOLD_NONE: &[u8] = &[
    0xB1, 0xDD, 0xBE, 0xD7, 0xC0, 0xBB, 0x20, 0xC0, 0xD4, 0xB7, 0xC2, 0xC7, 0xCF, 0xBC, 0xBC, 0xBF,
    0xE4,
];
const GOLD_SHORT_HERO: &[u8] = &[
    0xBC, 0xD2, 0xC1, 0xF6, 0xB1, 0xDD, 0xC0, 0xCC, 0x20, 0xBA, 0xCE, 0xC1, 0xB7, 0xC7, 0xD5, 0xB4,
    0xCF, 0xB4, 0xD9,
];
const GOLD_SHORT_CHEST: &[u8] = &[
    0xC3, 0xA2, 0xB0, 0xED, 0xC0, 0xC7, 0x20, 0xB0, 0xF1, 0xB5, 0xE5, 0xB0, 0xA1, 0x20, 0xBA, 0xCE,
    0xC1, 0xB7, 0xC7, 0xD5, 0xB4, 0xCF, 0xB4, 0xD9,
];
const GOLD_CAP_PUT: &[u8] = &[
    0xB4, 0xF5, 0x20, 0xB3, 0xD6, 0xC0, 0xBB, 0x20, 0xBC, 0xF6, 0x20, 0xBE, 0xF8, 0xBD, 0xC0, 0xB4,
    0xCF, 0xB4, 0xD9,
];
const GOLD_CAP_TAKE: &[u8] = &[
    0xB4, 0xF5, 0x20, 0xB2, 0xA8, 0xB3, 0xBE, 0x20, 0xBC, 0xF6, 0x20, 0xBE, 0xF8, 0xBD, 0xC0, 0xB4,
    0xCF, 0xB4, 0xD9,
];
const GOLD_BUSY: &[u8] = &[
    0xC1, 0xF6, 0xB1, 0xDD, 0xC0, 0xBA, 0x20, 0xBF, 0xC5, 0xB1, 0xE6, 0x20, 0xBC, 0xF6, 0x20, 0xBE,
    0xF8, 0xBD, 0xC0, 0xB4, 0xCF, 0xB4, 0xD9,
];

/// The digits at the start of what the keypad returned, as an amount.
/// Anything past the first non-digit is ignored, and a run longer than the
/// cap saturates one above it, which every check then refuses.
fn gold_amount(typed: &[u8]) -> u32 {
    let cap = u64::from(crate::vault::GOLD_CAP) + 1;
    let mut n: u64 = 0;
    for &b in typed {
        if !b.is_ascii_digit() {
            break;
        }
        n = (n * 10 + u64::from(b - b'0')).min(cap);
    }
    n as u32
}

/// Where the chest screen's hooks go, or `None` if any word has drifted.
///
/// The four entry-point pushes are not scenery: each hook stands in for a whole
/// shipped function through [`Vm::call`], so the word it replaces has to still
/// be that function's first instruction. The rest pins the two facts the screen
/// is built on — that `_mLetterState == 4` is the bag grid on both the key and
/// the draw side, and that the slot block still loads a slot.
pub fn vault_sites(
    key: u32,
    draw_letter: u32,
    bag_key: u32,
    bag_draw: u32,
    slot_draw: u32,
    item_bag_key: u32,
    draw_item_bag: u32,
    in_code_string: u32,
    talent: u32,
    load_hero: u32,
    load_game: u32,
    mem: &mut GuestMem,
) -> Option<Vec<(u32, Patch)>> {
    let words: [(u32, u32); 12] = [
        (key.wrapping_add(VAULT_KEY_EXIT), VAULT_KEY_EXIT_WORD),
        (key.wrapping_add(VAULT_KEY_EXIT_POP), VAULT_KEY_EXIT_POP_WORD),
        (key.wrapping_add(VAULT_SLOT_OK), VAULT_SLOT_OK_WORD),
        (key.wrapping_add(VAULT_SLOT_OK_HEAD), VAULT_SLOT_OK_HEAD_WORD),
        (key.wrapping_add(VAULT_SLOT_OK_NEXT), VAULT_SLOT_OK_NEXT_WORD),
        (key.wrapping_add(VAULT_SLOT_KEY), VAULT_SLOT_KEY_WORD),
        (key.wrapping_add(VAULT_GRID_STATE), VAULT_GRID_STATE_WORD),
        (bag_key, VAULT_BAG_KEY_PUSH),
        (bag_key.wrapping_add(VAULT_BAG_KEY_ARG), VAULT_BAG_KEY_ARG_WORD),
        (bag_key.wrapping_add(VAULT_BAG_KEY_BACK), VAULT_BAG_KEY_BACK_WORD),
        (bag_key.wrapping_add(VAULT_BAG_KEY_PICK), VAULT_BAG_KEY_PICK_WORD),
        (bag_draw.wrapping_add(VAULT_BAG_DRAW_ID), VAULT_BAG_DRAW_ID_WORD),
    ];
    // The two entry words the hooks replace, and the six branches that say what
    // the blocks around them are for.
    let ok = words.iter().all(|&(at, want)| mem.r32(at) == want)
        && mem.r32(bag_draw) == VAULT_BAG_DRAW_PUSH
        && mem.r32(slot_draw) == VAULT_SLOT_DRAW_PUSH
        && branch_target(key.wrapping_add(VAULT_SLOT_TOUCH), mem, false)
            == Some(key.wrapping_add(VAULT_SLOT_OK))
        && branch_target(key.wrapping_add(VAULT_SLOT_TALENT), mem, true) == Some(talent)
        && branch_target(key.wrapping_add(VAULT_SLOT_HERO), mem, true) == Some(load_hero)
        && branch_target(key.wrapping_add(VAULT_SLOT_GAME), mem, true) == Some(load_game)
        && branch_target(key.wrapping_add(VAULT_GRID_KEY_TAIL), mem, false) == Some(bag_key)
        && branch_target(draw_letter.wrapping_add(VAULT_GRID_DRAW_CALL), mem, true)
            == Some(bag_draw)
        && branch_target(bag_key.wrapping_add(VAULT_BAG_KEY_CALL), mem, true) == Some(item_bag_key)
        && branch_target(bag_draw.wrapping_add(VAULT_BAG_DRAW_STRING), mem, true)
            == Some(in_code_string)
        && branch_target(bag_draw.wrapping_add(VAULT_BAG_DRAW_GRID), mem, true)
            == Some(draw_item_bag);
    ok.then(|| {
        vec![
            (key.wrapping_add(VAULT_SLOT_OK), Patch::VaultSlotOk),
            (bag_key, Patch::VaultBagKey),
            (bag_draw, Patch::VaultBagDraw),
            (slot_draw, Patch::VaultSlotDraw),
        ]
    })
}

impl Vm {
    /// Put a live monster of data id `id` next to the hero, the way
    /// `Map_SetMapData` does for a map's own monsters: `MONSTER_addBody` loads
    /// its data and sprite definition into a body, `MONSTER_imageLoadAll`
    /// loads the bodies' images, `MONSTER_Clone` makes an instance of the
    /// body at map coordinates. There are no monsters within reach of a
    /// scripted tap sequence; this is how the probe (`--monster`) exercises
    /// 자동기본공격.
    pub fn spawn_monster(&mut self, id: u32, dx: i32, dy: i32) -> Result<bool, String> {
        let (Some(get_hero), Some(add_body), Some(clone), Some(map), Some(load_images)) = (
            self.symbol("MP_getHero"),
            self.symbol("MONSTER_addBody"),
            self.symbol("MONSTER_Clone"),
            self.symbol("_map"),
            self.symbol("MONSTER_imageLoadAll"),
        ) else {
            return Ok(false);
        };
        let hero = self.call(get_hero, &[0])?;
        if hero == 0 {
            return Ok(false);
        }
        let x = (self.mem.r32(hero.wrapping_add(CHAR_X)) as i32).wrapping_add(dx);
        let y = (self.mem.r32(hero.wrapping_add(CHAR_Y)) as i32).wrapping_add(dy);
        // The body lands in the map's eight-slot body list (`_map+4`,
        // `Map_addMobBodyList`); mode 1 returns nothing, so find it there.
        let before: Vec<u32> = (0..8).map(|i| self.mem.r32(map + 4 + i * 4)).collect();
        self.call(add_body, &[id, 1])?;
        let body =
            (0..8).map(|i| self.mem.r32(map + 4 + i * 4)).find(|&b| b != 0 && !before.contains(&b));
        let Some(body) = body else { return Ok(false) };
        // the map loader loads every body's images in one pass after adding them
        self.call(load_images, &[])?;
        let m = self.call(clone, &[body, 0, x as u32, y as u32, 0, 0])?;
        if m != 0 {
            // `MONSTER_getType`; 5 is what `MONSTER_isObject` says yes to, which
            // is what 펫 보물탐색 (and 스마트모드) goes for.
            log::info!("monster {id}: type {}", self.mem.r8(m.wrapping_add(0xc6)));
        }
        Ok(m != 0)
    }

    /// Save the pet file once a rename this host asked for has been confirmed.
    ///
    /// Hooked at [`PET_COMMIT_END`], the last instruction of the commit block, so
    /// the name is already in `MP_getHero(1) + 0x62d` and `nameTitleIdx` is back
    /// to -1. Then it **declines**, and the guest's own `b` to the epilogue runs.
    ///
    /// The shipped path does not need this — the cutscene autosaves right after
    /// naming the pet — and the flag is what keeps this out of it: a run that
    /// never bought the item executes nothing here, so the guest instruction
    /// count is untouched. (A player who buys the item and then reaches the
    /// story's own naming scene without confirming cannot happen: the overlay
    /// only closes on OK.)
    pub(crate) fn host_pet_rename_save(&mut self) -> Result<bool, String> {
        if !self.pet_rename_pending {
            return Ok(false);
        }
        self.pet_rename_pending = false;
        if let Some(save) = self.symbol("GameFrame_savePetData") {
            self.call(save, &[])?;
            log::info!("potions: 작명 — pet name committed and saved");
        }
        Ok(false)
    }

    /// Rescale an experience award at the door.
    ///
    /// `HERO_add_Exp(who, amount)` is the single way experience enters the
    /// game — quest rewards from `Event_setQuestReward` and kills from
    /// `MONSTER_dropExp`, four call sites in all, hero and pet alike. So the
    /// whole effect is one register, and this **declines** afterwards: the
    /// shipped body then runs with the new argument and the level-up loop,
    /// the stat awards and the skill unlocks are all the guest's own. `r1` is
    /// still live at the entry (`+0x8` is `mov r6, r1`).
    ///
    /// Two rescalings meet here. A kill whose level gap `host_exp_gap_gate`
    /// bypassed is halved per level past [`EXP_GAP_FULL`], floored at 1 —
    /// pinned by LR to the two calls inside `MONSTER_dropExp`, so a quest
    /// reward that lands while the remembered gap is stale is left whole.
    /// Then the growth 비약 multiply what is left. The order does not matter
    /// to the arithmetic (both are per-award), but gap first keeps the floor
    /// of 1 from being multiplied away.
    ///
    /// Declining is also what makes recursion impossible — nothing is called.
    pub(crate) fn host_exp_mul(&mut self) -> Result<bool, String> {
        let amount = self.cpu.reg_get(Mode::User, 1) as i32;
        if amount <= 0 {
            return Ok(false);
        }
        let mut scaled = amount;
        if self.exp_gap > EXP_GAP_FULL {
            let lr = self.cpu.reg_get(Mode::User, reg::LR);
            if self.exp_gap_lrs.contains(&lr) {
                let shift = (self.exp_gap - EXP_GAP_FULL).min(30) as u32;
                scaled = (scaled >> shift).max(1);
            }
        }
        if crate::potions::enabled() && crate::potions::exp_enabled() {
            let mul = self.potions.exp_mul(self.wall_now_ms());
            scaled = scaled.saturating_mul(mul as i32);
        }
        if scaled != amount {
            self.cpu.reg_set(Mode::User, 1, scaled as u32);
        }
        Ok(false)
    }

    /// Bypass the kill experience level gap, keeping the gap.
    ///
    /// Sits on the `cmp r2, #5` at `MONSTER_dropExp+0x3c`; `r2` is already
    /// `hero level - monster level`. Within the gate's own threshold this
    /// declines with the gap remembered as harmless, and the guest's compare
    /// and branch run untouched. Past it, the compare and the `bgt` to the
    /// no-experience exit are skipped — the guest falls through to computing
    /// the monster's full award, which `host_exp_mul` then decays by the gap
    /// recorded here. The gap is re-recorded on every kill, so it can never
    /// go stale between monsters.
    pub(crate) fn host_exp_gap_gate(&mut self) -> Result<bool, String> {
        let gap = self.cpu.reg_get(Mode::User, 2) as i32;
        self.exp_gap = gap;
        if gap <= EXP_GAP_FULL {
            return Ok(false);
        }
        let pc = self.cpu.reg_get(Mode::User, reg::PC);
        self.cpu.reg_set(Mode::User, reg::PC, pc.wrapping_add(8));
        Ok(true)
    }

    /// Answer an item's level requirement five lower.
    ///
    /// One `ldrb Rd, [Rn, #7]` reproduced by the host with the loaded value
    /// lowered, then `PC += 4`. See [`ITEM_LEVEL_SITES`] for why every gate is
    /// a load rather than a call, and why there are eight of them.
    ///
    /// Nothing is written to the item, so this cannot outlive the instruction:
    /// a save written while the buff is up carries the real requirement, and
    /// when the week ends the gates close again. Equipment already worn stays
    /// on — the game re-checks nothing on load (`Item_loadStorage` and
    /// `HERO_load_data` both call `ITEM_setEquip` unconditionally) — which is
    /// the same bargain as the shipped permanent cash items.
    pub(crate) fn host_item_level(&mut self, rd: u8, rn: u8) -> Result<bool, String> {
        if !crate::potions::enabled() || !crate::potions::level_enabled() {
            return Ok(false);
        }
        if !self.potions.active(crate::potions::Kind::Finisher, self.wall_now_ms()) {
            return Ok(false);
        }
        let item = self.cpu.reg_get(Mode::User, rn);
        // Let the interpreter fault exactly where the guest would rather than
        // inventing a value for an item that is not there.
        let Some(byte) = self.mem.slice(item.wrapping_add(7), 1).map(|b| b[0]) else {
            return Ok(false);
        };
        // Read as signed: a requirement with bit 7 set is the game's "no
        // requirement", and lowering it must not turn it positive. The load
        // itself is unsigned, so the answer goes back as a zero-extended byte.
        let req = byte as i8 as i32;
        let lowered = if req > 0 { (req - crate::potions::level_delta()).max(0) } else { req };
        self.cpu.reg_set(Mode::User, rd, u32::from(lowered as u8));
        let pc = self.cpu.reg_get(Mode::User, reg::PC);
        self.cpu.reg_set(Mode::User, reg::PC, pc.wrapping_add(4));
        Ok(true)
    }

    /// Answer an item's network flag (`item[8]`) as -1, so the seven equip
    /// gates ([`NET_ITEM_SITES`]) let network equipment through to the same
    /// class/slot/level checks everything else gets.
    ///
    /// A normal item (the byte already reads negative) declines and the guest
    /// executes its own load — bit for bit what shipped. Nothing is written
    /// to the item, so saves keep the real flag and `CS_NET_EQUIP=0` closes
    /// the gates again.
    pub(crate) fn host_net_item_flag(&mut self, rd: u8, rn: u8) -> Result<bool, String> {
        if !net_equip() {
            return Ok(false);
        }
        let item = self.cpu.reg_get(Mode::User, rn);
        // Let the interpreter fault exactly where the guest would rather than
        // inventing a flag for an item that is not there.
        let Some(byte) = self.mem.slice(item.wrapping_add(8), 1).map(|b| b[0]) else {
            return Ok(false);
        };
        if (byte as i8) < 0 {
            return Ok(false);
        }
        self.cpu.reg_set(Mode::User, rd, u32::MAX);
        let pc = self.cpu.reg_get(Mode::User, reg::PC);
        self.cpu.reg_set(Mode::User, reg::PC, pc.wrapping_add(4));
        Ok(true)
    }

    /// Add one network item to a freshly loaded monster's drop table.
    ///
    /// Hooked at `MONSTER_loadCsv` and gated to the `r1 = 0` call in
    /// `MONSTER_dataLoad` ([`MON_LOADCSV_RET`]) — right after
    /// `MONSTER_loadMonsterStatus` wrote the twelve drop slots from the
    /// decrypted file, with `r0` still the monster. Always declines, so the
    /// guest's `loadCsv` runs as shipped.
    ///
    /// The entry goes into the first empty slot **after** the last used one
    /// in the 2..=10 pool: `MONSTER_searchDropItemWithRate` walks the pool as
    /// one cumulative window, so appending behind the shipped entries leaves
    /// every shipped probability untouched and only converts a slice of the
    /// "no drop" remainder. The kind is drawn from the kinds this monster
    /// already drops — a sword-dropping monster network-drops the sword
    /// family — so a monster with no equipment drops (most of them: the
    /// shipped pool is empty on the median monster) is left alone.
    ///
    /// `MONSTER_dataLoad` re-reads the file on every spawn (it has no cache),
    /// so the slot is rebuilt from the file each time and this cannot
    /// compound; a different roll per spawn is the variety, not a bug.
    pub(crate) fn host_net_item_drop(&mut self) -> Result<bool, String> {
        if !net_drop() || self.netitems.is_empty() {
            return Ok(false);
        }
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        if self.symbol("MONSTER_dataLoad").map(|a| a.wrapping_add(MON_LOADCSV_RET)) != Some(lr) {
            return Ok(false);
        }
        let m = self.cpu.reg_get(Mode::User, 0);

        // The shipped pool: which kinds it drops, and where it ends.
        let mut kinds: Vec<u8> = Vec::new();
        let mut last_used = *MON_DROP_POOL.start() - 1;
        for i in MON_DROP_POOL {
            let kind = self.mem.r16(m.wrapping_add(MON_DROP_KIND + 2 * i));
            if kind & 0x8000 != 0 {
                continue;
            }
            last_used = i;
            let kind = kind as u8;
            if self.netitems.has(kind) && !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }
        let free = last_used + 1;
        if kinds.is_empty() || !MON_DROP_POOL.contains(&free) {
            return Ok(false);
        }
        // A slot past the last used one can still be occupied if the file
        // interleaved empties; don't overwrite anything the file wrote.
        if self.mem.r16(m.wrapping_add(MON_DROP_KIND + 2 * free)) & 0x8000 == 0 {
            return Ok(false);
        }

        let Some((kind, idx)) = self.netitems.roll(&kinds) else { return Ok(false) };
        self.mem.w16(m.wrapping_add(MON_DROP_KIND + 2 * free), kind as u16);
        self.mem.w16(m.wrapping_add(MON_DROP_IDX + 2 * free), idx as u16);
        self.mem
            .write_bytes(m.wrapping_add(MON_DROP_RATE + 4 * free), &net_drop_rate().to_le_bytes());
        log::debug!("net drop: monster {m:#x} slot {free} gains kind {kind} idx {idx}");
        Ok(false)
    }

    /// Show the item panel's 요구레벨 as `45 40`, the 45 ruled through.
    ///
    /// The eight gates above answer a lowered number, but the tooltip is the
    /// one place the *original* still matters: a bare "요구레벨 40" hides the
    /// buff and looks like the item changed. So the tooltip's own load is left
    /// alone — the guest formats the real requirement — and the line is
    /// intercepted at the draw instead, exactly the way `host_cash_price`
    /// handles `CASH 1000` becoming `1000 0`.
    ///
    /// The splice goes *after* the digits so their pen offsets are unchanged,
    /// which is what lets the rule be measured with the guest's own
    /// `HF_GetWidth` on the spliced buffer. `font = (c2 >= 0)` for the line
    /// start and `font = 0` for the glyph offsets, for the reason spelled out
    /// in `host_cash_price`: the measurer's per-glyph pixel and its seeded 1
    /// cancel only in that combination.
    ///
    /// Anything unexpected declines and the shipped line is drawn: no digits,
    /// a requirement of zero, a delta that changes nothing, a full scratch
    /// buffer.
    pub(crate) fn host_item_info_level(&mut self) -> Result<bool, String> {
        if !crate::potions::enabled() || !crate::potions::level_enabled() {
            return Ok(false);
        }
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        if self.symbol("drawItemInfo").map(|a| a.wrapping_add(ITEM_INFO_LEVEL_RET)) != Some(lr) {
            return Ok(false);
        }
        if !self.potions.active(crate::potions::Kind::Finisher, self.wall_now_ms()) {
            return Ok(false);
        }
        let (Some(h2), Some(get_width)) =
            (self.symbol("HF_DrawTextOutlineH2"), self.symbol("HF_GetWidth"))
        else {
            return Ok(false);
        };

        let sp = self.cpu.reg_get(Mode::User, reg::SP);
        let regs: Vec<u32> = (0..4).map(|i| self.cpu.reg_get(Mode::User, i)).collect();
        let stack: Vec<u32> = (0..H2_ARGS / 4).map(|i| self.mem.r32(sp + i * 4)).collect();
        let arg = |off: u32| stack[(off / 4) as usize];
        // `c1` (the outline colour) is only carried through, never read.
        let (s, len, c2, flags) = (arg(H2_STR), arg(H2_LEN), arg(H2_C2), arg(H2_FLAGS));

        // EUC-KR bytes, walked the way the renderer walks them: a byte with
        // bit 7 set is a two-byte syllable and is stepped over whole, so an
        // ASCII digit here can only be the number.
        let text = self.mem.cstr_bytes(s);
        let mut i = 0;
        let head = loop {
            match text.get(i) {
                None | Some(b';') => return Ok(false),
                Some(&b) if b >= 0x80 => i += 2,
                Some(&b) if b.is_ascii_digit() => break i,
                Some(_) => i += 1,
            }
        };
        let mut tail = head;
        while text.get(tail).is_some_and(u8::is_ascii_digit) {
            tail += 1;
        }
        // The minus sign belongs to the number. `item[7]` is signed and the
        // game's starting gear carries -1 for "no requirement", which prints as
        // 요구레벨-1 — scanning for the first *digit* alone reads that as 1 and
        // rules through a requirement that was never there.
        let head = if head > 0 && text[head - 1] == b'-' { head - 1 } else { head };
        let Ok(req) = std::str::from_utf8(&text[head..tail]).unwrap_or("").parse::<i32>() else {
            return Ok(false);
        };
        let lowered = (req - crate::potions::level_delta()).max(0);
        // Nothing to say when there was no requirement to lower.
        if req <= 0 || lowered == req {
            return Ok(false);
        }

        let mut out = Vec::with_capacity(text.len() + 8);
        out.extend_from_slice(&text[..tail]);
        out.push(b' ');
        out.extend_from_slice(lowered.to_string().as_bytes());
        out.extend_from_slice(&text[tail..]);
        out.push(0);
        if out.len() > HUD_SCRATCH {
            return Ok(false);
        }
        let Some(buf) = self.potion_scratch() else { return Ok(false) };
        self.mem.write_bytes(buf, &out);

        let font = u32::from(c2 as i32 >= 0);
        let w_line = self.call(get_width, &[buf, len, font])? as i32;
        let x = regs[1] as i32;
        let pen = match flags {
            1 => x - (w_line >> 1),
            2 => x - w_line,
            _ => x,
        };
        let x0 = pen + self.pen_offset(buf, &out, head)?;
        let x1 = pen + self.pen_offset(buf, &out, tail)?;
        static DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *DEBUG.get_or_init(|| std::env::var("CS_POTION_DEBUG").is_ok()) {
            log::info!(
                "item level {:?}: head {head} tail {tail} {req} -> {lowered}, \
                 x {x} pen {pen} rule {x0}..{x1}",
                String::from_utf8_lossy(&text)
            );
        }

        let mut args = regs.clone();
        args.extend_from_slice(&stack);
        args[4 + (H2_STR / 4) as usize] = buf;
        self.call(h2, &args)?;
        self.strike_through(regs[0], x0, x1, regs[2] as i32 + strike_y());

        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Draw the running buffs beside the HP/MP block.
    ///
    /// Hooked at two whole-screen draws and pinned to the one caller each is
    /// wanted from — which doubles as the recursion guard, since `Vm::call`
    /// enters with `LR = RETURN_MAGIC`:
    ///
    /// * `IGM_DrawMenu`, from `GameMenuFrame_Render`. This is the one the
    ///   request asked for, and it needs its own hook because the HP/MP block
    ///   visible on that screen is a blurred snapshot, not a live draw.
    /// * `GameFrame_drawGameUI`, from `GameFrame_Render` — not from
    ///   `MainMenuOptionKey`, which draws the same UI as a preview behind the
    ///   options screen.
    ///
    /// The guest's own draw runs first, so the lines land on top of everything
    /// that screen puts down. Nothing is drawn at all when no buff is
    /// running, which is what makes the "off" A/B byte-identical.
    ///
    /// The gamepad badges ride the same hook, world screen only: the six skill
    /// buttons they label are the world screen's own, and the in-game menu maps
    /// the pad differently anyway (arrows navigate, A is OK). When
    /// neither feature wants this site — potions off or scoped elsewhere by
    /// `CS_POTION_HUD`, no pad attached — the hook declines outright and the
    /// guest runs its own instruction, untouched.
    pub(crate) fn host_hud(&mut self) -> Result<bool, String> {
        let pc = self.cpu.reg_get(Mode::User, reg::PC);
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        let site = [
            ("IGM_DrawMenu", "GameMenuFrame_Render", IGM_MENU_RET, true),
            ("GameFrame_drawGameUI", "GameFrame_Render", GAME_UI_RET, false),
        ]
        .into_iter()
        .find(|&(f, _, _, _)| self.symbol(f) == Some(pc));
        let Some((_, caller, ret, menu)) = site else { return Ok(false) };
        if self.symbol(caller).map(|a| a.wrapping_add(ret)) != Some(lr) {
            return Ok(false);
        }
        let potion =
            crate::potions::enabled() && crate::potions::hud_mode() >= if menu { 1 } else { 2 };
        let pad = !menu && self.gamepad_hud && pad_hud();
        if !potion && !pad {
            return Ok(false);
        }

        let args: Vec<u32> = (0..4).map(|i| self.cpu.reg_get(Mode::User, i)).collect();
        let r0 = self.call(pc, &args)?;
        if potion {
            self.draw_potion_hud()?;
        }
        if pad {
            self.draw_pad_hud()?;
        }
        self.cpu.reg_set(Mode::User, 0, r0);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Up to three lines through the game's own outlined-text renderer, in the
    /// colours its cash-shop price line uses. EUC-KR bytes go into a guest
    /// buffer untouched — the strings are built as bytes in `potions.rs` and
    /// never pass through `String`.
    fn draw_potion_hud(&mut self) -> Result<(), String> {
        let now = self.wall_now_ms();
        if !self.potions.any(now) {
            return Ok(());
        }
        let (Some(draw), Some(lcd_g), Some(buf)) =
            (self.symbol("HF_DrawTextOutLine"), self.symbol("_lcdG"), self.potion_scratch())
        else {
            return Ok(());
        };
        let g = self.mem.r32(lcd_g + 4);
        if g == 0 {
            return Ok(());
        }
        let mut y = HUD_Y;
        for kind in crate::potions::KINDS {
            let Some(line) = self.potions.hud_line(kind, now) else { continue };
            if line.len() >= HUD_SCRATCH {
                continue;
            }
            let len = line.len() as u32;
            self.mem.write_bytes(buf, &line);
            self.mem.w8(buf + len, 0);
            self.call(draw, &[g, HUD_X, y, buf, len, HUD_C1, HUD_C2, HUD_FLAGS])?;
            y += HUD_STEP;
        }
        Ok(())
    }

    /// 화면의 스킬 버튼 여섯 칸에, 그 슬롯을 쏘는 패드 버튼의 **자리**를 그린다.
    ///
    /// 자리는 게스트 자신의 히트 사각형이다(위 [`PAD_DX_OFF`] 주석). 배지는
    /// 그 사각형의 **좌하단 안쪽**에 놓는다 — 이 게임의 버튼들은 오른쪽 아래
    /// 모서리에서 위로 호를 그리며 올라가므로, 좌하단이 여섯 칸 모두에서 칸
    /// 안이면서 이웃과 가장 멀다.
    ///
    /// `GameFrame_drawGameUI` 본체가 이미 돌아간 뒤(`host_hud` 가 `Vm::call` 로
    /// 먼저 실행한다)라 버튼 위에 얹힌다. 프레임버퍼에 직접 쓴다 — 이 화면은 매
    /// 렌더 틱마다 다시 그려지고, 이 저장소의 호스트 블리터·폰트가 이미 같은
    /// 표면에 쓰고 있다.
    fn draw_pad_hud(&mut self) -> Result<(), String> {
        // 표는 부팅 때 한 번 확인해 뒀다(`Vm::boot`). 0 이면 그때 경고를 찍었고,
        // 여기서는 조용히 안 그린다.
        let tbl = self.pad_tbl;
        if tbl == 0 {
            return Ok(());
        }
        let Some(lcd_g) = self.symbol("_lcdG") else { return Ok(()) };
        // 레이아웃 크기는 게스트의 히트테스트가 쓰는 그 전역(`_lcdG+0x4c/0x50`)이고,
        // 픽셀은 온스크린 서피스 구조체(`*(_lcdG+4)`)다 — `host_box_blur` 가 같은
        // 것을 읽는다. 어긋나면 그리지 않는다: 크기가 다르면 자리가 틀리고, 16bpp
        // 가 아니거나 pitch 가 폭과 다르면 아래의 `2 * (y*w + x)` 가 남의 바이트를
        // 짚는다(그 쓰기는 폴트로 잡히고 **게스트 탓으로** 계상된다).
        let surf = self.mem.r32(lcd_g + 4);
        if surf == 0 {
            return Ok(());
        }
        let fb = FrameBuffer::read(&mut self.mem, surf);
        let (w, h) = (self.mem.r32(lcd_g + 0x4c) as i32, self.mem.r32(lcd_g + 0x50) as i32);
        if fb.pixels == 0 || w <= 0 || h <= 0 || fb.w != w || fb.h != h {
            return Ok(());
        }
        if fb.bpp != 16 || fb.pitch != w * 2 {
            return Ok(());
        }
        let pixels = fb.pixels;
        for i in 0..6usize {
            let dx = self.mem.r32(tbl + PAD_DX_OFF + 4 * i as u32) as i32;
            let dy = self.mem.r32(tbl + PAD_DY_OFF + 4 * i as u32) as i32;
            let (rx, ry) = (w + dx - PAD_SLOT_BACK, h + dy - PAD_SLOT_BACK);
            // 게스트가 아직 화면을 안 잡았거나 표가 이 화면에 안 맞으면 건너뛴다.
            if rx < 0 || ry < 0 || rx + PAD_SLOT_SIZE > w || ry + PAD_SLOT_SIZE > h {
                continue;
            }
            let Some(pos) = pos_for_key(PAD_SLOT_KEYS[i]) else { continue };
            self.draw_pad_badge(pixels, w, h, rx, ry, pos);
        }
        Ok(())
    }

    /// `GameFrame_PressTouchKeypad` 가 자기 좌표 표들을 재는 그 블록의 시작점.
    ///
    /// 프롤로그의 `add r4, pc, r4`(GOT) + `add lr, r4, lr` 가 만드는 값을 그대로
    /// 다시 계산한다. **두 `add` 의 인코딩**을 확인하고, 마지막으로 **키 표의 여섯
    /// 값**이 게임이 스킬 슬롯을 부르는 그 숫자인지까지 본다 — 그것이 「옳은 표를
    /// 잡았다」의 증명 전부다.
    ///
    /// `Vm::boot` 가 한 번 불러 [`Vm::pad_tbl`] 에 담아 두고, 실패하면 거기서
    /// 경고를 찍는다 — 이 저장소의 다른 `*_site` 들과 같은 자리다. 매 렌더 틱마다
    /// 다시 도는 검증이었을 때는 표가 움직여도 **배지가 그냥 안 뜨는 것**뿐이라
    /// 「패드 미인식」인지 「노브 꺼짐」인지 가릴 방법이 없었다.
    /// 읽기는 전부 `peek32`(논폴트)다. 여기서 하는 것은 **질문**이고, 표가 움직인
    /// 빌드에서 그 질문이 폴트 리포트를 만들면 안 된다 — `Vm::context` 가 스택을
    /// 훑다가 자기 폴트를 낳던 그것과 같은 함정이다.
    pub(crate) fn pad_slot_tables(&mut self) -> Option<u32> {
        let f = self.symbol("GameFrame_PressTouchKeypad")?;
        for (off, word) in PAD_TBL_WORDS {
            if self.mem.peek32(f.wrapping_add(off)) != word {
                return None;
            }
        }
        let got = f.wrapping_add(0x20).wrapping_add(self.mem.peek32(f.wrapping_add(PAD_TBL_LIT1)));
        let tbl = got.wrapping_add(self.mem.peek32(f.wrapping_add(PAD_TBL_LIT2)));
        for (i, want) in PAD_SLOT_KEYS.iter().enumerate() {
            if self.mem.peek32(tbl.wrapping_add(PAD_KEY_OFF + 4 * i as u32)) as i32 != *want {
                return None;
            }
        }
        Some(tbl)
    }

    /// 배지 하나. 칸의 좌하단 안쪽에 어두운 원판 + 테두리를 깔고, 그 위에 그
    /// 버튼 **집합**을 그리고 묶인 쪽만 밝게 한다.
    fn draw_pad_badge(&mut self, fb: u32, w: i32, h: i32, rx: i32, ry: i32, pos: PadPos) {
        let r = PAD_BADGE_R;
        let cx = rx + r + 1;
        let cy = ry + PAD_SLOT_SIZE - r - 2;
        self.fb_disc(fb, w, h, cx, cy, r, PAD_BADGE_FILL, Some(PAD_BADGE_RIM));
        match pos {
            // 어깨 넷: 좌/우 x 위/아래의 2x2 막대. 어깨 버튼이 실제로 그렇게
            // 생겼고, 점보다 막대가 이 크기에서 훨씬 잘 읽힌다.
            PadPos::Shoulder { right, lower } => {
                for &rt in &[false, true] {
                    for &lo in &[false, true] {
                        let bx = cx + if rt { 1 } else { -4 };
                        let by = cy + if lo { 1 } else { -3 };
                        let c = if rt == right && lo == lower {
                            PAD_BADGE_LIT
                        } else {
                            PAD_BADGE_UNLIT
                        };
                        self.fb_rect(fb, w, h, bx, by, 3, 2, c);
                    }
                }
            }
            // 스틱 누르기 둘: 좌/우 두 원.
            PadPos::Stick { right } => {
                for &rt in &[false, true] {
                    let bx = cx + if rt { 3 } else { -3 };
                    let c = if rt == right { PAD_BADGE_LIT } else { PAD_BADGE_UNLIT };
                    self.fb_disc(fb, w, h, bx, cy, 2, c, None);
                }
            }
        }
    }

    /// `CS_JOY_HERO5` 의 설치 검증 — 통과하면 `Draw_TouchPad` 의 주소.
    ///
    /// 훅이 앉는 draw 사이트 12개가 전부 `bl DM_DrawImgRotateEx` 인지,
    /// 서브하는 히트 사이트 넷이 전부 `bl Check_Rect` 인지 **타깃을
    /// 디코드해서** 보고, [`JOY_WORDS`] 일곱 워드로 「인자가 앵커다」
    /// 「+0x6c 가 방향이다」의 근거 명령들을 고정한다. 하나라도 다르면
    /// 설치하지 않는다 — 그리다 마는 것보다 게스트의 출고 패드가 낫다.
    ///
    /// 읽기는 전부 `peek32`(논폴트)다. `pad_slot_tables` 와 같은 이유 —
    /// 여기서 하는 것은 질문이고, 다른 빌드에서 그 질문이 폴트 리포트를
    /// 만들면 안 된다.
    pub(crate) fn joy_sites_ok(&mut self) -> Option<u32> {
        crate::joy::art()?;
        let base = self.symbol("Draw_TouchPad")?;
        self.symbol("mainFrame")?;
        let dm = self.symbol("DM_DrawImgRotateEx")?;
        let cr = self.symbol("Check_Rect")?;
        let keypad = self.symbol("GameFrame_PressTouchKeypad")?;
        fn bl_to(mem: &mut crate::mem::GuestMem, at: u32, target: u32) -> bool {
            let word = mem.peek32(at);
            word >> 24 == 0xeb
                && at.wrapping_add(8).wrapping_add((((word << 8) as i32) >> 6) as u32) == target
        }
        for off in std::iter::once(JOY_FIXED_FIRST)
            .chain(std::iter::once(JOY_FLOAT_FIRST))
            .chain(JOY_SKIP_DRAWS)
        {
            if !bl_to(&mut self.mem, base.wrapping_add(off).wrapping_sub(4), dm) {
                return None;
            }
        }
        for (off, _, _) in WORLD_PAD_HIT {
            if !bl_to(&mut self.mem, keypad.wrapping_add(off).wrapping_sub(4), cr) {
                return None;
            }
        }
        // MOVE 경로(`onHandleEvent`)의 rect 사본 넷과, `+0x70`/프리클리어의
        // 근거 워드들. 이 넷을 안 잡으면 press 만 hero5 판정이 되고 드래그가
        // 출고 사각형으로 재어져 걷기가 뚝뚝 끊긴다 — 기기에서 실제로 났다.
        let ohe = self.symbol("onHandleEvent")?;
        for (off, _, _) in WORLD_PAD_MOVE_HIT {
            if !bl_to(&mut self.mem, ohe.wrapping_add(off).wrapping_sub(4), cr) {
                return None;
            }
        }
        if JOY_MOVE_WORDS.iter().any(|&(o, w)| self.mem.peek32(ohe.wrapping_add(o)) != w) {
            return None;
        }
        if JOY_WORDS.iter().any(|&(o, w)| self.mem.peek32(base.wrapping_add(o)) != w) {
            return None;
        }
        Some(base)
    }

    /// hero5 스틱 하나 — 판, 그리고 idle 이 아니면 노브까지.
    ///
    /// 서피스·가드는 [`Vm::draw_dpad_plate`] 와 같다. `Draw_TouchPad` 의
    /// draw 자리에서 불리므로 z 순서(밑에 맵, 위에 OK 버튼)가 저절로 맞고,
    /// 게스트가 매 렌더 틱 그 밑을 다시 그리므로 알파가 누적되지 않는다 —
    /// 출고 부동 패드가 같은 자리에서 같은 방식으로 블렌드해 왔다.
    fn draw_joy_stick(&mut self, cx: i32, cy: i32, dir: Option<Dir>, idle: bool) {
        let Some(art) = crate::joy::art() else { return };
        let Some(lcd_g) = self.symbol("_lcdG") else { return };
        let surf = self.mem.r32(lcd_g + 4);
        if surf == 0 {
            return;
        }
        let fb = FrameBuffer::read(&mut self.mem, surf);
        let (w, sh) = (self.mem.r32(lcd_g + 0x4c) as i32, self.mem.r32(lcd_g + 0x50) as i32);
        if fb.pixels == 0 || w <= 0 || sh <= 0 || fb.w != w || fb.h != sh {
            return;
        }
        if fb.bpp != 16 || fb.pitch != w * 2 {
            return;
        }
        if idle {
            let s = &art.idle;
            self.fb_rgba(fb.pixels, w, sh, cx - s.w / 2, cy - s.h / 2, s);
        } else {
            let s = &art.ring;
            self.fb_rgba(fb.pixels, w, sh, cx - s.w / 2, cy - s.h / 2, s);
            let (kx, ky) = crate::joy::knob_offset(dir);
            let k = &art.knob;
            self.fb_rgba(fb.pixels, w, sh, cx + kx - k.w / 2, cy + ky - k.h / 2, k);
        }
    }

    /// RGBA8888 스프라이트 한 장을 565 프레임버퍼에 알파 블렌드로.
    fn fb_rgba(&mut self, fb: u32, w: i32, h: i32, x: i32, y: i32, s: &crate::joy::Sprite) {
        for row in 0..s.h {
            let yy = y + row;
            if yy < 0 || yy >= h {
                continue;
            }
            for col in 0..s.w {
                let xx = x + col;
                if xx < 0 || xx >= w {
                    continue;
                }
                let i = ((row * s.w + col) * 4) as usize;
                let a = s.data[i + 3] as u32;
                if a == 0 {
                    continue;
                }
                let src = (((s.data[i] as u32) >> 3) << 11)
                    | (((s.data[i + 1] as u32) >> 2) << 5)
                    | ((s.data[i + 2] as u32) >> 3);
                let at = fb + 2 * (yy * w + xx) as u32;
                let px =
                    if a == 255 { src } else { blend565(self.mem.r16(at) as u32, src, a) };
                self.mem.w16(at, px as u16);
            }
        }
    }

    /// 프레임버퍼에 채운 사각형 하나. RGB565, 화면 밖은 자른다.
    /// 한 방향의 육각 플레이트와 그 안의 삼각형.
    ///
    /// 픽셀은 온스크린 서피스에 직접 쓴다 — `draw_pad_hud` 와 같은 자리, 같은
    /// 가드다(크기가 레이아웃과 다르거나 16bpp·pitch 가 어긋나면 그리지 않는다.
    /// 어긋난 채로 쓰면 주소 산술이 남의 바이트를 짚고 그 쓰기는 폴트로 잡혀
    /// **게스트 탓으로 계상된다**).
    ///
    /// 게스트의 `DM_DrawImgRotateEx` 자리에서 불리므로 z 순서가 저절로 맞는다.
    fn draw_dpad_plate(&mut self, dir: Dir, plate: (i32, i32, i32, i32)) {
        let Some(lcd_g) = self.symbol("_lcdG") else { return };
        let surf = self.mem.r32(lcd_g + 4);
        if surf == 0 {
            return;
        }
        let fb = FrameBuffer::read(&mut self.mem, surf);
        let (w, sh) = (self.mem.r32(lcd_g + 0x4c) as i32, self.mem.r32(lcd_g + 0x50) as i32);
        if fb.pixels == 0 || w <= 0 || sh <= 0 || fb.w != w || fb.h != sh {
            return;
        }
        if fb.bpp != 16 || fb.pitch != w * 2 {
            return;
        }
        let (px, py, pw, ph) = plate;
        self.fb_hex(fb.pixels, w, sh, px, py, pw, ph, matches!(dir, Dir::Up | Dir::Down));
    }

    /// 육각형 하나, 채움과 테두리 한 겹. **넷이 한 모양의 회전**이라 다른 것은
    /// 어느 축이 뾰족한가뿐이다.
    ///
    /// `vertical` 이면 위·아래가 뾰족하고 좌·우가 평평하다(위/아래 판), 아니면
    /// 그것의 90° 회전이다(좌/우 판). 평평한 축을 따라 걸으면서 뾰족한 축의
    /// 반지름을 가운데의 `p/2` 에서 양 끝의 [`DPAD_FLAT`] 배로 줄인다 — 정수
    /// 산술이고, 그 줄의 양 끝 픽셀과 평평한 축의 양 끝 줄이 테두리다.
    ///
    /// 그라데이션만 회전을 안 따른다 — **언제나 위에서 아래로** 옅어진다.
    /// 빛은 버튼을 따라 돌지 않는다.
    #[allow(clippy::too_many_arguments)]
    fn fb_hex(
        &mut self,
        fb: u32,
        w: i32,
        h: i32,
        x: i32,
        y: i32,
        bw: i32,
        bh: i32,
        vertical: bool,
    ) {
        if bw < 4 || bh < 4 {
            return;
        }
        let (hw, hh) = (bw / 2, bh / 2);
        let (cx, cy) = (x + hw, y + hh);
        // `run` 은 평평한 축의 반지름(그 축을 따라 걷는다), `tip` 은 뾰족한
        // 축의 가운데 반지름.
        let (run, tip) = if vertical { (hw, hh) } else { (hh, hw) };
        let (fn_, fd) = DPAD_FLAT;
        for t in -run..=run {
            let k = t.abs();
            let r = tip - tip * (fd - fn_) * k / (fd * run.max(1));
            let flat = k == run;
            for u in -r..=r {
                let (xx, yy) = if vertical { (cx + t, cy + u) } else { (cx + u, cy + t) };
                if xx < 0 || yy < 0 || xx >= w || yy >= h {
                    continue;
                }
                // 위에서 아래로 가는 검정 그라데이션.
                // i32 으로 셈한다 — 위가 진하고 아래가 옅으므로 기울기가 음수이고,
                // u32 로 빼면 한 바퀴 돌아 알파가 255 를 넘어 색이 깨진다(실측).
                let a = (DPAD_ALPHA_TOP
                    + (DPAD_ALPHA_BOTTOM - DPAD_ALPHA_TOP) * (yy - (cy - hh)) / (2 * hh).max(1))
                .clamp(0, 255) as u32;
                let rim = flat || u == -r || u == r;
                let (c, a) = if rim { DPAD_RIM } else { (DPAD_FILL, a) };
                let at = fb + 2 * (yy * w + xx) as u32;
                let dst = self.mem.r16(at) as u32;
                self.mem.w16(at, blend565(dst, c as u32, a) as u16);
            }
        }
    }

    fn fb_rect(&mut self, fb: u32, w: i32, h: i32, x: i32, y: i32, bw: i32, bh: i32, c: u16) {
        for yy in y.max(0)..(y + bh).min(h) {
            for xx in x.max(0)..(x + bw).min(w) {
                self.mem.w16(fb + 2 * (yy * w + xx) as u32, c);
            }
        }
    }

    /// 채운 원, 선택적으로 테두리 한 겹까지 — **상자를 한 번만 걷는다**.
    ///
    /// 표본은 픽셀 **중심**이고 산술은 전부 정수다. 반지름 비교에 반 픽셀 엡실론
    /// (`r*r + 0.25`)을 얹은 판이 있었는데, `dx`/`dy` 가 정수라 `dx² + dy² ≤ r²`
    /// 와 결과가 한 픽셀도 다르지 않다 — 부동소수만 들여왔다.
    fn fb_disc(
        &mut self,
        fb: u32,
        w: i32,
        h: i32,
        cx: i32,
        cy: i32,
        r: i32,
        c: u16,
        rim: Option<u16>,
    ) {
        let (rr, inner) = (r * r, (r - 1) * (r - 1));
        for yy in (cy - r).max(0)..=(cy + r).min(h - 1) {
            for xx in (cx - r).max(0)..=(cx + r).min(w - 1) {
                let (dx, dy) = (xx - cx, yy - cy);
                let d = dx * dx + dy * dy;
                if d > rr {
                    continue;
                }
                let px = match rim {
                    Some(rc) if d > inner => rc,
                    _ => c,
                };
                self.mem.w16(fb + 2 * (yy * w + xx) as u32, px);
            }
        }
    }

    /// A guest buffer for the HUD lines, allocated on first use so the feature
    /// costs nothing when off. Separate from `cash_scratch` because the price
    /// patch can be mid-string when this runs.
    fn potion_scratch(&mut self) -> Option<u32> {
        if self.potion_buf == 0 {
            self.potion_buf = self.heap.alloc(HUD_SCRATCH as u32)?;
        }
        Some(self.potion_buf)
    }

    /// Enlarge the menu D-pad and set its margins off the two screen edges.
    ///
    /// Both are arguments, so the whole change is made at the entry and then
    /// **declined**: `x`/`y` are plain `r1`/`r2` and the 0xc0-byte shim never
    /// writes either before passing them on, and the scale pair is two words in
    /// the caller's outgoing stack block. Declining also matters for
    /// correctness — the left and right arrows are drawn at 90°/270°, which
    /// `host_rotate_blit` hands back to the interpreter, so anything applied
    /// further downstream would resize only half the cluster.
    ///
    /// The anchor argument is 9 at all 18 sites and `DM_CalcAnchorGbm` does
    /// nothing for it (none of 0x10/0x20/0x02/0x04), so `(x, y)` is the
    /// top-left of the *scaled* destination rect for every rotation and the
    /// host has to place it itself. `dpad_x`/`dpad_y` do that.
    ///
    /// The LR test is what keeps this off the other 200-odd sprite draws that
    /// come through here — including `Draw_TouchPad`'s twenty, which is why the
    /// in-game world joystick is untouched. It runs before anything is read
    /// from memory.
    pub(crate) fn host_menu_arrow(&mut self) -> Result<bool, String> {
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        // hero5 조이스틱 (`CS_JOY_HERO5`). 육각 플레이트보다 먼저 본다 —
        // 고정 패드의 네 사이트를 양쪽이 다 알고, 켜져 있으면 이쪽이 이긴다.
        if let Some(base) = self.joy_base {
            let off = lr.wrapping_sub(base);
            if off == JOY_FIXED_FIRST || off == JOY_FLOAT_FIRST {
                // 방향은 게스트 자신의 것 — `mainFrame+0x6c` (WIPI −1..−4).
                // 하이라이트 dispatch 가 읽는 그 워드다 (`JOY_WORDS` 로 고정).
                let dir = self
                    .symbol("mainFrame")
                    .map(|mf| self.mem.r32(mf + JOY_MF_DIR) as i32)
                    .and_then(crate::joy::dir_of_wipi);
                if off == JOY_FIXED_FIRST {
                    // TYPE B: hero5 의 고정 앵커 (65, H−65). 방향이 없으면
                    // hero5 의 반투명 idle 판이다.
                    let Some(lcd_g) = self.symbol("_lcdG") else { return Ok(false) };
                    let h = self.mem.r32(lcd_g.wrapping_add(LCD_H)) as i32;
                    let (cx, cy) = (crate::joy::ANCHOR_X, h - crate::joy::ANCHOR_BOTTOM);
                    self.draw_joy_stick(cx, cy, dir, dir.is_none());
                } else {
                    // TYPE A: 앵커가 이 호출의 인자에 실려 온다 — r1 = 앵커 x,
                    // r2 = 앵커 y − 0x28 (`JOY_WORDS` 의 +0x7b4..+0x7d0 이
                    // 그 근거다). 게스트는 앵커됨 플래그가 서 있을 때만
                    // 여기 오므로(+0x400) idle 에는 아무것도 안 그린다 —
                    // hero5 의 자유 방식과 같다.
                    let ax = self.cpu.reg_get(Mode::User, 1) as i32;
                    let ay = self.cpu.reg_get(Mode::User, 2) as i32 + 0x28;
                    self.draw_joy_stick(ax, ay, dir, false);
                }
                self.cpu.reg_set(Mode::User, 0, 0);
                self.cpu.reg_set(Mode::User, reg::PC, lr);
                return Ok(true);
            }
            if JOY_SKIP_DRAWS.contains(&off) {
                self.cpu.reg_set(Mode::User, 0, 0);
                self.cpu.reg_set(Mode::User, reg::PC, lr);
                return Ok(true);
            }
        }
        if dpad_hex() {
            if let Some(dir) = self
                .symbol("Draw_TouchPad")
                .and_then(|b| WORLD_PAD_DRAW.iter().find(|&&(o, _)| b + o == lr))
                .map(|&(_, d)| d)
            {
                let Some(lcd_g) = self.symbol("_lcdG") else { return Ok(false) };
                let h = self.mem.r32(lcd_g.wrapping_add(LCD_H)) as i32;
                let w = self.mem.r32(lcd_g.wrapping_add(LCD_W)) as i32;
                // **메뉴 D-pad 와 한 자리 한 간격이다**(제보). 다른 컨트롤이고
                // 출고 자리도 다르지만, 플레이어에게는 같은 패드다. 그래서
                // 같은 `dpad_plate` 를 같은 인자로 부른다 — 히트 상자도 바로
                // 아래에서 이것으로 갈아 끼운다.
                let s = dpad_scale();
                let plate = dpad_plate(dir, h, s, dpad_dx_for(w, s), dpad_dy());
                self.draw_dpad_plate(dir, plate);
                self.cpu.reg_set(Mode::User, 0, 0);
                self.cpu.reg_set(Mode::User, reg::PC, lr);
                return Ok(true);
            }
        }
        if !self.menu_arrow_lrs.contains(&lr) {
            return Ok(false);
        }
        let Some(lcd_g) = self.symbol("_lcdG") else { return Ok(false) };
        let h = self.mem.r32(lcd_g.wrapping_add(LCD_H)) as i32;
        let w = self.mem.r32(lcd_g.wrapping_add(LCD_W)) as i32;
        let s = dpad_scale();
        let (dx, dy) = (dpad_dx_for(w, s), dpad_dy());
        let x = self.cpu.reg_get(Mode::User, 1) as i32;
        let y = self.cpu.reg_get(Mode::User, 2) as i32;
        // The new cluster is drawn here instead of the shipped sprite: the
        // four calls carry the same frame at four rotations, so the position
        // the guest asked for *is* the direction. Anything else is a picture
        // this patch does not know, and is left to the guest.
        if dpad_hex() {
            if let Some(dir) = DPAD_SPRITES
                .iter()
                .find(|&&(sx, sy, _)| x == sx && y == h - sy)
                .map(|&(_, _, d)| d)
            {
                self.draw_dpad_plate(dir, dpad_plate(dir, h, s, dx, dy));
                self.cpu.reg_set(Mode::User, 0, 0);
                self.cpu.reg_set(Mode::User, reg::PC, lr);
                return Ok(true);
            }
            return Ok(false);
        }
        self.cpu.reg_set(Mode::User, 1, dpad_x(x, s, dx) as u32);
        self.cpu.reg_set(Mode::User, 2, dpad_y(y, h, s, dy) as u32);
        // Safe to write: every one of the eighteen sites stores its whole
        // argument block immediately before the `bl`, so this cannot survive
        // into the next call.
        let sp = self.cpu.reg_get(Mode::User, reg::SP);
        self.mem.w32(sp.wrapping_add(DPAD_SCALE_X), s as u32);
        self.mem.w32(sp.wrapping_add(DPAD_SCALE_Y), s as u32);
        Ok(false)
    }

    /// Put the D-pad's four touch boxes through the same transform, so what is
    /// drawn is what responds.
    ///
    /// `Check_Rect(px, py, rx, ry, [sp+0] = w, [sp+4] = h)` — a hit test that
    /// also fires the haptic, so leaving it behind would buzz in the old place.
    /// Matched on the rect *and* on the caller: the quadruple is copy-pasted
    /// into sixteen key handlers, 69 copies in all, and the four
    /// `(rx, ry, w, h)` tuples are distinctive (they tile the corner exactly),
    /// so the values find every copy without an address list that could go
    /// stale — while the caller range keeps a coincidental match on some other
    /// screen from moving a rect whose sprite this patch never touched.
    ///
    /// The rects are not the sprite boxes; they are 4 px wider and 2 px taller
    /// on each side. Running them through `dpad_x`/`dpad_y` unchanged preserves
    /// that, because both boxes are concentric and the transform is a scale
    /// about a shared corner.
    pub(crate) fn host_menu_arrow_hit(&mut self) -> Result<bool, String> {
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        // hero5 조이스틱 (`CS_JOY_HERO5`): TYPE B 의 사각형 넷을 hero5 의
        // 판정으로 **서브한다** — 우세축(동률은 세로) + 축별 데드존 10 +
        // 도달 반경(`CS_JOY_REACH`), 전부 고정 앵커 (65, H−65) 기준이다.
        // 사각형 넷으로는 이 판정을 표현할 수 없어서(사분면은 축 정렬
        // 사각형이 아니다) 인자 치환이 아니라 결과로 답한다. 게스트의
        // `Check_Rect` 는 히트에 진동을 겸하는데 이 빌드에 진동은 없다.
        if self.joy_base.is_some() {
            let sp = self.cpu.reg_get(Mode::User, reg::SP);
            let (rw, rh) = (self.mem.r32(sp) as i32, self.mem.r32(sp + 4) as i32);
            if (rw, rh) == (0x39, 0x26) || (rw, rh) == (0x26, 0x39) {
                let (rx, ry) =
                    (self.cpu.reg_get(Mode::User, 2) as i32, self.cpu.reg_get(Mode::User, 3) as i32);
                let lcd_g = self.symbol("_lcdG");
                let h = lcd_g.map(|g| self.mem.r32(g.wrapping_add(LCD_H)) as i32).unwrap_or(0);
                // 같은 사각형 넷이 두 함수에서 검사된다 — PRESS 는
                // `GameFrame_PressTouchKeypad`, MOVE(드래그·쥔 손가락의
                // 흔들림)는 `onHandleEvent` 의 자기 사본.
                let rect_is = |&(o, (x, dyy, ww, hh), _): &(u32, (i32, i32, i32, i32), Dir),
                               b: u32| {
                    b + o == lr && rx == x && ry == h - dyy && rw == ww && rh == hh
                };
                let press = self
                    .symbol("GameFrame_PressTouchKeypad")
                    .and_then(|b| WORLD_PAD_HIT.iter().find(|e| rect_is(e, b)));
                let moved = self
                    .symbol("onHandleEvent")
                    .and_then(|b| WORLD_PAD_MOVE_HIT.iter().find(|e| rect_is(e, b)));
                if let Some(&(_, _, dir)) = press.or(moved) {
                    let px = self.cpu.reg_get(Mode::User, 0) as i32;
                    let py = self.cpu.reg_get(Mode::User, 1) as i32;
                    let dx = px - crate::joy::ANCHOR_X;
                    let dy = py - (h - crate::joy::ANCHOR_BOTTOM);
                    let mf = self.symbol("mainFrame");
                    let want = if press.is_some() {
                        // PRESS = 새로 잡기. hero5 그대로의 엄격한 판이고,
                        // 직전 방향은 일부러 안 본다(새 손가락이다).
                        crate::joy::quantize(dx, dy, joy_reach())
                    } else {
                        // MOVE = 쥔 채 밀기. 직전 방향(`+0x70`)이 살아 있으면
                        // 훨씬 너그러운 판으로 잰다 — 히스테리시스가 없던 첫
                        // 판은 노브 위 엄지의 흔들림마다 release 가 나가 걷기가
                        // 뚝뚝 끊겼다(기기 제보). keep 반경의 상한 170 은 400
                        // 폭 화면의 스킬 버튼 열(x ≥ 241)에 닿기 직전이다.
                        let cur = mf
                            .map(|m| self.mem.r32(m + JOY_MF_PREV) as i32)
                            .and_then(crate::joy::dir_of_wipi);
                        match cur {
                            Some(c) => crate::joy::quantize_held(
                                dx,
                                dy,
                                c,
                                (joy_reach() * 2).min(170),
                            ),
                            None => crate::joy::quantize(dx, dy, joy_reach()),
                        }
                    };
                    let hit = want == Some(dir);
                    // press 경로는 `+0x70` 을 안 세우고(실측 — 쥐고만 있으면
                    // −999) MOVE 경로는 rect 검사 전에 `+0x6c` 를 지우므로,
                    // 첫 이동이 히스테리시스 없이 재어지는 공백이 생긴다.
                    // 그래서 잡힌 press 가 직접 채운다 — MOVE 팔이 자기
                    // 꼬리에서 하는 그 저장(`str dir,[r7,#0x70]`)과 같은
                    // 값이고, release 처리가 −999 로 되지우는 것도 실측했다.
                    if hit && press.is_some() {
                        if let Some(m) = mf {
                            self.mem
                                .w32(m + JOY_MF_PREV, crate::joy::wipi_of_dir(dir) as u32);
                        }
                    }
                    self.cpu.reg_set(Mode::User, 0, hit as u32);
                    self.cpu.reg_set(Mode::User, reg::PC, lr);
                    return Ok(true);
                }
            }
        }
        // 월드 고정 패드(TYPE B). 판이 메뉴와 한 자리에 있으므로 상자도 거기로
        // 옮긴다 — 안 옮기면 보이는 곳과 눌리는 곳이 갈린다. `Check_Rect` 는
        // 프레임마다 수백 번 불리므로 **크기 두 개로 먼저 거른 뒤에야** 심볼을
        // 찾는다.
        if dpad_hex() {
            let sp = self.cpu.reg_get(Mode::User, reg::SP);
            let (rw, rh) = (self.mem.r32(sp) as i32, self.mem.r32(sp + 4) as i32);
            if (rw, rh) == (0x39, 0x26) || (rw, rh) == (0x26, 0x39) {
                let (rx, ry) =
                    (self.cpu.reg_get(Mode::User, 2) as i32, self.cpu.reg_get(Mode::User, 3) as i32);
                let lcd_g = self.symbol("_lcdG");
                let h = lcd_g.map(|g| self.mem.r32(g.wrapping_add(LCD_H)) as i32).unwrap_or(0);
                let w = lcd_g.map(|g| self.mem.r32(g.wrapping_add(LCD_W)) as i32).unwrap_or(0);
                let base = self.symbol("GameFrame_PressTouchKeypad");
                let found = base.and_then(|b| {
                    WORLD_PAD_HIT.iter().find(|&&(o, (x, dyy, ww, hh), _)| {
                        b + o == lr && rx == x && ry == h - dyy && rw == ww && rh == hh
                    })
                });
                if let Some(&(_, _, dir)) = found {
                    let s = dpad_scale();
                    let (px, py, pw, ph) = dpad_plate(dir, h, s, dpad_dx_for(w, s), dpad_dy());
                    // 판보다 사방 이만큼만 넓다 — 중심도 간격도 그대로다.
                    let t = dpad_slop() * s / 100;
                    self.cpu.reg_set(Mode::User, 2, (px - t) as u32);
                    self.cpu.reg_set(Mode::User, 3, (py - t) as u32);
                    self.mem.w32(sp, (pw + 2 * t) as u32);
                    self.mem.w32(sp + 4, (ph + 2 * t) as u32);
                    return Ok(false);
                }
            }
        }
        if !self.menu_arrow_keys.iter().any(|&(lo, hi)| (lo..hi).contains(&lr)) {
            return Ok(false);
        }
        let Some(lcd_g) = self.symbol("_lcdG") else {
            return Ok(false);
        };
        let h = self.mem.r32(lcd_g.wrapping_add(LCD_H)) as i32;
        let rx = self.cpu.reg_get(Mode::User, 2) as i32;
        let ry = self.cpu.reg_get(Mode::User, 3) as i32;
        let sp = self.cpu.reg_get(Mode::User, reg::SP);
        let (w, rh) = (self.mem.r32(sp) as i32, self.mem.r32(sp + 4) as i32);
        if !MENU_ARROW_RECTS
            .iter()
            .any(|&(x, dy, rw, rrh)| rx == x && ry == h - dy && w == rw && rh == rrh)
        {
            return Ok(false);
        }
        // `w` above is the *rect's* width; the screen's needs its own name.
        let screen_w = self.mem.r32(lcd_g.wrapping_add(LCD_W)) as i32;
        let s = dpad_scale();
        let (dx, dy) = (dpad_dx_for(screen_w, s), dpad_dy());
        // Same argument as in `host_menu_arrow`: each of the 69 sites re-stores
        // all four words immediately before its own `bl`.
        if dpad_hex() {
            // The plate *is* the box now, so the rect is replaced rather than
            // transformed — the shipped rects were 4 px wider and 2 px taller
            // than their sprite, and a hexagon has no such margin to keep.
            let Some(dir) = MENU_ARROW_RECTS
                .iter()
                .zip(MENU_ARROW_DIRS)
                .find(|&(&(x, d, rw, rrh), _)| rx == x && ry == h - d && w == rw && rh == rrh)
                .map(|(_, d)| d)
            else {
                return Ok(false);
            };
            let (px, py, pw, ph) = dpad_plate(dir, h, s, dx, dy);
            self.cpu.reg_set(Mode::User, 2, px as u32);
            self.cpu.reg_set(Mode::User, 3, py as u32);
            self.mem.w32(sp, pw as u32);
            self.mem.w32(sp + 4, ph as u32);
            return Ok(false);
        }
        self.cpu.reg_set(Mode::User, 2, dpad_x(rx, s, dx) as u32);
        self.cpu.reg_set(Mode::User, 3, dpad_y(ry, h, s, dy) as u32);
        self.mem.w32(sp, (w * s / 100) as u32);
        self.mem.w32(sp + 4, (rh * s / 100) as u32);
        Ok(false)
    }

    /// Finish the teardown the cast-abort block leaves half done.
    ///
    /// Moving while a cast is charging aborts it at
    /// `HERO_Key_DirectKeyPress+0xc0`, a hand-copy of the game's two canonical
    /// cast teardowns with three stores missing and one extra:
    ///
    /// * `0x29a` (active-cast slot), `0x298` (queued-cast slot) and `0x290`
    ///   (queued-cast ticks) are never written, though every other teardown in
    ///   the binary writes them. This puts them back, but honestly: the queued
    ///   pair is unreachable, because every reader sits behind
    ///   `HERO_End_Process+0x218`'s `hero[0xc4] != -1` and the abort clears
    ///   `0xc4`; and the worst `0x29a` can do is make the *next* cast's first
    ///   gauge frame read a stale slot for the skill name. Hygiene, not the
    ///   fix.
    /// * `0x294` — the skill index the whole chain machinery keys off — *is*
    ///   cleared to -1, and neither canonical cast teardown touches it. That is
    ///   what kills chain skills, and `0x294` really is the key to everything:
    ///
    ///   - the proc roll is `UM_RandRatio(UiSkill_Get_Active_Condition_2nd(
    ///     hero[0x294]))`, i.e. `_uiskill[skill] + 0x20` percent — the 45% a
    ///     player sees. For the casting class only skill index 4 or 15 reaches
    ///     it (`HERO_Attack_Monster+0x1470`); everything else needs
    ///     `hero[0x29c] > 0` and goes through `+0x1324`.
    ///   - the install is skipped unless
    ///     `Check_Shortcut_Slot_Same_Skill(page, chainOf(hero[0x294])) == -1`.
    ///   - the restore looks the slot up the same way, from
    ///     `HERO_End_Process+0x1f4`.
    ///
    ///   So with `0x294 == -1` the restore cannot answer 3, `_copyslot` is
    ///   never copied back, and `0x67f` is cleared anyway. The chain skill is
    ///   left sitting in the player's shortcut slot — and from then on the
    ///   install's own "not already in a slot" precondition is never true
    ///   again. `0x280`, the install mutex, is only cleared on the idle
    ///   transition, and while it is set `HERO_Attack_Monster+0x12b8` refuses
    ///   to roll at all.
    ///
    /// So `0x294` is put back, but only while there is chain state that still
    /// needs it: a chain installed (`0x280`) or a restore pending (`0x67f`).
    /// With neither set there is nothing to protect and the guest's -1 stands,
    /// which is what keeps this from arming a roll for a skill that never
    /// landed. `HERO_End_Process` then does the restore and clears both `0x294`
    /// and `0x280` at the idle transition (`+0x5e8`), in that order, exactly as
    /// it does after an ordinary skill.
    ///
    /// Note the restore cannot run while the hero is still moving: `+0x70`
    /// sends state 2 straight to `+0x470`. It happens on the key release that
    /// puts the state back to 0 — which is why the value has to survive until
    /// then rather than being fixed up at the next attack.
    ///
    /// Not fixed, because it is not a bug: the cooldown starts at key-press
    /// time (`Set_Shortcut_Slot_State(..., 1)` in
    /// `HERO_Key_Press_Shortcut_Slot+0x274`), so an aborted cast still burns
    /// it. The original APK does exactly the same. (No MP is spent, though —
    /// that happens only on the fire path.)
    ///
    /// Wrapping rather than poking at the entry is what makes "did the abort
    /// actually run" observable — the block sits behind three tests, two of
    /// them after `HERO_Set_State`. The shipped body still runs exactly once.
    /// `MP_getHero` is pure (`base + 0xb90 * i`), so calling it first cannot
    /// reorder anything.
    pub(crate) fn host_cast_cancel(&mut self) -> Result<bool, String> {
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        // Our own re-entry, one frame down.
        if lr == crate::RETURN_MAGIC {
            return Ok(false);
        }
        let (Some(func), Some(get_hero)) =
            (self.symbol("HERO_Key_DirectKeyPress"), self.symbol("MP_getHero"))
        else {
            return Ok(false);
        };
        let (a0, a1) = (self.cpu.reg_get(Mode::User, 0), self.cpu.reg_get(Mode::User, 1));
        let hero = self.call(get_hero, &[a1])?;
        if hero == 0 {
            return Ok(false);
        }
        let casting = self.mem.r8(hero + HERO_STATE) == HERO_CASTING
            && self.mem.r32(hero + HERO_ACT_TICKS) as i32 > 0;
        let chain_idx = self.mem.r8(hero + HERO_CHAIN_IDX);

        let ret = self.call(func, &[a0, a1])?;

        // The abort is the only thing in there that zeroes the tick count while
        // clearing the active skill, so this is exactly "it ran".
        if casting
            && self.mem.r32(hero + HERO_ACT_TICKS) == 0
            && self.mem.r8(hero + HERO_ACT_SKILL) == 0xff
        {
            self.mem.w8(hero + HERO_ACT_SLOT, 0xff);
            self.mem.w8(hero + HERO_QUE_SLOT, 0xff);
            self.mem.w32(hero + HERO_QUE_TICKS, 0);
            let (held, pending) =
                (self.mem.r8(hero + HERO_CHAIN_HELD), self.mem.r8(hero + HERO_CHAIN_PENDING));
            let keep = held != 0 || pending != 0;
            if keep {
                self.mem.w8(hero + HERO_CHAIN_IDX, chain_idx);
            }
            // Rare — it takes moving during a cast — and it is the only way to
            // tell a working chain restore from a broken one while playing.
            log::info!(
                "cast abort: skill {} chain idx {} held={held} pending={pending}{}",
                self.mem.r8(hero + HERO_ACT_SKILL) as i8,
                chain_idx as i8,
                if keep { ", chain index kept" } else { "" }
            );
        }
        self.cpu.reg_set(Mode::User, 0, ret);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Answer `MC_knlGetSystemProperty("PHONENUMBER", out, size)`.
    ///
    /// Contract read off the shipped code: `r0` is the name, `r1` the
    /// destination, `r2` its size, and the function `memset`s the destination
    /// before filling it. So writing the digits plus a NUL and returning is the
    /// whole job. Any other name is declined and the guest answers it itself.
    ///
    /// The string is written as **bytes** — it is ASCII digits, but the rule in
    /// this codebase is that anything handed back to the guest never goes
    /// through `String`, and a number is no reason to make an exception.
    ///
    /// **`size` is not the bound that matters.** It is `getSKTPhoneNumber`'s
    /// 0x20-byte scratch buffer; what the identity has to fit is the sixteen
    /// bytes of the handshake field two calls later
    /// ([`identity::HANDSHAKE_FIELD`]), and overrunning *that* is silent — the
    /// seventeenth byte lands on the length out-parameter, which the guest
    /// immediately overwrites. `cs_id.txt` is a hand-editable file with no
    /// upper bound on its digit run, so this is reachable without any ID work.
    pub(crate) fn host_phone_number(&mut self) -> Result<bool, String> {
        let (name, out, size) = (
            self.cpu.reg_get(Mode::User, 0),
            self.cpu.reg_get(Mode::User, 1),
            self.cpu.reg_get(Mode::User, 2),
        );
        if self.mem.cstr_bytes(name) != crate::patch::PHONE_PROPERTY {
            return Ok(false);
        }
        // Refuse rather than truncate: a half-written number is a *different*
        // player as far as the server is concerned, which is worse than the
        // empty string the guest would have produced on its own.
        // The claimed ID once the player has one and the knob is on; otherwise
        // the minted number, byte for byte what this hook always sent.
        let owned;
        let digits: &[u8] = if id_as_phone() && self.ident.is_claimed() {
            owned = self.ident.wire().to_vec();
            &owned
        } else {
            self.identity.as_bytes()
        };
        if out == 0 || size as usize <= digits.len() {
            log::error!(
                "identity: {} digits do not fit MC_knlGetSystemProperty's {size}-byte buffer",
                digits.len()
            );
            return Ok(false);
        }
        // What the handshake actually carries is this string *after*
        // `getSKTPhoneNumber` rebuilds it, so measure that.
        let wire = crate::identity::wire_number(digits);
        if wire.len() > crate::identity::WIRE_MAX_BYTES {
            log::error!(
                "identity: {:?} reaches the handshake as {} bytes, over the {}-byte field; \
                 declining rather than sending a truncated address",
                String::from_utf8_lossy(digits),
                wire.len(),
                crate::identity::HANDSHAKE_FIELD,
            );
            return Ok(false);
        }
        for (i, b) in digits.iter().enumerate() {
            self.mem.w8(out + i as u32, *b);
        }
        self.mem.w8(out + digits.len() as u32, 0);
        // **Serving means returning.** The run loop `continue`s on `Ok(true)`
        // without stepping, so a handler that leaves PC alone spins on its own
        // instruction forever. Leaving this out did not show up at boot — the
        // startup path asks for MANUFACTURER and PHONEMODEL, never PHONENUMBER,
        // so the first call would have been the handshake, i.e. the moment the
        // player opened 편지.
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Say why 데이터 관리 does nothing, instead of letting it eat the save.
    ///
    /// Hooked on the two `mov r0, #0x64` that make `MainMenuOptionKey` return
    /// "go to the data-manage screen". Returning `0` instead is the guest's own
    /// "nothing happened" value (`+0x40`), so the menu stays exactly where it
    /// was and no other state is touched.
    ///
    /// PC is advanced by one instruction rather than jumped to LR: the very next
    /// instruction is the branch into the epilogue, so letting the guest run it
    /// keeps the stack unwinding in the guest's own hands.
    pub(crate) fn host_data_manage_blocked(&mut self) -> Result<bool, String> {
        let Some(set_popup) = self.symbol("setPopUp") else {
            return Ok(false);
        };
        // Allocated once and kept: this runs on a button press, and re-allocating
        // guest heap per press would leak.
        let buf = match self.data_manage_msg {
            Some(p) => p,
            None => {
                let n = DATA_MANAGE_MSG.len() as u32 + 1;
                let p = self.heap.alloc(n).ok_or("no guest heap for the message")?;
                for (i, b) in DATA_MANAGE_MSG.iter().enumerate() {
                    self.mem.w8(p + i as u32, *b);
                }
                self.mem.w8(p + DATA_MANAGE_MSG.len() as u32, 0);
                self.data_manage_msg = Some(p);
                p
            }
        };
        self.my_id_popup = false;
        self.call(set_popup, &[buf, 0])?;
        // The guest's own "no screen change" return value.
        self.cpu.reg_set(Mode::User, 0, 0);
        let pc = self.cpu.reg_get(Mode::User, reg::PC);
        self.cpu.reg_set(Mode::User, reg::PC, pc.wrapping_add(4));
        Ok(true)
    }

    /// Report that consent was already recorded.
    ///
    /// All three callers only test the return against zero, so this is the whole
    /// contract — nothing reads the buffer the real loader would have filled.
    pub(crate) fn host_sms_agreed(&mut self) -> Result<bool, String> {
        self.cpu.reg_set(Mode::User, 0, 1);
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Log what an options save is about to put on disk, and who asked.
    ///
    /// Always declines — the guest does the whole save itself. `GameFrame_
    /// saveOptionData` reads `_menuOpt` fields 9..0x14 into a 20-byte record,
    /// so dumping that window at entry shows exactly what will land in
    /// `OPTIONDATA`. The return address separates the twelve call sites, which
    /// is what tells "the row's own save" from "some other screen saving over
    /// it".
    pub(crate) fn host_opt_save_log(&mut self) -> Result<bool, String> {
        if let Some(opt) = self.symbol("_menuOpt") {
            let b: Vec<String> =
                (9..=0x18u32).map(|i| format!("{:02x}", self.mem.r8(opt + i))).collect();
            log::info!(
                "opt save: _menuOpt[9..0x19] = {} (pad = {}) from lr {:#x}",
                b.join(" "),
                self.mem.r8(opt.wrapping_add(MENU_OPT_PAD)),
                self.cpu.reg_get(Mode::User, reg::LR),
            );
        }
        Ok(false)
    }

    /// Rescue a touch-scheme byte the settings screen cannot reach, and
    /// otherwise leave the player's choice alone.
    ///
    /// See [`MENU_OPT_PAD`]: `_menuOpt+0x14` is the 터치방식 row, the settings
    /// screen writes it and saves it itself, and the loader runs several times
    /// per process — so anything unconditional here is a setting that silently
    /// resets. Only a value the UI cannot produce (`> 1`, which pins the fixed
    /// pad with no way back) is rewritten, and `CS_PAD_MODE >= 0` still forces
    /// a value outright for debugging.
    ///
    /// Wrapping the loader is still the right seam: it is the only place the
    /// byte is written from the save, so acting right afterwards is both the
    /// narrowest edit and the one the rest of the frame sees. It also sticks —
    /// `GameFrame_saveOptionData+0x7c` writes the field straight back out, so
    /// the next options save repairs the file without the host ever opening it.
    pub(crate) fn host_pad_mode(&mut self) -> Result<bool, String> {
        let want = pad_mode();
        let migrate = pad_migrate();
        if want < 0 && !migrate {
            return Ok(false);
        }
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        // Our own re-entry.
        if lr == crate::RETURN_MAGIC {
            return Ok(false);
        }
        let (Some(func), Some(opt)) =
            (self.symbol("GameFrame_loadOptionData"), self.symbol("_menuOpt"))
        else {
            return Ok(false);
        };
        let ret = self.call(func, &[])?;
        let was = self.mem.r8(opt.wrapping_add(MENU_OPT_PAD));
        match pad_target(was, want, migrate) {
            Some(v) if want >= 0 => {
                log::info!("touch pad mode: save says {was}, forcing {v} (0 = floating joystick)");
                self.mem.w8(opt.wrapping_add(MENU_OPT_PAD), v);
            }
            Some(v) => {
                log::warn!(
                    "touch pad mode: save says {was}, outside the settings range; using {v}"
                );
                self.mem.w8(opt.wrapping_add(MENU_OPT_PAD), v);
            }
            // `info`, not `debug`: the android logger drops `debug`, and this
            // is the one line that answers "did the setting survive?" on a
            // device. It runs a handful of times per session, not per frame.
            None => log::info!("touch pad mode: save says {was}, left as the player set it"),
        }
        self.cpu.reg_set(Mode::User, 0, ret);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Unwedge the casting gauge's file-static.
    ///
    /// `HERO_draw_casting_guage` keeps `total`, `elapsed` and an `initialised`
    /// flag in one unnamed block, and clears the flag only when
    /// `total < elapsed`. The frame that initialises it writes `total` and the
    /// flag and then jumps straight to that test **without drawing or bumping
    /// `elapsed`** — so a cast aborted in that one frame leaves
    /// `flag = 1, total = 0, elapsed = 0`, where `0 < 0` is false. Every later
    /// cast then takes the "already initialised" path, loads `total == 0`, and
    /// skips the draw. The bar is gone for the rest of the process, which is
    /// exactly the reported symptom.
    ///
    /// That triple is reachable no other way — an initialised gauge always has
    /// `total > 0`, and any frame that zeroes `total` has `elapsed > 0` and
    /// resets itself — so clearing the flag when it is seen is a strict
    /// superset of the guest's own reset and cannot change any case the guest
    /// already handles. It needs no hero pointer, which is what keeps it cheap
    /// enough for a function that runs every in-game frame.
    pub(crate) fn host_cast_gauge(&mut self) -> Result<bool, String> {
        let Some(g) = self.casting_gauge() else { return Ok(false) };
        if self.mem.r8(g + GAUGE_READY) != 0
            && self.mem.r32(g + GAUGE_TOTAL) == 0
            && self.mem.r32(g + GAUGE_ELAPSED) == 0
        {
            self.mem.w8(g + GAUGE_READY, 0);
            log::info!("casting gauge was wedged (total = elapsed = 0); unlatched");
        }
        Ok(false)
    }

    /// Advance a cutscene skip that the renderer has stopped advancing.
    ///
    /// See [`EV_SKIPPING`] for why the guest's skip cannot make progress on its
    /// own once `GameFrame_Render` takes one of the branches that misses
    /// `Event_Draw_Skip_Effect`, and why that leaves the game frozen for the
    /// rest of the process rather than merely looking wrong for a moment.
    ///
    /// Always declines — the guest's `EVENT_Tick` still runs, and on the tick
    /// where progress finally reaches the total it takes its own
    /// `EVENT_TickMovie` path exactly as it would have.
    pub(crate) fn host_skip_watchdog(&mut self) -> Result<bool, String> {
        let Some(ev) = self.symbol("_event") else { return Ok(false) };
        if self.mem.r8(ev.wrapping_add(EV_SKIPPING)) == 0 {
            self.skip_stall = 0;
            return Ok(false);
        }
        let progress = self.mem.r8(ev.wrapping_add(EV_SKIP_PROGRESS));
        let total = self.mem.r8(ev.wrapping_add(EV_SKIP_TOTAL));
        // Already at the end: this tick resumes the script by itself.
        if progress as i8 >= total as i8 {
            self.skip_stall = 0;
            return Ok(false);
        }
        if progress != self.skip_last {
            self.skip_last = progress;
            self.skip_stall = 0;
            return Ok(false);
        }
        self.skip_stall += 1;
        if self.skip_stall < SKIP_STALL_TICKS {
            return Ok(false);
        }
        // `Event_Draw_Skip_Effect+0x30`: progress + 1, clamped to the total.
        let next = if (progress as i8).saturating_add(1) > total as i8 {
            total
        } else {
            progress.wrapping_add(1)
        };
        self.mem.w8(ev.wrapping_add(EV_SKIP_PROGRESS), next);
        self.skip_last = next;
        self.skip_stall = 0;
        log::info!(
            "skip watchdog: cutscene skip stalled at {progress}/{total} for \
             {SKIP_STALL_TICKS} ticks (the event renderer is not running); advanced to {next}"
        );
        Ok(false)
    }

    /// Do not let a spare in the bag take an equip slot away from the item that
    /// is actually wearing it.
    ///
    /// `Item_loadStorage` rebuilds the eight equip pointers by matching each
    /// bag item's 64-bit creation stamp (`item+0xd0`) against the eight stamps
    /// the save recorded, and nothing else. Two items with the same stamp both
    /// match, and because `ITEM_setEquip` overwrites, the one later in bag
    /// order ends up worn — which is how a mace in the bag replaced the
    /// greatsword in the weapon slot after a trip to the main menu.
    /// [`crate::boot_clock_ms`] is why the stamps collided; this is the part
    /// that repairs a save in which they already have.
    ///
    /// The tie is broken with the game's own second record of the same fact:
    /// `item[0x98]`, which is 1 while worn and -1 while spare, is written into
    /// the save beside the stamp and read straight back. So the item that was
    /// really equipped can be told from the impostor without guessing.
    ///
    /// Deliberately asymmetric: an item that says "not equipped" is turned away
    /// **only when the slot already holds one that says it is**. An empty slot
    /// is always filled, whatever the flag says, so no reading of `[0x98]` can
    /// leave the player unarmed — and if the flag turned out to mean something
    /// else entirely, this patch would simply never fire. It also fixes the
    /// opposite bag order for free: spare first into an empty slot, then the
    /// real one overwrites it exactly as the guest intended.
    pub(crate) fn host_equip_restore(&mut self) -> Result<bool, String> {
        // `ITEM_setEquip` has 29 call sites — every equip action in the game
        // reaches it. Only the loader's stamp-match call is ours, so the hook
        // is pinned to that return address; the other 28 fall straight through.
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        let Some(base) = self.symbol("Item_loadStorage") else { return Ok(false) };
        if lr != base.wrapping_add(EQUIP_RESTORE_CALL).wrapping_add(4) {
            return Ok(false);
        }
        let item = self.cpu.reg_get(Mode::User, 2);
        // The loader's opening pass clears all eight slots with item = NULL.
        if item == 0 {
            return Ok(false);
        }
        if self.mem.r8(item.wrapping_add(ITEM_EQUIP_FLAG)) as i8 >= 0 {
            return Ok(false);
        }
        let (hero, slot) = (self.cpu.reg_get(Mode::User, 0), self.cpu.reg_get(Mode::User, 1));
        let Some(get_equip) = self.symbol("ITEM_getEquip") else { return Ok(false) };
        let held = self.call(get_equip, &[hero, slot])?;
        if held == 0 || (self.mem.r8(held.wrapping_add(ITEM_EQUIP_FLAG)) as i8) < 0 {
            return Ok(false);
        }
        log::info!(
            "equip restore: slot {slot} kept {held:#010x} (worn); \
             {item:#010x} shares its id but is a spare in the bag"
        );
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Address of that file-static, recovered from the two literals the
    /// function reaches it through. Declines (as `None`) if the result is not
    /// inside the loaded image, so a differently built guest writes nothing.
    fn casting_gauge(&mut self) -> Option<u32> {
        let f = self.symbol("HERO_draw_casting_guage")?;
        let got = f.wrapping_add(0x18).wrapping_add(self.mem.r32(f.wrapping_add(GAUGE_GOT_LIT)));
        let g = got.wrapping_add(self.mem.r32(f.wrapping_add(GAUGE_OFF_LIT)));
        (g >= crate::mem::IMAGE_BASE && g + GAUGE_ELAPSED + 4 < self.elf.image_end).then_some(g)
    }

    /// Keep the cutscene SKIP button on screen through the narration, the timed
    /// message and the screen fade.
    ///
    /// `EVENT_checkCanSkip` is a table lookup and nothing else, so answering 1
    /// for [`EXTRA_SKIPPABLE`] and letting the caller carry on is the whole
    /// change: `Event_Draw_Event` still applies its other four tests
    /// (`+0x690` popup, `+0x32a` message box, `+0x328` in-event, `+0x975` event
    /// wait) after this returns, so the button cannot appear outside a running
    /// cutscene.
    pub(crate) fn host_event_can_skip(&mut self) -> Result<bool, String> {
        let Some(ev) = self.symbol("_event") else { return Ok(false) };
        if !EXTRA_SKIPPABLE.contains(&self.mem.r32(ev.wrapping_add(EV_OPCODE))) {
            return Ok(false);
        }
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        if self.symbol("Event_Draw_Event").map(|a| a.wrapping_add(EVENT_CAN_SKIP_RET)) != Some(lr) {
            return Ok(false);
        }
        self.cpu.reg_set(Mode::User, 0, 1);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Let SKIP's keypress and tap through for the same opcodes.
    ///
    /// Two branch redirects, no reimplementation — see [`EVK_KEY_CHAIN`] for
    /// what each one steps over and where it lands. Every redirect target
    /// reloads the registers it uses from the GOT, so nothing depends on the
    /// state at the point the branch was taken.
    pub(crate) fn host_event_skip_key(&mut self) -> Result<bool, String> {
        let (Some(ev), Some(base)) = (self.symbol("_event"), self.symbol("Event_Key")) else {
            return Ok(false);
        };
        let op = self.mem.r32(ev.wrapping_add(EV_OPCODE));
        if !EXTRA_SKIPPABLE.contains(&op) {
            return Ok(false);
        }
        let to = match self.cpu.reg_get(Mode::User, reg::PC).wrapping_sub(base) {
            EVK_KEY_CHAIN => EVK_KEY_SKIP,
            EVK_TOUCH_CHAIN => EVK_TOUCH_SKIP,
            _ => return Ok(false),
        };
        self.cpu.reg_set(Mode::User, reg::PC, base.wrapping_add(to));
        Ok(true)
    }

    /// RIGHT on the 가방 cell of the game menu enters the bag, as if OK.
    ///
    /// The original moves the highlight on to 퀘스트 instead, and since
    /// 2026-09-24 that is the default again — this only installs with
    /// `CS_BAG_RIGHT=1`.
    ///
    /// Hooked at the one instruction that consumes RIGHT for this screen rather
    /// than at the function entry, because the drawn D-pad never produces a key
    /// code: a tap arrives as `MV_POINTER_PRESS` with x/y, is hit-tested by
    /// `Check_Rect`, and is reduced to the same stack flag this instruction
    /// loads. One address therefore covers the hardware key, the '6' alias and
    /// the on-screen arrow — and it cannot fire on any other screen, because
    /// the only branches into this block come from the state table's entry 12
    /// and from that state's own key/touch handler.
    ///
    /// Zeroing the flag before jumping is not tidiness: the OK handler falls
    /// back through this very block on its way out, so an unconsumed flag would
    /// both step the highlight on to 퀘스트 and re-enter this patch.
    pub(crate) fn host_bag_right(&mut self) -> Result<bool, String> {
        let (Some(state_p), Some(cur_p), Some(func)) = (
            self.symbol("_GameMenuState"),
            self.symbol("_g_IGMKey"),
            self.symbol("GameMenuFrame_KeyPressed"),
        ) else {
            return Ok(false);
        };
        let sp = self.cpu.reg_get(Mode::User, reg::SP);
        if self.mem.r32(sp.wrapping_add(0x1c)) == 0 {
            return Ok(false);
        }
        if self.mem.r32(state_p) != IGM_STATE_MENU || self.mem.r8(cur_p) != IGM_CELL_BAG {
            return Ok(false);
        }
        self.mem.w32(sp.wrapping_add(0x1c), 0);
        self.cpu.reg_set(Mode::User, reg::PC, func.wrapping_add(MENU_OK_ENTRY));
        Ok(true)
    }

    /// Using a 오토루팅 that is already applied takes the copy away.
    ///
    /// The original prints 「이미 오토루팅이 적용되어있습니다.」 and keeps the
    /// item, which for this one item means keeping it forever: it is
    /// 판매불가 by template price and its use dispatcher gives it a confirm
    /// popup instead of the 사용/버리기 menu, so there is no other way out of
    /// the bag. See the note above [`AUTOLOOT_APPLIED`] for how each of those
    /// three refusals was pinned down.
    ///
    /// This does exactly what the *other* arm of the same branch does with the
    /// copy — `ITEMSTORAGE_removeCurItem(1)`, which decrements the stack and
    /// unlinks the slot when it empties — and nothing else. It then declines,
    /// so the message, the error index and the state change are still the
    /// guest's own instructions running unmodified. There is no re-entrancy to
    /// guard: `Vm::call` saves and restores the whole CPU, and the call runs a
    /// leaf that never comes back through this address.
    pub(crate) fn host_autoloot_dup(&mut self) -> Result<bool, String> {
        let Some(remove_cur) = self.symbol("ITEMSTORAGE_removeCurItem") else {
            return Ok(false);
        };
        self.call(remove_cur, &[1])?;
        log::info!("auto-loot: already on, so the spare copy was used up");
        Ok(false)
    }

    /// Where the pen sits after `upto` bytes of `text`, in the *draw loop's*
    /// arithmetic rather than the measurer's.
    ///
    /// `HF_GetWidth` and `HF_DrawTextOutlineH2` do not agree, which is why the
    /// rules this draws used to sit a few pixels to the left of the digits they
    /// were meant to cross. Both walk the same string the same way — escapes
    /// free, a space 4, an ASCII glyph `HF_getRealWidthEng` = 5, a syllable
    /// `HF_hangleWidth` = 9 — but the drawn stride is **one pixel more per
    /// glyph**, measured off the screen: `요구레벨` strides 10 per syllable and
    /// `CASH` strides 6 per letter. The measurer's seeded 1 pays for exactly one
    /// of those, so the correction is one per glyph drawn, less that seed.
    ///
    /// Glyphs are counted here rather than in the guest because the measurer
    /// cannot report them, but which bytes *are* glyphs is still the guest's
    /// answer: `HF_IsEscape` decides, and a byte with bit 7 set is half of a
    /// two-byte syllable.
    fn pen_offset(&mut self, buf: u32, text: &[u8], upto: usize) -> Result<i32, String> {
        let Some(get_width) = self.symbol("HF_GetWidth") else { return Ok(0) };
        let width = self.call(get_width, &[buf, upto as u32, 0])? as i32;
        let Some(is_escape) = self.symbol("HF_IsEscape") else { return Ok(width) };
        let mut glyphs = 0;
        let mut i = 0;
        while i < upto {
            match text.get(i) {
                None | Some(0) | Some(b';') => break,
                Some(&b) if b >= 0x80 => {
                    glyphs += 1;
                    i += 2;
                }
                Some(&b) => {
                    if self.call(is_escape, &[u32::from(b)])? == 0 {
                        glyphs += 1;
                    }
                    i += 1;
                }
            }
        }
        // `HF_GetWidth2` reports 0, not 1, for a run that measured nothing but
        // its own seed — so the seed is only there to subtract when something
        // was measured.
        Ok(if width > 0 { width - 1 } else { 0 } + glyphs)
    }

    /// A guest buffer for the strings the cash-price patch builds, allocated on
    /// first use so the feature costs nothing when off.
    fn cash_scratch(&mut self) -> Option<u32> {
        if self.cash_buf == 0 {
            self.cash_buf = self.heap.alloc(CASH_SCRATCH as u32)?;
        }
        Some(self.cash_buf)
    }

    /// One row of white pixels across `[x0, x1)`. Clipped, and declines silently
    /// on anything but a 16bpp surface — a missing rule is cosmetic, a stray
    /// write into the framebuffer is not.
    fn strike_through(&mut self, fb_p: u32, x0: i32, x1: i32, row: i32) {
        if fb_p == 0 {
            return;
        }
        let fb = FrameBuffer::read(&mut self.mem, fb_p);
        if fb.w <= 0 || fb.pitch <= 0 || fb.pitch / fb.w != 2 {
            return;
        }
        if row < 0 || row >= fb.h {
            return;
        }
        let (lo, hi) = (x0.max(0), x1.min(fb.w));
        let base = fb.pixels.wrapping_add((row * fb.pitch) as u32);
        for cx in lo..hi {
            self.mem.w16(base.wrapping_add((cx * 2) as u32), 0xffff);
        }
    }

    /// Log a watched call's arguments and decline, so the guest runs its own
    /// code and behaviour is unchanged.
    pub(crate) fn watch_call(&mut self, pc: u32) -> Result<bool, String> {
        let sp = self.cpu.reg_get(Mode::User, reg::SP);
        let regs: Vec<String> =
            (0..4).map(|i| format!("{:#x}", self.cpu.reg_get(Mode::User, i))).collect();
        // Probe the stack rather than reading it: `r32` faults, and a function
        // entered with SP near the top of the stack (`clsidAuth` does) has fewer
        // than 16 words above it. Tracing must never be what kills the run.
        let stack: Vec<String> = (0..16)
            .map_while(|i| {
                let b = self.mem.slice(sp.wrapping_add(i * 4), 4)?;
                Some(format!("{:#x}", u32::from_le_bytes([b[0], b[1], b[2], b[3]])))
            })
            .collect();
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        log::info!(
            "{}({}) stack[{}] from {}",
            self.describe(pc),
            regs.join(", "),
            stack.join(", "),
            self.describe(lr)
        );
        Ok(false)
    }

    /// Host `DM_FillRectAlphaRGB(fb, x, y, w, h, alpha, r, g, b)` — the
    /// alpha-weighted rectangle fill behind dialogue boxes and HUD panels.
    ///
    /// The guest has two paths: a direct RGB565 one taken when
    /// `_lcdG[0x8a] == 1`, and a generic one that calls the
    /// `DM_ByteToIntPixel`/`DM_IntToBytePixel` function pointers **per
    /// pixel**. Only the 565 path is served here; the pointer path declines,
    /// same rule as a custom blit operator.
    pub(crate) fn host_fill_alpha(&mut self) -> Result<bool, String> {
        let sp = self.cpu.reg_get(Mode::User, reg::SP);
        let fb_p = self.cpu.reg_get(Mode::User, 0);
        let mut x = self.cpu.reg_get(Mode::User, 1) as i32;
        let mut y = self.cpu.reg_get(Mode::User, 2) as i32;
        let mut w = self.cpu.reg_get(Mode::User, 3) as i32;
        let mut h = self.mem.r32(sp) as i32;
        let alpha = (self.mem.r32(sp + 4) & 0xff) as i32;
        let cr = self.mem.r32(sp + 8) as i32;
        let cg = self.mem.r32(sp + 0xc) as i32;
        let cb = self.mem.r32(sp + 0x10) as i32;

        let lcd_g = match self.symbol("_lcdG") {
            Some(a) => a,
            None => return Ok(false),
        };
        if fb_p == 0 {
            return Ok(false);
        }
        let ret = |vm: &mut Vm| {
            let lr = vm.cpu.reg_get(Mode::User, reg::LR);
            vm.cpu.reg_set(Mode::User, reg::PC, lr);
            vm.fill_host += 1;
            Ok(true)
        };
        if alpha == 0 {
            return ret(self);
        }
        // DM_GET_FRAME_BUFFER_INFO, inlined: bpp is pitch / width.
        let fb = FrameBuffer::read(&mut self.mem, fb_p);
        if fb.w <= 0 || self.mem.r8(lcd_g.wrapping_add(0x8a)) != 1 {
            return Ok(false);
        }
        let bpp = fb.pitch / fb.w;

        // The prologue, transcribed: negative origins are folded into the
        // extent, overhang is trimmed against the surface.
        let col_off = if x >= 0 {
            x * bpp
        } else {
            w += x;
            x = 0;
            0
        };
        let row_off = if y >= 0 {
            y * fb.pitch
        } else {
            h += y;
            y = 0;
            0
        };
        if x + w > fb.w {
            w = fb.w - x;
        }
        if y + h > fb.h {
            h = fb.h - y;
        }
        if h <= 0 {
            return ret(self);
        }
        let base = fb.pixels.wrapping_add((row_off + col_off) as u32);
        if w > 0 {
            // Validate the whole touched span up front so the host either
            // serves the call completely or declines untouched.
            let span = ((h - 1) as i64 * fb.pitch as i64 + w as i64 * 2) as i64;
            if span <= 0 || fb.pitch < 0 {
                return Ok(false);
            }
            let ra = alpha * cr;
            let ga = alpha * cg;
            let ba = alpha * cb;
            let ia = 255 - alpha;
            let Some(buf) = self.mem.slice_mut(base, span as u32) else {
                return Ok(false);
            };
            for row in 0..h {
                let line = (row * fb.pitch) as usize;
                for col in 0..w {
                    let o = line + col as usize * 2;
                    let p = u16::from_le_bytes([buf[o], buf[o + 1]]) as i32;
                    let g6 = (p & 0x7e0) >> 3;
                    let b8 = (p << 3) & 0xff;
                    let r8 = (p & 0xf800) >> 8;
                    let out = ((((ia * g6 + ga) >> 5) & 0x7e0) | ((ia * r8 + ra) & 0xf800)) as u32
                        | (((ia * b8 + ba) as u32) << 16) >> 27;
                    buf[o] = out as u8;
                    buf[o + 1] = (out >> 8) as u8;
                }
            }
        }
        ret(self)
    }

    pub(crate) fn host_fill_rect(&mut self) -> Result<bool, String> {
        let sp = self.cpu.reg_get(Mode::User, reg::SP);
        let fb_p = self.cpu.reg_get(Mode::User, 0);
        let mut x = self.cpu.reg_get(Mode::User, 1) as i32;
        let mut y = self.cpu.reg_get(Mode::User, 2) as i32;
        let mut w = self.cpu.reg_get(Mode::User, 3) as i32;
        let mut h = self.mem.r32(sp) as i32;
        let ctx_p = self.mem.r32(sp + 4);

        if fb_p == 0 || ctx_p == 0 {
            return Ok(false);
        }
        let ctx = Ctx::read(&mut self.mem, ctx_p);
        if ctx.custom_op != 0 {
            return Ok(false);
        }
        let fb = FrameBuffer::read(&mut self.mem, fb_p);
        if fb.bpp != 16 || fb.pixels == 0 {
            return Ok(false);
        }
        let color = self.mem.r32(ctx_p + 0x14);

        // Prologue, transcribed: a negative extent flips the rectangle, then a
        // negative origin is absorbed into the extent.
        if w < 0 {
            x += w;
            w = -w;
        }
        if h < 0 {
            y += h;
            h = -h;
        }
        if x < 0 {
            w += x;
            x = 0;
        }
        if y < 0 {
            h += y;
            y = 0;
        }
        let y2 = (y + h).min(fb.h);
        let x2 = (x + w).min(fb.w);

        let blend = ctx.alpha <= 0xfe;
        let a = ctx.alpha.clamp(0, 255) as u32;
        let lo = x.max(ctx.clip_l);
        let hi = x2.min(ctx.clip_r);
        // `CS_FILL_DEBUG=1`. The game redraws only what it thinks is damaged,
        // so a full-screen fill arriving every frame is the difference between
        // "the picture accumulates" and "the picture is a single frame's worth
        // of drawing".
        static FILL_DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *FILL_DEBUG.get_or_init(|| std::env::var("CS_FILL_DEBUG").is_ok()) {
            log::info!(
                "fill fb={}x{} rect={}x{}@{x},{y} color={color:#x} alpha={}",
                fb.w,
                fb.h,
                x2 - x,
                y2 - y,
                ctx.alpha
            );
        }

        for row in y..y2 {
            if ctx.clip_t > row || ctx.clip_b <= row {
                continue;
            }
            let base = fb.pixels.wrapping_add((row * fb.pitch) as u32);
            for col in lo..hi {
                let addr = base.wrapping_add((col * 2) as u32);
                let v = if blend { blend565(self.mem.r16(addr) as u32, color, a) } else { color };
                self.mem.w16(addr, v as u16);
            }
        }

        self.fill_host += 1;
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Host `internalGrpCopyFrameBuffer(dst, dx, dy, w, h, src, sx, sy, ctx, useTrans)`.
    ///
    /// Returns `Ok(false)` without touching CPU state if the host cannot serve
    /// this call, in which case the caller must let the guest run its own code.
    pub(crate) fn host_blit(&mut self) -> Result<bool, String> {
        let sp = self.cpu.reg_get(Mode::User, reg::SP);
        let dst_p = self.cpu.reg_get(Mode::User, 0);
        let dx = self.cpu.reg_get(Mode::User, 1) as i32;
        let dy = self.cpu.reg_get(Mode::User, 2) as i32;
        let w = self.cpu.reg_get(Mode::User, 3) as i32;
        let h = self.mem.r32(sp) as i32;
        let src_p = self.mem.r32(sp + 4);
        let sx = self.mem.r32(sp + 8) as i32;
        let sy = self.mem.r32(sp + 0xc) as i32;
        let ctx_p = self.mem.r32(sp + 0x10);
        let use_trans = self.mem.r32(sp + 0x14) & 0xff;

        if dst_p == 0 || src_p == 0 || ctx_p == 0 {
            return Ok(false);
        }
        let ctx = Ctx::read(&mut self.mem, ctx_p);
        // A guest function pointer per pixel: no reason to be here.
        if ctx.custom_op != 0 {
            return Ok(false);
        }
        let dst = FrameBuffer::read(&mut self.mem, dst_p);
        let src = FrameBuffer::read(&mut self.mem, src_p);
        if dst.bpp != 16 || src.pixels == 0 || dst.pixels == 0 {
            // 32bpp exists in the guest but this game's screen is RGB565 and
            // nothing has ever been observed taking that path. Let the guest
            // handle it rather than shipping an untested transcription.
            return Ok(false);
        }

        self.blit_host += 1;
        // `CS_BLIT_DEBUG=1`. Counting lit source pixels separates "the copy is
        // wrong" from "there was nothing to copy" — the two halves of a black
        // screen that still runs the whole draw path.
        static BLIT_DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *BLIT_DEBUG.get_or_init(|| std::env::var("CS_BLIT_DEBUG").is_ok()) {
            let n = (src.w * src.h) as u32;
            let lit = match self.mem.slice(src.pixels, n * 2) {
                Some(b) => b.chunks_exact(2).filter(|p| p != &[0, 0]).count(),
                None => usize::MAX,
            };
            log::info!(
                "blit {w}x{h} src px={:#x} lit={lit}/{n} -> dst px={:#x}",
                src.pixels,
                dst.pixels
            );
        }
        if self.trace {
            log::debug!(
                "blit {w}x{h} src {}x{}@{sx},{sy} px={:#x} -> dst {}x{}@{dx},{dy} px={:#x} trans={} alpha={}",
                src.w, src.h, src.pixels, dst.w, dst.h, dst.pixels, use_trans, ctx.alpha
            );
        }
        run_blit16(&mut self.mem, dst, dx, dy, w, h, src, sx, sy, &ctx, use_trans != 0);

        // Return void.
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Host `DM_DrawRotate565Ex` (0x47c2c) and `DM_DrawRotate565` (0x486cc) —
    /// the paletted rotate/scale blitters that hold ~81% of a main-menu frame
    /// and ~73% of a title frame.
    ///
    /// Full transcriptions of the row-major walks (internal rotation codes
    /// 0..3: identity, H-mirror, V-mirror, 180°), including scaling, the
    /// `DM_CalcAnchorGbm` anchor adjustment, the `level` lookup-table blend,
    /// the `level == 0x10` grayscale mode and the `alpha <= 0xfe` per-pixel
    /// weighting. The 90°/270° transposed walks (codes 4..8, column-major
    /// writes) and non-paletted source formats decline back to the guest.
    ///
    /// Everything here mirrors the shipped code paths bit for bit — see the
    /// disassembly notes in ANALYSIS.md. `DM_DrawRotate565` is the same walk
    /// with the blend pipeline removed: `strh` of the palette entry.
    pub(crate) fn host_rotate_blit(&mut self, ex: bool) -> Result<bool, String> {
        let t0 = std::time::Instant::now();
        let entry = self.cpu.reg_get(Mode::User, reg::PC);
        let sp = self.cpu.reg_get(Mode::User, reg::SP);
        // Stack argument N lives at sp + 4N: this runs at the entry point,
        // before the guest's own `push`/`sub sp`.
        let mut arg = [0u32; 15];
        for (i, slot) in arg.iter_mut().enumerate().take(if ex { 15 } else { 13 }) {
            *slot = self.mem.r32(sp + 4 * i as u32);
        }

        let dst_p = self.cpu.reg_get(Mode::User, 0);
        let mut dx = self.cpu.reg_get(Mode::User, 1) as i32;
        let mut dy = self.cpu.reg_get(Mode::User, 2) as i32;
        let w = self.cpu.reg_get(Mode::User, 3) as i32;
        let h = arg[0] as i32;
        let dst_w = arg[1] as i32;
        let img_p = arg[3];
        let (sx, sy) = (arg[4] as i32, arg[5] as i32);
        let (rot, angle) = (arg[8], arg[9]);
        let (scale_x, scale_y) = (arg[10] as i32, arg[11] as i32);
        // The non-Ex variant has no level/alpha; the write is unconditional.
        let (level, alpha, anchor) =
            if ex { (arg[12], arg[13] as i32, arg[14]) } else { (0, 0xff, arg[12]) };

        // The guest's own first test.
        if dst_p == 0 || img_p == 0 {
            return Ok(false);
        }
        static ROT_DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *ROT_DEBUG.get_or_init(|| std::env::var("CS_ROT_DEBUG").is_ok()) {
            log::info!(
                "rot{} w={w} h={h} rot={rot} angle={angle} sc={scale_x}/{scale_y} level={level} alpha={alpha} anchor={anchor:#x}",
                if ex { "Ex" } else { "" }
            );
        }

        // The (rot, angle) pair collapses to one internal code (0x47cd0..).
        // 0..3 walk the destination row-major; 4..8 are the 90°/270° transposed
        // walks, which stay with the guest.
        let code = match rot {
            0 => match angle {
                0xb4 => 3,
                0x5a => 4,
                0x10e => 6,
                _ => 0,
            },
            1 => match angle {
                0 => 1,
                0xb4 => 2,
                0x5a => 7,
                0x10e => 8,
                _ => 0,
            },
            2 => match angle {
                0 => 2,
                0xb4 => 1,
                0x5a => 8,
                0x10e => 7,
                _ => 0,
            },
            3 => 3,
            _ => 0,
        };
        // 4/6/7/8 walk the destination column-major: the inner loop steps one
        // whole destination *row* (`add ip,ip,r3` with `r3 = dstW*2`, 0x48504)
        // and the outer loop steps one pixel to the right (`add r2,r2,#2`,
        // 0x48560). So the destination rectangle is `sh` wide by `sw` tall, and
        // each screen axis picks up the *other* source axis — but `xstep` and
        // `ystep` are not swapped with it: they still belong to the source's own
        // width and height. Everything else — anchor, palette, stride, base,
        // clip bounds, transparency, the whole blend — is a second register
        // allocation of the identical code.
        //
        // 5 is not in this numbering at all. No `mov r4,#5` exists in either
        // rotate blitter; the two-way mirror called 5 belongs to
        // `DM_DrawScreen565Ex`, which numbers its codes differently.
        let transposed = matches!(code, 4 | 6 | 7 | 8);
        let hflip = matches!(code, 1 | 3 | 6 | 7);
        let vflip = matches!(code, 2 | 3 | 4 | 7);
        if w < 1 || h < 1 || dst_w <= 0 {
            // The guest would walk a degenerate source; nothing real calls this.
            return Ok(false);
        }

        // `*(u8*)img->[0x20]`: 0xb is 4bpp paletted, 0xc is 8bpp. Any other
        // value leaves the guest's stride/base slots uninitialised (a real
        // call never does), so it must keep the guest's own behaviour.
        let img_data = self.mem.r32(img_p + 0x20);
        let fmt = self.mem.r8(img_data) as u32;
        if fmt != 0xb && fmt != 0xc {
            return Ok(false);
        }
        let four_bit = fmt == 0xb;

        // Every global this needs is reached PC-relatively, so it can be
        // resolved from the entry point without knowing the load address. The
        // literal pool sits at a fixed offset from each function's entry.
        let lits: (u32, u32, u32, u32) =
            if ex { (0xa8c, 0xa94, 0xa98, 0xa9c) } else { (0x638, 0x640, 0x644, 0) };
        let got_off = self.mem.r32(entry + lits.0);
        let got = entry.wrapping_add(0x24).wrapping_add(got_off);
        let o_clip = self.mem.r32(entry + lits.1);
        let o_trans = self.mem.r32(entry + lits.2);
        let clip_p = self.mem.r32(got.wrapping_add(o_clip));
        let trans_p = self.mem.r32(got.wrapping_add(o_trans));
        if clip_p == 0 || trans_p == 0 {
            return Ok(false);
        }
        let table = if ex {
            let o_tbl = self.mem.r32(entry + lits.3);
            self.mem.r32(got.wrapping_add(o_tbl))
        } else {
            0
        };
        if ex && level != 0x10 && table == 0 {
            return Ok(false);
        }
        let trans = self.mem.r32(trans_p);
        let (cl, ct) = (self.mem.r32(clip_p) as i32, self.mem.r32(clip_p + 4) as i32);
        let (cr, cb) = (self.mem.r32(clip_p + 8) as i32, self.mem.r32(clip_p + 0xc) as i32);

        // scaledW/H, truncated toward zero exactly like the guest's div-by-100,
        // then clamped to 1. The 10.10 steps get the guest's observable `+1`.
        let sw = ((w * scale_x) / 100).max(1);
        let sh = ((h * scale_y) / 100).max(1);
        let xstep = (w.wrapping_shl(10) / sw) + 1;
        let ystep = (h.wrapping_shl(10) / sh) + 1;

        // `DM_CalcAnchorGbm(&dx, &dy, scaledW, scaledH, rot, angle, anchor)`.
        // The same arithmetic the hooked function itself serves, so the oracle
        // test on that covers this too. The angle-swap block is reachable here
        // only through rot == 3 with a 90/270 angle, which still walks row-major.
        (dx, dy) = calc_anchor_gbm(dx, dy, sw, sh, angle, anchor);

        let palette = self.mem.r32(img_p + 0x10);
        // The guest writes this back before drawing; keep doing so, because the
        // caller may read it afterwards.
        self.mem.write_bytes(img_p + 0x1c, &palette.to_le_bytes());

        // Source addressing: the 4bpp block at 0x48620, the 8bpp one at 0x48654.
        // They differ only in the stride and in how `sx` folds into the base.
        let img_w = self.mem.r32(img_p) as i32;
        let head = (self.mem.r32(img_p + 0x14) as i32 + 4) * 2 + 1;
        let (stride, base) = if four_bit {
            let stride = (img_w + (img_w & 1)) >> 1;
            (stride, head + (sx >> 1) + stride * sy)
        } else {
            (img_w, head + sx + img_w * sy)
        };

        // Clip once, up front, instead of per pixel: the guest walks every
        // output pixel and tests the clip rectangle on each one. A transposed
        // walk covers `sh` across and `sw` down, and the guest's four clip
        // compares (0x48328) bind to the screen axes either way — so only the
        // extents exchange, not the tests.
        let (ext_w, ext_h) = if transposed { (sh, sw) } else { (sw, sh) };
        let (x0, x1) = (dx.max(cl), (dx + ext_w).min(cr));
        let (y0, y1) = (dy.max(ct), (dy + ext_h).min(cb));
        if x0 >= x1 || y0 >= y1 {
            self.rot_host += 1;
            let lr = self.cpu.reg_get(Mode::User, reg::LR);
            self.cpu.reg_set(Mode::User, reg::PC, lr);
            return Ok(true);
        }

        // Source index of output column i / row j — the guest's accumulator,
        // written multiplicatively (identical mod 2^32), plus the mirror flips.
        let src_ix = |i: i32| -> i32 {
            let s = i.wrapping_mul(xstep) >> 10;
            if hflip {
                (w - 1) - s
            } else {
                s
            }
        };
        let src_iy = |j: i32| -> i32 {
            let s = j.wrapping_mul(ystep) >> 10;
            if vflip {
                (h - 1) - s
            } else {
                s
            }
        };

        // Everything the inner loop reads is pulled into host memory first.
        // Going through the checked guest accessors per pixel costs more than
        // the guest instructions this replaces — measured, not assumed.
        let mut pal = [0u16; 256];
        let pal_n = if four_bit { 16 } else { 256 };
        match self.mem.slice(palette, pal_n as u32 * 2) {
            Some(b) => {
                for (i, e) in pal.iter_mut().enumerate().take(pal_n) {
                    *e = u16::from_le_bytes([b[i * 2], b[i * 2 + 1]]);
                }
            }
            None => return Ok(false),
        }
        // One 32x32 block per channel value, selected by the alpha level.
        let mut blend = Blend565::new(alpha, level);
        if ex && !blend.load(&mut self.mem, table, level) {
            return Ok(false);
        }

        // Source bytes actually touched, as one contiguous span. The index maps
        // are monotonic in i and j (mirrored or not), so the extremes are at
        // the endpoints.
        let (ia, ib) = (x0 - dx, x1 - 1 - dx);
        let (ja, jb) = (y0 - dy, y1 - 1 - dy);
        // Which screen axis feeds which source axis. Row-major: the destination
        // column indexes the source column. Transposed: it indexes the source
        // *row*, and the destination row indexes the source column.
        let (ca, cz) = if transposed { (src_ix(ja), src_ix(jb)) } else { (src_ix(ia), src_ix(ib)) };
        let (ra, rz) = if transposed { (src_iy(ia), src_iy(ib)) } else { (src_iy(ja), src_iy(jb)) };
        let (cmin, cmax) = (ca.min(cz), ca.max(cz));
        let (rmin, rmax) = (ra.min(rz), ra.max(rz));
        let (bmin, bmax) = if four_bit { (cmin >> 1, cmax >> 1) } else { (cmin, cmax) };
        let span_lo = base + rmin * stride + bmin;
        let span_hi = base + rmax * stride + bmax + 1;
        if span_lo < 0 || span_hi <= span_lo {
            return Ok(false);
        }
        let src =
            match self.mem.slice(img_data.wrapping_add(span_lo as u32), (span_hi - span_lo) as u32)
            {
                Some(b) => b.to_vec(),
                None => return Ok(false),
            };

        // ...and the destination rectangle, read-modify-written as one block.
        let dst_lo = (y0 * dst_w + x0) * 2;
        let dst_len = ((y1 - 1 - y0) * dst_w + (x1 - x0)) * 2;
        if dst_lo < 0 || dst_len <= 0 {
            return Ok(false);
        }
        let dst_addr = dst_p.wrapping_add(dst_lo as u32);
        let mut buf = match self.mem.slice(dst_addr, dst_len as u32) {
            Some(b) => b.to_vec(),
            None => return Ok(false),
        };

        for j in ja..=jb {
            let out_row = ((j - ja) * dst_w) as usize * 2;
            // The destination row fixes one source axis for the whole row: the
            // source row when walking row-major, the source column when
            // transposed.
            let outer = if transposed { src_ix(j) } else { src_iy(j) };
            for i in ia..=ib {
                let (src_row, sx_c) =
                    if transposed { (src_iy(i), outer) } else { (outer, src_ix(i)) };
                let row_off = (src_row - rmin) * stride;
                let bidx = if four_bit { sx_c >> 1 } else { sx_c };
                let byte = src[(row_off + (bidx - bmin)) as usize];
                let idx = if !four_bit {
                    byte as usize
                } else if sx_c & 1 != 0 {
                    (byte & 0xf) as usize
                } else {
                    // Even column is the high nibble (0x480e8).
                    (byte >> 4) as usize
                };
                let s = pal[idx] as u32;
                // `cmp r3, sl` compares the 16-bit palette entry against the
                // full 32-bit transparent word.
                if s == trans {
                    continue;
                }
                let o = out_row + (i - ia) as usize * 2;
                if !ex {
                    // DM_DrawRotate565: `strhgt r3, [ip]` — the palette entry.
                    buf[o..o + 2].copy_from_slice(&(s as u16).to_le_bytes());
                    continue;
                }

                let d = u16::from_le_bytes([buf[o], buf[o + 1]]) as u32;
                let out = blend.apply(s, d);
                buf[o..o + 2].copy_from_slice(&out.to_le_bytes());
            }
        }
        self.mem.write_bytes(dst_addr, &buf);

        self.rot_host += 1;
        self.rot_ns += t0.elapsed().as_nanos() as u64;
        if self.trace {
            log::debug!(
                "rot{} {w}x{h}@{scale_x}/{scale_y}% code={code} -> {dx},{dy} level={level} alpha={alpha}",
                if ex { "Ex" } else { "" }
            );
        }

        // Return void.
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Host `DM_DrawScreen565` (0x3eb04) and `DM_DrawScreen565Ex` (0x401e8) —
    /// the unscaled paletted blitters that draw every sprite, tile and
    /// full-screen illustration in the game.
    ///
    /// These are the last two big ones, and by a wide margin the most expensive
    /// thing left: 99.4% of all guest instructions on the title screen, and
    /// 73.6% during the effect-heavy intro fight. Neither has a single `bl` call
    /// site — they are reached only indirectly, through a GOT function-pointer
    /// table keyed on the screen's pixel format, from `DM_DrawImg+0x198` and
    /// `DM_DrawImgEx+0x1b0` (`blx ip`). A third dispatch site exists at
    /// `DM_OffDrawImg+0x18c` (`mov lr,pc / ldr pc,[ip]`, drawing into an
    /// off-screen GBM), but `DM_OffDrawImg` is unreachable: nothing branches to
    /// it and its address appears only in `.dynsym`. The port is correct for it
    /// anyway, because `dstW` is taken from `arg[1]` rather than from anything
    /// derived from `r0` — worth knowing, since that caller passes the GBM's own
    /// width where the other two pass the screen's.
    ///
    /// Those callers are also what makes the port small: **both have already
    /// clipped**. Each intersects the destination rectangle with the clip rect,
    /// returns early if it is empty, and folds the overhang back into
    /// `dx/dy/w/h/sx/sy` before dispatching. Which is why neither blitter reads
    /// the clip globals at all: only `trans` (GOT +0x6cc) and, for `Ex`, the
    /// blend table (GOT +0x894), the same two slots `host_rotate_blit` already
    /// resolves. So the walk here is a plain 1:1 rectangle — no clip test, no
    /// scaling, no accumulator.
    ///
    /// `CS_SCREEN_DEBUG=1` prints the rotation code, format, alpha and level of
    /// every call. That trace is the only way to know what the data actually
    /// asks for; over one session it is 55% code 0, 23% code 1, 21% the
    /// transposed 4/6/8, and 63% 4bpp.
    ///
    /// The `Ex` blend is [`Blend565`], the same host code the rotate blitter has
    /// already been A/B'd on pixel for pixel.
    pub(crate) fn host_draw_screen(&mut self, ex: bool) -> Result<bool, String> {
        let entry = self.cpu.reg_get(Mode::User, reg::PC);
        let sp = self.cpu.reg_get(Mode::User, reg::SP);
        // Stack argument N at sp + 4N: this runs at the entry point, before the
        // guest's own `push`/`sub sp`.
        let arg = |vm: &mut Self, i: u32| vm.mem.r32(sp + 4 * i);

        let dst_p = self.cpu.reg_get(Mode::User, 0);
        let dx = self.cpu.reg_get(Mode::User, 1) as i32;
        let dy = self.cpu.reg_get(Mode::User, 2) as i32;
        let w = self.cpu.reg_get(Mode::User, 3) as i32;
        let h = arg(self, 0) as i32;
        let dst_w = arg(self, 1) as i32;
        let img_p = arg(self, 3);
        let (sx, sy) = (arg(self, 4) as i32, arg(self, 5) as i32);
        let code = arg(self, 8);
        let (alpha, level) = if ex { (arg(self, 9) as i32, arg(self, 10)) } else { (0xff, 0) };

        // The guest's own first test (`cmp r0,#0 / cmpne ip,#0`).
        if dst_p == 0 || img_p == 0 {
            return Ok(false);
        }
        // `CS_SCREEN_DEBUG=1`. Which rotation codes and pixel formats actually
        // turn up is the whole question for how much of this walk is worth
        // porting, and it is not answerable from the binary — the code comes
        // from `DM_DrawImg`'s (rot, angle) pair, which comes from the data.
        static SCREEN_DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *SCREEN_DEBUG.get_or_init(|| std::env::var("CS_SCREEN_DEBUG").is_ok()) {
            let fmt = self.mem.r32(img_p + 0x20);
            log::info!(
                "screen{} {w}x{h} @{dx},{dy} src {sx},{sy} code={code} alpha={alpha} level={level} fmt={:#x}",
                if ex { "Ex" } else { "" },
                self.mem.r8(fmt)
            );
        }
        // A degenerate extent is not worth reproducing: with `w < 0` the guest
        // skips the pair loop but `tst w,#1` still lets one tail pixel through
        // (0x3ec88). Nothing calls it that way, and declining is free.
        if w <= 0 || h <= 0 || dst_w <= 0 {
            return Ok(false);
        }
        // The nine rotation codes differ **only** in where each source pixel
        // lands: the source is walked `j = 0..h`, `i = 0..w` in every one of the
        // eighteen blocks, with the cursor advancing by `stride` per row. So the
        // whole difference is an affine destination index
        // `D(i, j) = d0 + j*dr + i*di`, in destination *pixels*.
        //
        // Codes 4, 6, 7 and 8 transpose — they write an `h` wide by `w` tall
        // rectangle. That is safe without a clip test here for the same reason
        // code 0 is: `DM_DrawImg` clips the transposed codes through a separate
        // block (0x47350) that tests `x + h` and `y + w` against the clip rect.
        //
        // Code 5 is **not** a transpose. `DM_DrawImg+0x3b4` emits it for
        // `rot == 0, angle == 0xb4`, and its block is 180° walked row-major —
        // bit-identical to code 3. `DM_DrawRotate565Ex` numbers that same
        // rotation 3, which is why the two functions' code tables must not be
        // shared.
        let (d0, dr, di) = match code {
            0 => (dst_w * dy + dx, dst_w, 1),
            1 => (dst_w * dy + dx + w - 1, dst_w, -1),
            2 => (dst_w * (dy + h - 1) + dx, -dst_w, 1),
            3 | 5 => (dst_w * (dy + h - 1) + dx + w - 1, -dst_w, -1),
            4 => (dst_w * dy + dx + h - 1, -1, dst_w),
            6 => (dst_w * (dy + w - 1) + dx, 1, -dst_w),
            7 => (dst_w * (dy + w - 1) + dx + h - 1, -1, -dst_w),
            8 => (dst_w * dy + dx, 1, dst_w),
            // The guest's compare chains are pure equality tests, so anything
            // outside 0..=8 returns without drawing (0x3ee10, 0x3efe4).
            _ => return Ok(false),
        };

        // `*(u8*)img->[0x20]`: 0xb is 4bpp paletted, 0xc is 8bpp.
        let img_data = self.mem.r32(img_p + 0x20);
        let fmt = self.mem.r8(img_data) as u32;
        if fmt != 0xb && fmt != 0xc {
            return Ok(false);
        }
        let four_bit = fmt == 0xb;

        // Globals, reached PC-relatively exactly as the guest does. The `add
        // r4, pc, r4` sits at a different offset in the two functions (+0x24 in
        // `DM_DrawScreen565`, +0x1c in `DM_DrawScreen565Ex`), which is the one
        // place a shared literal table would have been wrong.
        let (pc_off, l_got, l_trans, l_tbl) =
            if ex { (0x1c, 0xefc, 0xf00, 0xf04) } else { (0x24, 0xf48, 0xf4c, 0) };
        let got_off = self.mem.r32(entry + l_got);
        let got = entry.wrapping_add(pc_off).wrapping_add(got_off);
        let o_trans = self.mem.r32(entry + l_trans);
        let trans_p = self.mem.r32(got.wrapping_add(o_trans));
        if trans_p == 0 {
            return Ok(false);
        }
        let trans = self.mem.r32(trans_p);

        let mut blend = Blend565::new(alpha, level);
        if ex {
            // `DM_Initialize` builds `BlendingTable32Bit` as 16 planes of 32x32
            // (`mov r1,#0x10` at 0x4d938 into `DM_SetBlendingTable565`), and
            // 0x10 is the grayscale mode rather than a plane. The guest does not
            // bound this and would read whatever `.data` follows; declining is
            // the one response that is never a difference.
            if level > 0x10 {
                return Ok(false);
            }
            let o_tbl = self.mem.r32(entry + l_tbl);
            let table = self.mem.r32(got.wrapping_add(o_tbl));
            if !blend.gray && table == 0 {
                return Ok(false);
            }
            if !blend.load(&mut self.mem, table, level) {
                return Ok(false);
            }
        }

        // Source addressing, identical to the rotate blitter's: the 4bpp stride
        // rounds the width up to a whole byte, and `sx` folds into the base.
        let img_w = self.mem.r32(img_p) as i32;
        let head = (self.mem.r32(img_p + 0x14) as i32 + 4) * 2 + 1;
        let (stride, base) = if four_bit {
            let stride = (img_w + (img_w & 1)) >> 1;
            (stride, head + (sx >> 1) + stride * sy)
        } else {
            (img_w, head + sx + img_w * sy)
        };
        if stride <= 0 || base < 0 {
            return Ok(false);
        }

        // Byte offset within a source row of output column `c`. The base
        // already carries `sx`, so this is the distance from it.
        let col_byte = |c: i32| if four_bit { ((sx + c) >> 1) - (sx >> 1) } else { c };
        let span = (h - 1) as i64 * stride as i64 + col_byte(w - 1) as i64 + 1;
        if span <= 0 || span > i32::MAX as i64 {
            return Ok(false);
        }
        let src = match self.mem.slice(img_data.wrapping_add(base as u32), span as u32) {
            Some(b) => b.to_vec(),
            None => return Ok(false),
        };
        let span = span as u32;

        // ...and the destination rectangle, read-modify-written as one block.
        // `D` is affine in `i` and `j`, so its extremes are at the corners —
        // which is what makes even the transposed codes one contiguous span.
        let (dr, di) = (dr as i64, di as i64);
        let ext = |step: i64, n: i32| step * (n - 1) as i64;
        let (er, ei) = (ext(dr, h), ext(di, w));
        let d_lo = d0 as i64 + er.min(0) + ei.min(0);
        let d_hi = d0 as i64 + er.max(0) + ei.max(0);
        let dst_lo = d_lo * 2;
        let dst_len = (d_hi - d_lo + 1) * 2;
        if dst_lo < 0 || dst_len <= 0 || dst_len > i32::MAX as i64 {
            return Ok(false);
        }
        let dst_addr = dst_p.wrapping_add(dst_lo as u32);
        // The guest reads each source byte and writes its destination pixel
        // immediately; the host computes from a snapshot and writes back once.
        // Those are the same thing only if the two spans do not overlap. They
        // never do — a sprite is not its own target — but an overlap would
        // otherwise diverge silently, in frames nobody dumped. Same guard, same
        // reasoning, as `host_box_blur`.
        let src_addr = img_data.wrapping_add(base as u32);
        if src_addr < dst_addr.wrapping_add(dst_len as u32)
            && dst_addr < src_addr.wrapping_add(span)
        {
            return Ok(false);
        }
        let mut buf = match self.mem.slice(dst_addr, dst_len as u32) {
            Some(b) => b.to_vec(),
            None => return Ok(false),
        };

        // The palette copy the guest makes in its prologue, before it draws.
        // The caller reads it back, so it has to happen even though nothing
        // here needs it.
        let palette = self.mem.r32(img_p + 0x10);
        self.mem.write_bytes(img_p + 0x1c, &palette.to_le_bytes());
        let mut pal = [0u16; 256];
        let pal_n = if four_bit { 16 } else { 256 };
        match self.mem.slice(palette, pal_n as u32 * 2) {
            Some(b) => {
                for (i, e) in pal.iter_mut().enumerate().take(pal_n) {
                    *e = u16::from_le_bytes([b[i * 2], b[i * 2 + 1]]);
                }
            }
            None => return Ok(false),
        }

        for j in 0..h {
            let row = (j * stride) as usize;
            let out_row = (d0 as i64 + dr * j as i64 - d_lo) as usize * 2;
            for i in 0..w {
                let byte = src[row + col_byte(i) as usize];
                let idx = if !four_bit {
                    byte as usize
                } else if (sx + i) & 1 != 0 {
                    (byte & 0xf) as usize
                } else {
                    // Even *absolute* source column is the high nibble
                    // (0x3ec44), which is why `sx`'s parity has to be carried
                    // rather than the loop counter's.
                    (byte >> 4) as usize
                };
                let s = pal[idx] as u32;
                // `cmp r5, r4` compares the 16-bit palette entry against the
                // full 32-bit transparent word.
                if s == trans {
                    continue;
                }
                let o = (out_row as i64 + di * i as i64 * 2) as usize;
                let v = if ex {
                    let d = u16::from_le_bytes([buf[o], buf[o + 1]]) as u32;
                    blend.apply(s, d)
                } else {
                    // DM_DrawScreen565: `strhne r4, [r1]` — the palette entry.
                    s as u16
                };
                buf[o..o + 2].copy_from_slice(&v.to_le_bytes());
            }
        }
        self.mem.write_bytes(dst_addr, &buf);

        self.screen_host += 1;
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Host `DM_DrawImageRotateEx` (0x4b0f8) — the arbitrary-angle,
    /// arbitrary-scale blitter that draws the screen back onto itself.
    ///
    /// One caller, `HERO_Draw_Resize_Background+0x74`, and with the radial blur
    /// gone this is the most expensive thing left: 63.7% of a ~100 ms frame.
    /// What it draws is the off-screen scratch buffer (`_lcdG+8`, which
    /// `GameFrame_Render` has just drawn the map into) scaled about the screen
    /// centre onto the screen (`_lcdG+4`).
    ///
    /// **The cost is the walk itself, not a blend.** The call site builds
    /// `HERO_Draw_Resize_Background(hero[0x278], hero[0x27c], (s16)hero[0x282],
    /// 0xff)` — that last argument is a literal `mov r3, #0xff`, and the shim
    /// passes `level = 0` with it. So `alpha == 0xff && level == 0` always
    /// holds, the per-pixel `DM_ProcPixelBlending` call is never made, and every
    /// output pixel costs only its two accumulators, four clip compares and two
    /// offset multiplies. Measured: ~30 guest instructions per destination
    /// pixel, so 7.3 M for one 400x240 frame at 150% (`--resize 8100:150`).
    ///
    /// The blend arm is ported anyway because it is the function's own contract
    /// and it is free: `DM_ProcPixelBlending`'s 16bpp path (`_lcdG[0x8a] == 1`,
    /// `level <= 0xf`) is arithmetic for arithmetic the same code as
    /// [`Blend565`] — the same `level*1024 + s_ch*32 + d_ch` index into the same
    /// GOT slot (+0x894, where `BlendingTable32Bit` lives, the slot
    /// `host_rotate_blit` and `host_draw_screen` already resolve), green looked
    /// up in five bits and doubled back to six, the `alpha <= 0xfe` weighting
    /// with blue's un-incremented weight. So that half inherits the
    /// pixel-for-pixel A/B those two already carry, and the test below drives it
    /// directly rather than relying on the one caller to. The generic path — the
    /// one that leaves through the `DM_ByteToIntPixel` function pointers —
    /// declines, the same rule as `host_fill_alpha`.
    ///
    /// The angle is degrees, turned into a Q10 `(cos, sin)` off `sin1024`: 19
    /// entries, 5° apart, one quadrant each way. All four quadrant tests are
    /// **unsigned**, so an angle outside `0..=360` falls through every one of
    /// them and the guest blits with two uninitialised registers — declined
    /// rather than guessed at. The reduction above them only fires for
    /// `angle > 360`, so 360 is not folded to 0; that costs nothing, because the
    /// fourth quadrant yields the same pair.
    ///
    /// `sx`/`sy` never offset the source read. They appear only as the
    /// reflection bases `sx + w - 1` and `sy + h - 1` for the mirror codes
    /// (`rot` 4 flips Y, 8 flips X, 0xc both). That is the shipped behaviour,
    /// and the one caller passes 0 for both anyway.
    pub(crate) fn host_image_rotate(&mut self) -> Result<bool, String> {
        let t0 = std::time::Instant::now();
        let sp = self.cpu.reg_get(Mode::User, reg::SP);
        // Stack argument N at sp + 4N: this runs at the entry point, before the
        // guest's own `push`/`sub sp`.
        let mut arg = [0u32; 11];
        for (i, slot) in arg.iter_mut().enumerate() {
            *slot = self.mem.r32(sp + 4 * i as u32);
        }
        let dst_p = self.cpu.reg_get(Mode::User, 0);
        let x = self.cpu.reg_get(Mode::User, 1) as i32;
        let y = self.cpu.reg_get(Mode::User, 2) as i32;
        let w = self.cpu.reg_get(Mode::User, 3) as i32;
        let h = arg[0] as i32;
        let img_p = arg[1];
        let (sx, sy) = (arg[2] as i32, arg[3] as i32);
        // `rot`, `level` and `alpha` arrive through `ldrb`.
        let rot = arg[4] & 0xff;
        let angle = arg[5] as i32;
        let (scale_x, scale_y) = (arg[6] as i32, arg[7] as i32);
        let level = arg[8] & 0xff;
        let alpha = (arg[9] & 0xff) as i32;
        let anchor = arg[10];

        // The guest's own two early-outs. `dst_p == 0` is not one of them — that
        // faults inside `DM_GET_FRAME_BUFFER_INFO` — so it declines too, and the
        // interpreter reproduces the fault where it belongs.
        if img_p == 0 || alpha == 0 || scale_x == 0 || scale_y == 0 || dst_p == 0 {
            return Ok(false);
        }
        let (Some(sin_t), Some(lcd_g)) = (self.symbol("sin1024"), self.symbol("_lcdG")) else {
            return Ok(false);
        };

        let mut deg = angle;
        if deg > 360 {
            deg -= (deg / 360) * 360;
        }
        let mut tbl = [0i32; 19];
        for (k, e) in tbl.iter_mut().enumerate() {
            *e = self.mem.r16(sin_t.wrapping_add(2 * k as u32)) as i32;
        }
        let quadrant = |lo: i32| -> Option<usize> {
            let d = deg.wrapping_sub(lo);
            ((d as u32) <= 90).then(|| (d / 5) as usize)
        };
        let (cos, sin) = if let Some(k) = quadrant(0) {
            (tbl[18 - k], tbl[k])
        } else if let Some(k) = quadrant(90) {
            (-tbl[k], tbl[18 - k])
        } else if let Some(k) = quadrant(180) {
            (-tbl[18 - k], -tbl[k])
        } else if let Some(k) = quadrant(270) {
            (tbl[k], -tbl[18 - k])
        } else {
            return Ok(false);
        };

        // `DM_GET_FRAME_BUFFER_INFO`, inlined twice: bpp is pitch / width (kept
        // in a byte), and the clip bounds are the destination's own w and h.
        let src = FrameBuffer::read(&mut self.mem, img_p);
        let dst = FrameBuffer::read(&mut self.mem, dst_p);
        if src.w <= 0 || dst.w <= 0 || src.pitch <= 0 || dst.pitch <= 0 {
            return Ok(false);
        }
        let src_bpp = (src.pitch / src.w) & 0xff;
        let dst_bpp = (dst.pitch / dst.w) & 0xff;
        if src_bpp <= 0 || dst_bpp <= 0 {
            return Ok(false);
        }

        // The scaled destination extent, and the source step per output pixel.
        // Both divisions run before the guest tests the height, so a zero extent
        // is a guest divide-by-zero and not an empty blit.
        let dw = w.wrapping_mul(scale_x) / 100;
        let dh = h.wrapping_mul(scale_y) / 100;
        if dw == 0 || dh == 0 {
            return Ok(false);
        }
        // The guest adds one to each step right after dividing.
        let xstep = w.wrapping_shl(10).wrapping_div(dw).wrapping_add(1);
        let ystep = h.wrapping_shl(10).wrapping_div(dh).wrapping_add(1);

        // `DM_CalcAnchor(&ax, &ay, dw, dh, anchor)`, inlined. Both start at 0,
        // and the guest tests the bits in this order, taking at most one each.
        let (mut ax, mut ay) = (0i32, 0i32);
        if anchor & 0x10 != 0 {
            ax -= dw >> 1;
        } else if anchor & 0x20 != 0 {
            ax -= dw;
        }
        if anchor & 2 != 0 {
            ay -= dh >> 1;
        } else if anchor & 4 != 0 {
            ay -= dh;
        }
        let x0 = x.wrapping_add(ax.wrapping_mul(cos).wrapping_sub(ay.wrapping_mul(sin)) >> 10);
        let y0 = y.wrapping_add(ay.wrapping_mul(cos).wrapping_add(ax.wrapping_mul(sin)) >> 10);

        // A negative height returns before the walk; a negative width leaves
        // every row empty. Both draw nothing, which is what serving them means.
        if dh < 0 || dw < 0 {
            return self.imgrot_done(t0);
        }

        // `sx`/`sy` enter only here.
        let src_right = sx.wrapping_add(w).wrapping_sub(1);
        let src_bottom = sy.wrapping_add(h).wrapping_sub(1);
        let flip_x = rot == 8 || rot == 0xc;
        let flip_y = rot == 4 || rot == 0xc;
        let src_ix = |i: i32| -> i32 {
            let s = i.wrapping_mul(xstep) >> 10;
            if flip_x {
                src_right.wrapping_sub(s)
            } else {
                s
            }
        };
        let src_iy = |j: i32| -> i32 {
            let s = j.wrapping_mul(ystep) >> 10;
            if flip_y {
                src_bottom.wrapping_sub(s)
            } else {
                s
            }
        };
        // The guest accumulates these a row and a column at a time; written
        // multiplicatively they are identical mod 2^32.
        let dest_x = |i: i32, j: i32| {
            x0.wrapping_add(i.wrapping_mul(cos).wrapping_sub(j.wrapping_mul(sin)) >> 10)
        };
        let dest_y = |i: i32, j: i32| {
            y0.wrapping_add(j.wrapping_mul(cos).wrapping_add(i.wrapping_mul(sin)) >> 10)
        };

        // `alpha == 0xff && level == 0` copies the pixel whole — at the *source*
        // bpp, which is what picks `ldrh`/`strh` over `ldr`/`str`. Anything else
        // is one `DM_ProcPixelBlending` per pixel.
        let plain = alpha == 0xff && level == 0;
        let read_w = if src_bpp == 2 { 2i64 } else { 4 };
        let mut blend = Blend565::new(alpha, level);
        if !plain {
            if level > 0xf
                || src_bpp != 2
                || dst_bpp != 2
                || self.mem.r8(lcd_g.wrapping_add(0x8a)) != 1
            {
                return Ok(false);
            }
            let Some(table) = self.symbol("BlendingTable32Bit") else {
                return Ok(false);
            };
            // `level <= 0xf` here, so `Blend565`'s `level == 0x10` grayscale arm
            // — which belongs to the other blenders, not to this one — is
            // unreachable, and `load` really does copy a table block.
            if !blend.load(&mut self.mem, table, level) {
                return Ok(false);
            }
        }

        // Source and destination bytes actually touched, each as one span. Both
        // index maps are monotonic in their loop counter, and each byte offset
        // is a positive-weighted sum of the two, so the endpoints bound it.
        let (ia, ib) = (0, dw - 1);
        let (ja, jb) = (0, dh - 1);
        let (cx0, cx1) = (src_ix(ia), src_ix(ib));
        let (cy0, cy1) = (src_iy(ja), src_iy(jb));
        let s_lo = src_bpp as i64 * cx0.min(cx1) as i64 + src.pitch as i64 * cy0.min(cy1) as i64;
        let s_hi =
            src_bpp as i64 * cx0.max(cx1) as i64 + src.pitch as i64 * cy0.max(cy1) as i64 + read_w;
        if s_lo < 0 || s_hi <= s_lo || s_hi - s_lo > u32::MAX as i64 {
            return Ok(false);
        }
        let src_addr = src.pixels.wrapping_add(s_lo as u32);
        let s_len = (s_hi - s_lo) as u32;
        let sbuf = match self.mem.slice(src_addr, s_len) {
            Some(b) => b.to_vec(),
            None => return Ok(false),
        };

        // The destination is affine in (i, j), so its extremes are at the four
        // corners; clipping then folds it to what the guest will actually write.
        let xs = [dest_x(ia, ja), dest_x(ib, ja), dest_x(ia, jb), dest_x(ib, jb)];
        let ys = [dest_y(ia, ja), dest_y(ib, ja), dest_y(ia, jb), dest_y(ib, jb)];
        let bx0 = xs.iter().copied().min().unwrap_or(0).max(0);
        let bx1 = xs.iter().copied().max().unwrap_or(0).min(dst.w - 1);
        let by0 = ys.iter().copied().min().unwrap_or(0).max(0);
        let by1 = ys.iter().copied().max().unwrap_or(0).min(dst.h - 1);
        if bx0 > bx1 || by0 > by1 {
            return self.imgrot_done(t0);
        }
        let d_lo = dst_bpp as i64 * bx0 as i64 + dst.pitch as i64 * by0 as i64;
        let d_hi = dst_bpp as i64 * bx1 as i64 + dst.pitch as i64 * by1 as i64 + read_w;
        if d_lo < 0 || d_hi <= d_lo || d_hi - d_lo > u32::MAX as i64 {
            return Ok(false);
        }
        let dst_addr = dst.pixels.wrapping_add(d_lo as u32);
        let d_len = (d_hi - d_lo) as u32;

        // The source is snapshotted, so an overlap is the one case where this
        // would differ from the guest's in-place walk. The two are different
        // framebuffers (`_lcdG+8` and `_lcdG+4`), so it never happens.
        if src_addr < dst_addr.wrapping_add(d_len) && dst_addr < src_addr.wrapping_add(s_len) {
            return Ok(false);
        }
        let Some(dbuf) = self.mem.slice_mut(dst_addr, d_len) else {
            return Ok(false);
        };

        let n = read_w as usize;
        for j in 0..dh {
            let row = src_iy(j) as i64 * src.pitch as i64;
            for i in 0..dw {
                let dx = dest_x(i, j);
                let dy = dest_y(i, j);
                // The guest's four clip compares, in its order and all signed.
                if dy < 0 || dx < 0 || dx >= dst.w || dy >= dst.h {
                    continue;
                }
                let so = (src_bpp as i64 * src_ix(i) as i64 + row - s_lo) as usize;
                let dof =
                    (dst_bpp as i64 * dx as i64 + dst.pitch as i64 * dy as i64 - d_lo) as usize;
                if plain {
                    dbuf[dof..dof + n].copy_from_slice(&sbuf[so..so + n]);
                    continue;
                }
                let s = u16::from_le_bytes([sbuf[so], sbuf[so + 1]]) as u32;
                let d = u16::from_le_bytes([dbuf[dof], dbuf[dof + 1]]) as u32;
                let out = blend.apply(s, d);
                dbuf[dof..dof + 2].copy_from_slice(&out.to_le_bytes());
            }
        }

        if self.trace {
            log::debug!(
                "imgrot {w}x{h}@{scale_x}/{scale_y}% angle={angle} rot={rot} \
                 -> {dw}x{dh} at {x0},{y0} level={level} alpha={alpha}"
            );
        }
        self.imgrot_done(t0)
    }

    /// Served, however many pixels survived the clip: count it, charge the wall
    /// clock like `rot_ns` does, and return to the caller.
    fn imgrot_done(&mut self, t0: std::time::Instant) -> Result<bool, String> {
        self.imgrot_host += 1;
        self.imgrot_ns += t0.elapsed().as_nanos() as u64;
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }
}

/// Per-pixel polar coordinates for the radial blur.
///
/// `Sqrt` and `_atan2` are the expensive half of that function — a 15-step
/// restoring square root and a divide plus a binary search, per output pixel —
/// and **neither depends on the animation counter**. The shipped code redoes
/// both every frame because it has nowhere to keep them. Hoisting them into a
/// map that outlives the frame is worth about 55% of the port's per-frame cost
/// (5.9 ms -> 2.6 ms at 520x240, measured), which is what makes an effect that
/// runs for two seconds cost what five frames used to.
///
/// Keyed on the surface size alone: the only other inputs are `nTan` and the
/// arithmetic in `guest_sqrt`/`guest_atan2`, and `nTan` is `.rodata` this game
/// never writes.
pub(crate) struct RadialMap {
    pub(crate) w: i32,
    pub(crate) h: i32,
    /// Where the rays converge. `(w>>1, h>>1)` is the guest's; the boss's
    /// position is ours. Part of the key, so moving it rebuilds the map — which
    /// is once per kill, because the centre is fixed for the effect's life.
    pub(crate) cx: i32,
    pub(crate) cy: i32,
    /// `Sqrt(w*w + h*h)` — the divisor the ray length is scaled by.
    pub(crate) diag: i32,
    /// `(radius, angle)` per pixel, row-major, the angle already folded into
    /// 0..359 the way the blur's own `if angle < 0 { angle += 360 }` does. Four
    /// bytes a pixel, so a quarter of a megabyte at 520x240.
    pub(crate) polar: Vec<(u16, u16)>,
}

impl RadialMap {
    /// `None` when the surface is one the blur should decline: a zero diagonal
    /// (the guest would call `__div0`), a size too big to be a framebuffer, or
    /// a radius or angle that will not index what it is about to index.
    fn build(w: i32, h: i32, cx: i32, cy: i32, n_tan: &[i32]) -> Option<Self> {
        let diag = guest_sqrt((w.wrapping_mul(w).wrapping_add(h.wrapping_mul(h))) as u32) as i32;
        let count = (w as i64).checked_mul(h as i64)?;
        if diag <= 0 || count <= 0 || count > (16 << 20) {
            return None;
        }
        let mut polar = Vec::with_capacity(count as usize);
        for y in 0..h {
            let dy = cy - y;
            let dy2 = dy.wrapping_mul(dy);
            for x in 0..w {
                let dx = cx - x;
                let radius = guest_sqrt(dx.wrapping_mul(dx).wrapping_add(dy2) as u32) as i32;
                let mut angle = guest_atan2(dy, dx, n_tan);
                if angle < 0 {
                    angle += 360;
                }
                // Guaranteed by `_atan2`'s own arithmetic, but the angle
                // indexes a table and the radius is multiplied by a counter, so
                // both are checked once here instead of per frame.
                if !(0..360).contains(&angle) || !(0..=u16::MAX as i32).contains(&radius) {
                    return None;
                }
                polar.push((radius as u16, angle as u16));
            }
        }
        Some(Self { w, h, cx, cy, diag, polar })
    }
}

/// The guest's `Sqrt` (0xeec14): a 15-step restoring integer square root.
///
/// Transcribed step for step rather than replaced with `(x as f64).sqrt()`,
/// because the two are not the same function. This one is exact only where the
/// restoring algorithm happens to be, it takes its argument as unsigned, and
/// the prologue's `cmn r0, #0xc0000001` folds the top quarter of the range
/// (`x >= 0x4000_0000`) down by hand before the digit loop starts. Every
/// output pixel of the radial blur depends on it twice over, through the
/// radius and through the ray length, so a one-off would be visible.
fn guest_sqrt(x: u32) -> u32 {
    // HI on `cmn r0, #0xc0000001` is exactly `x >= 0x4000_0000`.
    let (mut rem, mut sub, mut root) = if x >= 0x4000_0000 {
        (x.wrapping_add(0xc000_0000), 0x5000_0000u32, 0x8000u32)
    } else {
        (x, 0x1000_0000u32, 0u32)
    };
    // (bit added on this compare, then the shift/offset that builds the next
    // trial value out of the running root).
    const STEPS: [(u32, u32, u32); 14] = [
        (0x4000, 0xe, 0x400_0000),
        (0x2000, 0xd, 0x100_0000),
        (0x1000, 0xc, 0x40_0000),
        (0x0800, 0xb, 0x10_0000),
        (0x0400, 0xa, 0x4_0000),
        (0x0200, 0x9, 0x1_0000),
        (0x0100, 0x8, 0x4000),
        (0x0080, 0x7, 0x1000),
        (0x0040, 0x6, 0x0400),
        (0x0020, 0x5, 0x0100),
        (0x0010, 0x4, 0x0040),
        (0x0008, 0x3, 0x0010),
        (0x0004, 0x2, 0x0004),
        (0x0002, 0x1, 0x0001),
    ];
    for (bit, sh, delta) in STEPS {
        if rem >= sub {
            root += bit;
            rem -= sub;
        }
        sub = root.wrapping_shl(sh).wrapping_add(delta);
    }
    if rem >= sub {
        root += 1;
    }
    root
}

/// The guest's `BinarySearch` (0x49d5c).
///
/// Not `slice::binary_search`: on a miss this returns the **last midpoint it
/// looked at**, which is a specific wrong-ish index the caller then treats as
/// an angle. Rust's would return an insertion point, one off from this about
/// half the time.
fn guest_binary_search(tbl: &[i32], key: i32, n: i32) -> i32 {
    let mut hi = n - 1;
    let mut lo = 0i32;
    let mut mid = 0i32;
    if hi < 0 {
        return mid;
    }
    loop {
        let s = hi.wrapping_add(lo);
        // `add r3, r3, r3, lsr #31; asr r3, r3, #1` — a halving that rounds
        // toward zero, which differs from `>> 1` once the sum goes negative.
        mid = s.wrapping_add(((s as u32) >> 31) as i32) >> 1;
        let v = tbl.get(mid as usize).copied().unwrap_or(0);
        if v < key {
            lo = mid + 1;
        } else if v != key {
            hi = mid - 1;
        } else {
            return mid;
        }
        if lo > hi {
            return mid;
        }
    }
}

/// The guest's `_atan2` (0x49da8) — degrees, not radians.
///
/// `atan2(y, x)` by way of `(y << 10) / x` looked up in `nTan`, then four
/// quadrant fixups with the constants the original uses (+91, −269, −89). The
/// result lands in −269..=270 and the caller folds it into 0..359.
fn guest_atan2(y: i32, x: i32, n_tan: &[i32]) -> i32 {
    if x == 0 {
        return if y < 0 {
            -90
        } else if y > 0 {
            90
        } else {
            0
        };
    }
    let t = y.wrapping_shl(10).wrapping_div(x);
    if t < n_tan.first().copied().unwrap_or(0) {
        // Below the table: straight up or straight down. Note this tests the
        // sign of `x` *before* the sign bit is extracted, so it reads the
        // opposite way round to the fixups below — as the original does.
        return if x <= 0 { 90 } else { -90 };
    }
    let idx = guest_binary_search(n_tan, t, 180);
    if y >= 0 && x < 0 {
        idx + 91
    } else if y <= 0 && x < 0 {
        idx - 269
    } else {
        idx - 89
    }
}

#[allow(clippy::too_many_arguments)]
fn run_blit16(
    mem: &mut GuestMem,
    dst: FrameBuffer,
    dx: i32,
    dy0: i32,
    w0: i32,
    h0: i32,
    src: FrameBuffer,
    sx: i32,
    sy0: i32,
    ctx: &Ctx,
    use_trans: bool,
) {
    // ---- the prologue, transcribed ------------------------------------
    let (mut w, mut sx, mut sy) = (w0, sx, sy0);
    if sx < 0 {
        w += sx;
        sx = 0;
    }
    if sy < 0 {
        // ★ Yes, the *width*. The original adds the y underflow to `w`, not to
        // `h` — a bug in the shipped binary. It is reproduced here on purpose:
        // this function must be indistinguishable from the code it replaces,
        // and callers clip beforehand so the path is effectively dead.
        w += sy;
        sy = 0;
    }
    let mut h = h0;
    if dx + w > dst.w {
        w = dst.w - dx;
    }
    if dy0 + h > dst.h {
        h = dst.h - dy0;
    }
    if h <= 0 || w <= 0 {
        return;
    }

    let alpha = ctx.alpha;
    let blend = alpha <= 0xfe;
    let a = alpha.clamp(0, 255) as u32;

    for row in 0..h {
        let dy = dy0 + row;
        let sy_r = sy + row;

        // Everything that does not depend on the column, lifted out of the
        // inner loop. This is the whole optimisation: the guest re-tested all
        // of it per pixel.
        if sy_r < 0 || sy_r >= src.h {
            continue;
        }
        if dy < 0 || dy >= dst.h {
            continue;
        }
        if ctx.clip_t > dy || ctx.clip_b <= dy {
            continue;
        }

        // Column range where every remaining test passes.
        let lo = 0.max(-sx).max(-dx).max(ctx.clip_l - dx);
        let hi = w.min(src.w - sx).min(dst.w - dx).min(ctx.clip_r - dx);
        if hi <= lo {
            continue;
        }

        let src_row = src.pixels.wrapping_add((sy_r * src.pitch) as u32);
        let dst_row = dst.pixels.wrapping_add((dy * dst.pitch) as u32);

        for col in lo..hi {
            let sa = src_row.wrapping_add(((sx + col) * 2) as u32);
            let s = mem.r16(sa) as u32;
            if use_trans && s == ctx.trans {
                continue;
            }
            let da = dst_row.wrapping_add(((dx + col) * 2) as u32);
            let v = if blend { blend565(mem.r16(da) as u32, s, a) } else { s };
            mem.w16(da, v as u16);
        }
    }
}

// ---- 그리기 리프 다섯 — `CS_NO_MAPLEAF` / `CS_NO_ANCHOR` / `CS_NO_PAL` -----
//
// By 2026-09-23 the expensive *leaves* were already host — the two screen
// blitters, the two rotate blitters, the font stack, both blurs, DES. What a
// combat profile shows instead is **dispatch**: the tile and object walk, and
// the little helpers it calls once per tile or per sprite. Each is thirty or
// forty guest instructions, which is nothing until you count the calls.
//
// Measured on an in-game replay with six monsters (11,000 frames, 497 M guest
// instructions), by share of sampled PCs:
//
// | | |
// |---|---|
// | `SPR_drawSprite` | 12.5% |
// | `Map_MiddleObjectDraw` | 10.6% |
// | `Map_checkTileToRectNDraw_TopObj` | 8.0% |
// | `DM_DrawImg` | 6.7% |
// | **`MapClip`** | **5.3%** |
// | `Map_drawPartTile` | 5.0% |
// | `DM_SetChagePalBlendEx` | 3.9% |
// | **`DM_CalcAnchorGbm`** | **2.2%** |
// | **`DM_RestorePalette`** | **2.1%** |
// | **`Map_getTileIndex`** | **1.7%** |
//
// The four in bold are the ones ported here, together 11.3%. They were chosen
// for being *leaves*: each has a single observable effect — four globals, one
// returned byte, two `int` out-parameters, one palette array — so a host port
// can be checked against the shipped ARM code exactly, which is what the tests
// beside this do. The walk itself (`SPR_drawSprite`, `Map_*Draw*`, 57%) is the
// game's scene graph and is left alone.
//
// `DM_CalcAnchorGbm` is a special case worth naming. `host_rotate_blit` already
// had this arithmetic transcribed inline, because its own A/B is what proved the
// transcription right; porting the function would have made a second copy. So
// the two now **share** [`calc_anchor_gbm`], which means the oracle test beside
// this covers the rotate blitter's anchor as well, and the guest's own six
// callers — `DM_DrawImg`, `DM_DrawImgEx`, `Map_ImageLoad`, `MAPOBJ_drawTopMiddle`,
// `MAPOBJ_drawClipFrontObj`, `SprObject_drawBottom` — get it for two loads.

/// `MapClip(x1, y1, x2, y2)` — set the four map clip globals, clamping the near
/// edges up to 0 and the far ones down to the on-screen surface.
///
/// The shipped body stores the raw value *and then* the clamp (`str` followed by
/// a conditional `str`), which no one can observe between, so the final state is
/// `max(0)` and `min(screen)` on signed values.
const MAP_CLIP_WORDS: [(u32, u32); 12] = [
    (0x018, 0xe350_0000), // cmp   r0,#0
    (0x024, 0xb3a0_0000), // movlt r0,#0            x1 clamped up to 0
    (0x02c, 0xe351_0000), // cmp   r1,#0
    (0x038, 0xb3a0_0000), // movlt r0,#0            y1 too
    (0x040, 0xb581_0000), // strlt r0,[r1]
    (0x05c, 0xe590_004c), // ldr   r0,[r0,#0x4c]    surface width
    (0x064, 0xe152_0000), // cmp   r2,r0
    (0x06c, 0xc582_0000), // strgt r0,[r2]          x2 clamped down to it
    (0x074, 0xe592_2050), // ldr   r2,[r2,#0x50]    surface height
    (0x078, 0xe153_0002), // cmp   r3,r2
    (0x080, 0xc583_2000), // strgt r2,[r3]          y2 too
    (0x088, 0xe12f_ff1e), // bx    lr
];

/// `_lcdG[0x8a]` — the kernel's bits-per-pixel flag, `1` for the 16bpp kernel
/// every ported blitter in this file requires. The surface it clamps against is
/// [`LCD_W`]/[`LCD_H`], the pair the hit-testing and the box blur already use.
const LCD_BPP16: u32 = 0x8a;

/// `Map_getTileIndex(layer, off)` — `((u8*)_map[layer + 0xaa])[off]`, and on the
/// way past it copies that pointer to `_map[layer + 0xae]`.
///
/// The copy is not a cache this can skip: it is a word of guest state, and the
/// only way to know nothing reads it is to keep writing it.
const MAP_TILE_WORDS: [(u32, u32); 6] = [
    (0x010, 0xe280_20aa), // add  r2,r0,#0xaa
    (0x014, 0xe280_00ae), // add  r0,r0,#0xae
    (0x018, 0xe79c_2102), // ldr  r2,[ip,r2,lsl #2]   the layer's tile array
    (0x01c, 0xe78c_2100), // str  r2,[ip,r0,lsl #2]   and that copy
    (0x020, 0xe7d2_0001), // ldrb r0,[r2,r1]
    (0x024, 0xe12f_ff1e), // bx   lr
];
const MAP_TILE_LAYER: u32 = 0xaa;
const MAP_TILE_COPY: u32 = 0xae;

/// `DM_CalcAnchorGbm(&x, &y, w, h, rot, angle, anchor)` — move a blit's origin
/// from the anchor the caller asked for to the top-left the blitters want.
///
/// **Seven arguments, and the fifth is never read.** The shipped body pushes
/// three registers and then loads `[sp+0x10]`/`[sp+0x14]`, which is `[sp+4]` and
/// `[sp+8]` as the caller left them — and `DM_DrawImg+0x50` does
/// `stm sp,{r6,r8,ip}`, three stack words. Reading the first two would put the
/// angle where the rotation is.
///
/// `0x10`/`0x20` are centre/right on x and `2`/`4` centre/bottom on y, each pair
/// tested in that order so the lower bit wins. Then, for a quarter turn only
/// (`0x5a` or `0x10e` degrees — the second written as `0x10c + 2`), whichever
/// axis was *centred* gets the difference of the half-extents added back,
/// because the blit is about to come out `h` wide and `w` tall.
///
/// Every shift is arithmetic, so a negative extent moves the origin the other
/// way; `i32 >> 1` in Rust is the same instruction.
const ANCHOR_WORDS: [(u32, u32); 15] = [
    (0x004, 0xe59d_c014), // ldr   ip,[sp,#0x14]   anchor
    (0x008, 0xe59d_5010), // ldr   r5,[sp,#0x10]   angle
    (0x00c, 0xe21c_4010), // ands  r4,ip,#0x10     x centred
    (0x018, 0xe046_60c2), // sub   r6,r6,r2,asr #1
    (0x020, 0xe21c_6002), // ands  r6,ip,#2        y centred
    (0x034, 0xe355_005a), // cmp   r5,#0x5a        90
    (0x03c, 0xe3a0_cf43), // mov   ip,#0x10c
    (0x040, 0xe28c_c002), // add   ip,ip,#2        270
    (0x044, 0xe155_000c), // cmp   r5,ip
    (0x05c, 0x108c_c0c2), // addne ip,ip,r2,asr #1
    (0x060, 0x104c_c0c3), // subne ip,ip,r3,asr #1
    (0x074, 0x1062_30c3), // rsbne r3,r2,r3,asr #1
    (0x084, 0xe31c_0004), // tst   ip,#4           y bottom
    (0x098, 0xe31c_0020), // tst   ip,#0x20        x right
    (0x0a0, 0x1062_6006), // rsbne r6,r2,r6
];
/// The anchor correction itself, shared by the hook and `host_rotate_blit`.
pub(crate) fn calc_anchor_gbm(
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    angle: u32,
    anchor: u32,
) -> (i32, i32) {
    let (x_mid, y_mid) = (anchor & ANCHOR_X_MID != 0, anchor & ANCHOR_Y_MID != 0);
    let mut x = x;
    let mut y = y;
    if x_mid {
        x -= w >> 1;
    } else if anchor & ANCHOR_X_END != 0 {
        x -= w;
    }
    if y_mid {
        y -= h >> 1;
    } else if anchor & ANCHOR_Y_END != 0 {
        y -= h;
    }
    // A quarter turn swaps the extents, so a centred axis is re-centred against
    // the one it is about to be measured by.
    if ANCHOR_QUARTER.contains(&angle) {
        if x_mid {
            x += (w >> 1) - (h >> 1);
        }
        if y_mid {
            y += (h >> 1) - (w >> 1);
        }
    }
    (x, y)
}

const ANCHOR_X_MID: u32 = 0x10;
const ANCHOR_X_END: u32 = 0x20;
const ANCHOR_Y_MID: u32 = 2;
const ANCHOR_Y_END: u32 = 4;
/// Compared for equality only, exactly as `cmp r5,#0x5a` does, so the sign of
/// the angle never comes into it.
const ANCHOR_QUARTER: [u32; 2] = [0x5a, 0x10e];

/// `DM_RestorePalette(obj)` — put `obj`'s live palette back from the resource
/// blob it was loaded from, and clear the "already blended" flag.
///
/// The object's fields, all read out of these words: `+0xc` the blob, `+0x10`
/// the live palette, `+0x14` how many entries, `+0x1c`/`+0x20` cursors the
/// shipped loop keeps re-reading, `+0x24` the flag. Entries live in the blob as
/// little-endian halfwords from **offset 9** — an odd offset, which is why the
/// loop reads the two bytes separately.
const RESTORE_PAL_WORDS: [(u32, u32); 15] = [
    (0x014, 0xe5d0_2024), // ldrb r2,[r0,#0x24]   the flag
    (0x020, 0xe590_200c), // ldr  r2,[r0,#0xc]    the blob
    (0x030, 0xe580_2020), // str  r2,[r0,#0x20]
    (0x038, 0xe5d3_308a), // ldrb r3,[r3,#0x8a]   16bpp only
    (0x03c, 0xe353_0001), // cmp  r3,#1
    (0x048, 0xe5c0_3024), // strb r3,[r0,#0x24]   cleared on every exit
    (0x054, 0xe590_1010), // ldr  r1,[r0,#0x10]   the live palette
    (0x060, 0xe590_6014), // ldr  r6,[r0,#0x14]   entries
    (0x064, 0xe580_101c), // str  r1,[r0,#0x1c]
    (0x070, 0xe3a0_c00a), // mov  ip,#0xa         the blob's first high byte
    (0x084, 0xe7f2_500c), // ldrb r5,[r2,ip]!
    (0x090, 0xe552_2001), // ldrb r2,[r2,#-1]     …and the low one before it
    (0x098, 0xe28c_c002), // add  ip,ip,#2
    (0x09c, 0xe182_2405), // orr  r2,r2,r5,lsl #8
    (0x0a0, 0xe181_20b4), // strh r2,[r1,r4]
];
const PAL_BLOB: u32 = 0xc;
const PAL_LIVE: u32 = 0x10;
const PAL_COUNT: u32 = 0x14;
const PAL_CURSOR: u32 = 0x1c;
const PAL_BLOB_CURSOR: u32 = 0x20;
const PAL_DONE: u32 = 0x24;
const PAL_BLOB_FIRST: u32 = 9;

/// `MC_grpPutPixel(gbm, x, y, ctx)` — one pixel, clipped twice.
///
/// The clip is the surface (`gbm[0]`/`gbm[4]`) **and** the context's rectangle
/// (`ctx[4]`/`ctx[8]` near, `ctx[0xc]`/`ctx[0x10]` far), all signed, with a
/// combined sign test first so one negative coordinate rejects both.
///
/// **It calls two kernel accessors per pixel and throws both results away.**
/// `MIDASKernelManager::getInstance` is `return *global` and `getKernel` is a
/// bare `bx lr`; `r0` is reloaded from `gbm[0xc]` immediately after. So skipping
/// them is not a shortcut — there is nothing there.
///
/// Served for the 16bpp kernel with no per-pixel hook installed
/// (`ctx[0x2c] == 0`), opaque or not; `ctx[0x20] > 0xfe` stores the colour and
/// anything less goes through [`PIXEL_OP_WORDS`]. Declined: 32bpp, and a
/// `ctx[0x2c]` that is set — the same rule `host_fill_alpha` uses for
/// `DM_ByteToIntPixel`. A bpp that is neither draws nothing, which is the
/// shipped answer and is reproduced rather than declined.
const PUT_PIXEL_WORDS: [(u32, u32); 21] = [
    (0x01c, 0xe197_3004), // orrs r3,r7,r4        x|y, for the sign test
    (0x020, 0xe596_000c), // ldr  r0,[r6,#0xc]    bpp — and what the two calls
    (0x028, 0xe596_3000), // ldr  r3,[r6]         returned is gone by here
    (0x034, 0xe596_3004), // ldr  r3,[r6,#4]
    (0x040, 0xe595_3004), // ldr  r3,[r5,#4]      clip x1
    (0x04c, 0xe595_3008), // ldr  r3,[r5,#8]      clip y1
    (0x058, 0xe595_300c), // ldr  r3,[r5,#0xc]    clip x2
    (0x064, 0xe595_3010), // ldr  r3,[r5,#0x10]   clip y2
    (0x070, 0xe350_0010), // cmp  r0,#0x10
    (0x074, 0xe596_1010), // ldr  r1,[r6,#0x10]   pixels
    (0x07c, 0xe350_0020), // cmp  r0,#0x20
    (0x0c8, 0xe596_c008), // ldr  ip,[r6,#8]      pitch, in bytes
    (0x0cc, 0xe595_3020), // ldr  r3,[r5,#0x20]   alpha
    (0x0d0, 0xe595_6014), // ldr  r6,[r5,#0x14]   colour
    (0x0d4, 0xe02c_1c97), // mla  ip,r7,ip,r1
    (0x0d8, 0xe353_00fe), // cmp  r3,#0xfe
    (0x0dc, 0xc08c_4084), // addgt r4,ip,r4,lsl #1
    (0x0e4, 0xe595_302c), // ldr  r3,[r5,#0x2c]   the per-pixel hook
    (0x110, 0xe1c4_30b0), // strh r3,[r4]
    (0x118, 0xe1a0_4084), // lsl  r4,r4,#1
    (0x11c, 0xe19c_10b4), // ldrh r1,[ip,r4]
];

/// The `bl` at `MC_grpPutPixel+0x128`, which has to be the 565 lerp below.
const PUT_PIXEL_OP_CALL: u32 = 0x128;

/// `_midas_funcGrpPixelOpProc(bpp, dst, src, alpha)` — a 565 lerp over 256.
///
/// `bpp` is never read. Each channel is `(a*src + (255-a)*dst) >> 8` in place,
/// so red and green are masked back out of the shifted accumulator, while blue
/// goes through `lsl #19` / `lsr #27` — which is `(acc >> 8) & 0x1f`, and the
/// `lsl` really does drop the accumulator's 14th bit.
const PIXEL_OP_WORDS: [(u32, u32); 19] = [
    (0x000, 0xe202_0e7e), // and  r0,r2,#0x7e0
    (0x004, 0xe202_cb3e), // and  ip,r2,#0xf800
    (0x008, 0xe000_0093), // mul  r0,r3,r0
    (0x00c, 0xe00c_0c93), // mul  ip,r3,ip
    (0x014, 0xe002_0293), // mul  r2,r3,r2
    (0x01c, 0xe263_30ff), // rsb  r3,r3,#0xff     255 - alpha
    (0x024, 0xe201_4b3e), // and  r4,r1,#0xf800
    (0x028, 0xe020_0395), // mla  r0,r5,r3,r0
    (0x02c, 0xe02c_c394), // mla  ip,r4,r3,ip
    (0x030, 0xe201_101f), // and  r1,r1,#0x1f
    (0x034, 0xe023_2391), // mla  r3,r1,r3,r2
    (0x038, 0xe1a0_0440), // asr  r0,r0,#8
    (0x03c, 0xe1a0_c44c), // asr  ip,ip,#8
    (0x040, 0xe20c_cb3e), // and  ip,ip,#0xf800
    (0x044, 0xe1a0_3983), // lsl  r3,r3,#0x13
    (0x048, 0xe200_0e7e), // and  r0,r0,#0x7e0
    (0x04c, 0xe180_000c), // orr  r0,r0,ip
    (0x050, 0xe180_0da3), // orr  r0,r0,r3,lsr #27
    (0x054, 0xe8bd_0030), // pop  {r4,r5}
];

/// The context fields `MC_grpPutPixel` reads.
const CTX_CLIP: [u32; 4] = [4, 8, 0xc, 0x10];
const CTX_COLOUR: u32 = 0x14;
const CTX_ALPHA: u32 = 0x20;
const CTX_HOOK: u32 = 0x2c;
/// And the surface's: width, height, pitch in bytes, bits per pixel, pixels.
const GBM_W: u32 = 0;
const GBM_H: u32 = 4;
const GBM_PITCH: u32 = 8;
const GBM_BPP: u32 = 0xc;
const GBM_PIXELS: u32 = 0x10;

/// `_midas_funcFntInvalidate(font)` — where a font's four glyph tables begin.
///
/// `font[0x2c]` is the loaded blob, `font[0x30]` the bytes per hangul component
/// and `font[0x38]` the ASCII table's own offset; the blob's bytes 6 and 7 are
/// how many cho and jung entries there are. From those it seeds
/// `_midas_internalFont[0..3]` — cho, jung, jong, ASCII — and leaves
/// `_midas_r0` holding the last offset it computed, which is scratch the font
/// stack reads back (see `font.rs`).
///
/// Returns 1, or 0 for a null font, and writes nothing in that case.
const FNT_INVALIDATE_WORDS: [(u32, u32); 24] = [
    (0x004, 0xe350_0000), // cmp  r0,#0
    (0x01c, 0xe590_202c), // ldr  r2,[r0,#0x2c]   the blob
    (0x020, 0xe3a0_5009), // mov  r5,#9           its header
    (0x02c, 0xe590_c030), // ldr  ip,[r0,#0x30]   bytes per component
    (0x030, 0xe282_4009), // add  r4,r2,#9
    (0x034, 0xe581_5000), // str  r5,[r1]         _midas_r0 = 9
    (0x038, 0xe583_4000), // str  r4,[r3]         cho
    (0x03c, 0xe3a0_4014), // mov  r4,#0x14
    (0x040, 0xe5d2_5006), // ldrb r5,[r2,#6]      cho count
    (0x044, 0xe004_049c), // mul  r4,ip,r4
    (0x048, 0xe3a0_6016), // mov  r6,#0x16
    (0x04c, 0xe004_0495), // mul  r4,r5,r4
    (0x050, 0xe590_5038), // ldr  r5,[r0,#0x38]
    (0x054, 0xe284_0009), // add  r0,r4,#9
    (0x058, 0xe082_4000), // add  r4,r2,r0
    (0x05c, 0xe581_0000), // str  r0,[r1]
    (0x060, 0xe583_4004), // str  r4,[r3,#4]      jung
    (0x064, 0xe5d2_4007), // ldrb r4,[r2,#7]      jung count
    (0x068, 0xe00c_0c96), // mul  ip,r6,ip
    (0x06c, 0xe285_5006), // add  r5,r5,#6
    (0x070, 0xe02c_0c94), // mla  ip,r4,ip,r0
    (0x074, 0xe583_500c), // str  r5,[r3,#0xc]    ASCII
    (0x080, 0xe581_c000), // str  ip,[r1]
    (0x084, 0xe583_2008), // str  r2,[r3,#8]      jong
];
const FNT_BLOB: u32 = 0x2c;
const FNT_COMP_BYTES: u32 = 0x30;
const FNT_ASCII_OFF: u32 = 0x38;
const FNT_HEADER: u32 = 9;
const FNT_CHO_ROWS: u32 = 0x14;
const FNT_JUNG_ROWS: u32 = 0x16;

/// The map walk's two leaves. `0` is the shipped pair.
pub fn map_leaf() -> bool {
    !std::env::var("CS_NO_MAPLEAF").is_ok_and(|v| v != "0")
}

/// The anchor correction. Its own knob because it is the one leaf this host was
/// already calling, so turning it off is also how you re-measure what the two
/// rotate blitters pay for asking the guest.
pub fn host_anchor() -> bool {
    !std::env::var("CS_NO_ANCHOR").is_ok_and(|v| v != "0")
}

/// The palette pair.
pub fn host_palette() -> bool {
    !std::env::var("CS_NO_PAL").is_ok_and(|v| v != "0")
}

/// `MC_grpPutPixel` and the lerp under it. Its own knob because it is the one
/// leaf here that *writes pixels*, so it is the one whose A/B is a picture.
pub fn put_pixel() -> bool {
    !std::env::var("CS_NO_PUTPIXEL").is_ok_and(|v| v != "0")
}

/// `_midas_funcFntInvalidate`. Its own knob rather than the font stack's
/// `CS_NO_FNT`, because that one turns off `font.rs` — a much bigger thing — and
/// measuring this leaf means turning off only this leaf.
pub fn fnt_invalidate() -> bool {
    !std::env::var("CS_NO_FNTINV").is_ok_and(|v| v != "0")
}

/// Addresses the leaf ports read, resolved once at boot.
///
/// Resolved once because these are the hottest entry hooks in the build — the
/// map pair alone runs tens of thousands of times a second — and `Vm::symbol`
/// hashes its name. The same mistake cost a measured 8 ms a frame when
/// `_midas_drawDIR` was looked up per call.
#[derive(Default, Clone, Copy)]
pub struct Leaves {
    /// `_lcdG`, whose `+0x4c`/`+0x50` are the surface the clip is clamped to.
    pub lcd_g: u32,
    /// `_map`, the word array `Map_getTileIndex` indexes.
    pub map: u32,
    /// `nMapClipX1`, `nMapClipY1`, `nMapClipX2`, `nMapClipY2`.
    pub clip: [u32; 4],
}

impl Leaves {
    pub fn resolve(mut sym: impl FnMut(&str) -> Option<u32>) -> Self {
        let mut l = Leaves { lcd_g: 0, map: 0, clip: [0; 4] };
        l.lcd_g = sym("_lcdG").unwrap_or(0);
        l.map = sym("_map").unwrap_or(0);
        for (i, n) in ["nMapClipX1", "nMapClipY1", "nMapClipX2", "nMapClipY2"].iter().enumerate() {
            l.clip[i] = sym(n).unwrap_or(0);
        }
        l
    }
}

/// The five leaves, each with the check that has to pass before it is served
/// and the knob that can hand it back. Unlike [`PATCHED`], which installs on a
/// symbol name alone, every one of these is a *reimplementation* of arithmetic
/// that has to match — so a build whose leaf has moved gets the interpreter and
/// a warning, never a host answer to a question it did not ask.
pub const LEAF_SITES: [(&str, Patch); 7] = [
    ("MapClip", Patch::MapClip),
    ("Map_getTileIndex", Patch::MapTileIndex),
    ("DM_CalcAnchorGbm", Patch::CalcAnchor),
    ("DM_RestorePalette", Patch::RestorePalette),
    ("DM_SetChagePalBlendEx", Patch::PalBlend),
    ("MC_grpPutPixel", Patch::PutPixel),
    ("_midas_funcFntInvalidate", Patch::FntInvalidate),
];

/// Is this leaf wanted, and is the guest still the one it was written against?
pub fn leaf_ok(kind: Patch, base: u32, mem: &mut GuestMem) -> bool {
    match kind {
        Patch::MapClip => map_leaf() && map_clip_site(base, mem),
        Patch::MapTileIndex => map_leaf() && map_tile_site(base, mem),
        Patch::CalcAnchor => host_anchor() && anchor_site(base, mem),
        Patch::RestorePalette => host_palette() && restore_pal_site(base, mem),
        Patch::PalBlend => host_palette() && pal_blend_site(base, mem),
        Patch::PutPixel => put_pixel() && put_pixel_site(base, mem),
        Patch::FntInvalidate => fnt_invalidate() && fnt_invalidate_site(base, mem),
        _ => false,
    }
}

/// Is this leaf wanted at all? Separates "switched off" from "it moved", so
/// only the second warns.
pub fn leaf_wanted(kind: Patch) -> bool {
    match kind {
        Patch::MapClip | Patch::MapTileIndex => map_leaf(),
        Patch::CalcAnchor => host_anchor(),
        Patch::RestorePalette | Patch::PalBlend => host_palette(),
        Patch::PutPixel => put_pixel(),
        Patch::FntInvalidate => fnt_invalidate(),
        _ => false,
    }
}

/// True when `MapClip` is still the clamp this file ported.
pub fn map_clip_site(base: u32, mem: &mut GuestMem) -> bool {
    MAP_CLIP_WORDS.iter().all(|&(off, w)| mem.r32(base.wrapping_add(off)) == w)
}

/// True when `Map_getTileIndex` is still that one indexed byte.
pub fn map_tile_site(base: u32, mem: &mut GuestMem) -> bool {
    MAP_TILE_WORDS.iter().all(|&(off, w)| mem.r32(base.wrapping_add(off)) == w)
}

/// True when `DM_CalcAnchorGbm` is still the anchor arithmetic above.
pub fn anchor_site(base: u32, mem: &mut GuestMem) -> bool {
    ANCHOR_WORDS.iter().all(|&(off, w)| mem.r32(base.wrapping_add(off)) == w)
}

/// `DM_SetChagePalBlendEx(obj, r, g, b, mode, alpha)` — walk `obj`'s live
/// palette and blend every entry toward one colour. Same object fields as
/// `DM_RestorePalette`, and the same `+0x24` flag: set on the way out, so the
/// blend happens once per object until something restores it.
///
/// **Six arguments, and the two stack ones are at `[sp]` and `[sp+4]`** — not
/// where `DM_CalcAnchorGbm` keeps its pair. The body reads `[sp+0x78]` and
/// `[sp+0x7c]` behind `push {r4..fp,lr}` + `sub sp,#0x54`, which is exactly
/// 0x78, so those are the first two stack words as the caller left them;
/// `Map_drawPartTile+0x1f4` confirms it with `str r1,[sp]` / `str ip,[sp,#4]`.
/// Both are read with `ldrb`, so only the low byte of each counts.
///
/// What each entry becomes, and every one of these is a thing that can be got
/// wrong:
///
/// * **Green is carried as five bits.** Every channel is looked up and weighted
///   as 5 bits — green by `(c & 0x7e0) >> 6`, which drops its low bit — and the
///   result is doubled back on the way into the 565 word. The rotate and screen
///   blitters do the same thing (`Blend565`); this is the same table.
/// * **Two weights, and they are not the same fraction.** `alpha >> 3` gives
///   `w = a+1` out of 32 for red and blue, but green gets `2a+1` out of 64 —
///   which is *not* `2(a+1)`. That off-by-one asymmetry is the shipped
///   behaviour, and it is why green needs its own pair of weights.
/// * **`alpha == 0xff` skips the weighting**, so the blended value lands whole.
/// * **`mode == 0x10` is greyscale**: `(6r + 4g + 2b) >> 4` on the 5-bit
///   channels, the same luma the two `Ex` blitters use.
/// * **`mode == 0` with no colour asked for is not a no-op.** It rebuilds the
///   entry from its own channels, which drops green's low bit — the one path
///   where "blend toward nothing" still changes the palette. Reproduced rather
///   than shortcut.
///
/// Declined, never guessed: `mode == 0x16`, which calls `DM_ChangeHSB` once per
/// entry, and any other mode above 0xf, where the shipped code indexes past the
/// sixteen 32x32 planes of `BlendingTable32Bit` and reads whatever `.data`
/// follows.
const PAL_BLEND_WORDS: [(u32, u32); 42] = [
    (0x008, 0xe5d0_c024), // ldrb ip,[r0,#0x24]     the flag
    (0x010, 0xe5dd_007c), // ldrb r0,[sp,#0x7c]     alpha
    (0x01c, 0xe35c_0001), // cmp  ip,#1             …and only 1 returns
    (0x030, 0xe5dd_8078), // ldrb r8,[sp,#0x78]     mode
    (0x03c, 0xe595_9010), // ldr  sb,[r5,#0x10]     the live palette
    (0x050, 0xe5d3_308a), // ldrb r3,[r3,#0x8a]     16bpp only
    (0x064, 0xe595_2014), // ldr  r2,[r5,#0x14]     entries
    (0x068, 0xe200_00f8), // and  r0,r0,#0xf8
    (0x06c, 0xe1a0_0140), // asr  r0,r0,#2          green's weight, 2a
    (0x070, 0xe1a0_31a1), // lsr  r3,r1,#3          red/blue's, a
    (0x074, 0xe283_3001), // add  r3,r3,#1
    (0x078, 0xe280_0001), // add  r0,r0,#1
    (0x084, 0xe585_901c), // str  sb,[r5,#0x1c]     before the count test
    (0x0a0, 0xe1a0_1143), // asr  r1,r3,#2          g>>2, six bits
    (0x0a4, 0xe1a0_31c0), // asr  r3,r0,#3          b>>3
    (0x0ac, 0xe183_3281), // orr  r3,r3,r1,lsl #5
    (0x0b4, 0xe1a0_21c0), // asr  r2,r0,#3          r>>3
    (0x0bc, 0xe183_3582), // orr  r3,r3,r2,lsl #11
    (0x0f4, 0xe261_1020), // rsb  r1,r1,#0x20       32 - w
    (0x0f8, 0xe262_2040), // rsb  r2,r2,#0x40       64 - wg
    (0x0fc, 0xe1a0_3288), // lsl  r3,r8,#5          the table's plane
    (0x134, 0xe7d2_6006), // ldrb r6,[r2,r6]
    (0x15c, 0xe1a0_1086), // lsl  r1,r6,#1          green doubled
    (0x164, 0xe356_00ff), // cmp  r6,#0xff          alpha == 0xff?
    (0x19c, 0xe1a0_22c2), // asr  r2,r2,#5
    (0x1a0, 0xe1a0_1341), // asr  r1,r1,#6          green out of 64
    (0x1ac, 0xe1a0_1011), // lsl  r1,r1,r0          5 or 6
    (0x1b4, 0xe183_3582), // orr  r3,r3,r2,lsl #11
    (0x1b8, 0xe201_1e7e), // and  r1,r1,#0x7e0
    (0x1c0, 0xe1c9_30b0), // strh r3,[sb]           in place
    (0x1e4, 0xe199_00b2), // ldrh r0,[sb,r2]
    (0x1e8, 0xe597_3000), // ldr  r3,[r7]           *SKIP_COL
    (0x204, 0xe358_0016), // cmp  r8,#0x16          the HSB mode
    (0x218, 0xe358_0010), // cmp  r8,#0x10          greyscale
    (0x238, 0xe1a0_3100), // lsl  r3,r0,#2          6r
    (0x23c, 0xe083_3080), // add  r3,r3,r0,lsl #1
    (0x240, 0xe083_3105), // add  r3,r3,r5,lsl #2   4g
    (0x244, 0xe083_308c), // add  r3,r3,ip,lsl #1   2b
    (0x24c, 0xe1a0_3243), // asr  r3,r3,#4
    (0x280, 0xe3a0_0006), // mov  r0,#6             the five-bit-green shift
    (0x290, 0xe5c5_3024), // strb r3,[r5,#0x24]     set on the way out
    (0x2b8, 0xe3a0_0005), // mov  r0,#5
];

/// The mode that calls `DM_ChangeHSB` per entry, and the greyscale one.
const PAL_MODE_HSB: u32 = 0x16;
const PAL_MODE_GRAY: u32 = 0x10;

/// True when `DM_RestorePalette` is still that little-endian walk from offset 9.
pub fn restore_pal_site(base: u32, mem: &mut GuestMem) -> bool {
    RESTORE_PAL_WORDS.iter().all(|&(off, w)| mem.r32(base.wrapping_add(off)) == w)
}

/// True when `DM_SetChagePalBlendEx` is still the blend above.
pub fn pal_blend_site(base: u32, mem: &mut GuestMem) -> bool {
    PAL_BLEND_WORDS.iter().all(|&(off, w)| mem.r32(base.wrapping_add(off)) == w)
}

/// True when `MC_grpPutPixel` is still that pixel, **and** the `bl` at the end
/// of its blended arm still reaches the lerp this file ported, which is checked
/// by target and then by the lerp's own words.
pub fn put_pixel_site(base: u32, mem: &mut GuestMem) -> bool {
    if !PUT_PIXEL_WORDS.iter().all(|&(off, w)| mem.r32(base.wrapping_add(off)) == w) {
        return false;
    }
    let Some(op) = branch_target(base.wrapping_add(PUT_PIXEL_OP_CALL), mem, true) else {
        return false;
    };
    PIXEL_OP_WORDS.iter().all(|&(off, w)| mem.r32(op.wrapping_add(off)) == w)
}

/// True when `_midas_funcFntInvalidate` is still that seeding.
pub fn fnt_invalidate_site(base: u32, mem: &mut GuestMem) -> bool {
    FNT_INVALIDATE_WORDS.iter().all(|&(off, w)| mem.r32(base.wrapping_add(off)) == w)
}

/// `_midas_funcGrpPixelOpProc(_, dst, src, alpha)` — the 565 lerp, over 256.
pub(crate) fn pixel_op(dst: u32, src: u32, alpha: u32) -> u32 {
    let inv = 255 - alpha;
    let r = alpha * (src & 0xf800) + inv * (dst & 0xf800);
    let g = alpha * (src & 0x7e0) + inv * (dst & 0x7e0);
    let b = alpha * (src & 0x1f) + inv * (dst & 0x1f);
    // Red and green are masked out of the shifted accumulator; blue is shifted
    // left 19 and back right 27, which is the same as `>> 8` masked to five
    // bits — the left shift is what drops the accumulator's top bit.
    ((r >> 8) & 0xf800) | ((g >> 8) & 0x7e0) | (b.wrapping_shl(19) >> 27)
}

/// A 565 word as the three five-bit channels this function works in — green by
/// dropping its low bit, which is the whole reason it needs doubling later.
fn pal_split(c: u32) -> (u32, u32, u32) {
    (c >> 11, (c >> 6) & 0x1f, c & 0x1f)
}

/// Where in `BlendingTable32Bit` a (target, source) pair for this mode is.
fn pal_blend_at(mode: u32, t: u32, s: u32) -> u32 {
    (mode * 32 + t) * 32 + s
}

/// One palette entry's new 565 word, from the source and target channels and the
/// three already looked up. `b.1` is green **doubled** back to six bits.
///
/// Split out so the loop stays readable; every line is one of the bullets on
/// [`PAL_BLEND_WORDS`]. The table lookups are the caller's because they are
/// reads of guest memory and doing them here would mean handing this a copy of
/// the whole 16 KB table on every call.
fn pal_blend_out(
    s: (u32, u32, u32),
    t: (u32, u32, u32),
    b: (u32, u32, u32),
    plain: bool,
    alpha: u32,
    weights: (i32, i32, i32, i32),
) -> u32 {
    let (wq, inv_q, wg, inv_g) = weights;
    if alpha == 0xff {
        // `mode == 0` with no colour asked for rebuilds the entry from its own
        // channels — and so drops green's low bit.
        if plain {
            return (t.0 << 11) | ((t.1 << 6) & 0x7e0) | (t.2 & 0x1f);
        }
        return (b.0 << 11) | ((b.1 << 5) & 0x7e0) | (b.2 & 0x1f);
    }
    let r = (wq * b.0 as i32 + inv_q * s.0 as i32) >> 5;
    let g = (wg * b.1 as i32 + inv_g * (s.1 << 1) as i32) >> 6;
    let bl = (wq * b.2 as i32 + inv_q * s.2 as i32) >> 5;
    ((r as u32) << 11) | (((g as u32) << 5) & 0x7e0) | (bl as u32 & 0x1f)
}

impl Vm {
    // ---- 그리기 리프 다섯 -------------------------------------------------

    /// `MapClip` — the four clip globals.
    ///
    /// `r0` is left holding the surface width, which is what the shipped body
    /// happens to return: the function is void, so nothing should read it, but
    /// matching costs one store and removes the question.
    pub(crate) fn host_map_clip(&mut self) -> Result<bool, String> {
        let l = self.leaves;
        if l.lcd_g == 0 || l.clip.iter().any(|&a| a == 0) {
            return Ok(false);
        }
        let a: Vec<i32> = (0..4).map(|i| self.cpu.reg_get(Mode::User, i) as i32).collect();
        let w = self.mem.r32(l.lcd_g + LCD_W) as i32;
        let h = self.mem.r32(l.lcd_g + LCD_H) as i32;
        let want = [a[0].max(0), a[1].max(0), a[2].min(w), a[3].min(h)];
        for (slot, v) in l.clip.iter().zip(want) {
            self.mem.w32(*slot, v as u32);
        }
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, 0, w as u32);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// `Map_getTileIndex` — one byte of a layer's tile array, and the pointer
    /// copy the shipped body makes on its way past.
    pub(crate) fn host_map_tile_index(&mut self) -> Result<bool, String> {
        let map = self.leaves.map;
        if map == 0 {
            return Ok(false);
        }
        let layer = self.cpu.reg_get(Mode::User, 0);
        let off = self.cpu.reg_get(Mode::User, 1);
        let tiles = self.mem.r32(map.wrapping_add(layer.wrapping_add(MAP_TILE_LAYER) << 2));
        self.mem.w32(map.wrapping_add(layer.wrapping_add(MAP_TILE_COPY) << 2), tiles);
        let byte = self.mem.r8(tiles.wrapping_add(off)) as u32;
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, 0, byte);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// `DM_CalcAnchorGbm` — the anchor correction, through the two pointers.
    ///
    /// Void, and the shipped body leaves `r0` holding whichever of its working
    /// values it last loaded, so nothing is written back to it here.
    pub(crate) fn host_calc_anchor(&mut self) -> Result<bool, String> {
        let px = self.cpu.reg_get(Mode::User, 0);
        let py = self.cpu.reg_get(Mode::User, 1);
        let w = self.cpu.reg_get(Mode::User, 2) as i32;
        let h = self.cpu.reg_get(Mode::User, 3) as i32;
        // `[sp+4]` and `[sp+8]`: the fifth stack argument is the rotation and
        // this function never reads it.
        let sp = self.cpu.reg_get(Mode::User, reg::SP);
        let angle = self.mem.r32(sp.wrapping_add(4));
        let anchor = self.mem.r32(sp.wrapping_add(8));
        let (x, y) =
            calc_anchor_gbm(self.mem.r32(px) as i32, self.mem.r32(py) as i32, w, h, angle, anchor);
        self.mem.w32(px, x as u32);
        self.mem.w32(py, y as u32);
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// `DM_SetChagePalBlendEx` — the whole palette blended toward one colour.
    ///
    /// Declines on the two modes that would have this guessing: `0x16`, which
    /// runs `DM_ChangeHSB` per entry, and anything above `0x10`, where the
    /// shipped table index leaves the table.
    pub(crate) fn host_pal_blend(&mut self) -> Result<bool, String> {
        let lcd = self.leaves.lcd_g;
        let (Some(skip_p), Some(tbl_p)) =
            (self.symbol("SKIP_COL"), self.symbol("BlendingTable32Bit"))
        else {
            return Ok(false);
        };
        if lcd == 0 {
            return Ok(false);
        }
        let sp = self.cpu.reg_get(Mode::User, reg::SP);
        let mode = self.mem.r32(sp) & 0xff;
        if mode == PAL_MODE_HSB || mode > PAL_MODE_GRAY {
            return Ok(false);
        }
        let alpha = self.mem.r32(sp.wrapping_add(4)) & 0xff;
        let obj = self.cpu.reg_get(Mode::User, 0);
        let rgb: Vec<u32> = (1..4).map(|i| self.cpu.reg_get(Mode::User, i)).collect();
        let lr = self.cpu.reg_get(Mode::User, reg::LR);

        let flag = self.mem.r8(obj + PAL_DONE) as u32;
        let pal = self.mem.r32(obj + PAL_LIVE);
        if flag == 1 || (pal as i32) <= 0 || self.mem.r8(lcd + LCD_BPP16) != 1 {
            self.cpu.reg_set(Mode::User, reg::PC, lr);
            return Ok(true);
        }
        let n = self.mem.r32(obj + PAL_COUNT) as i32;
        self.mem.w32(obj + PAL_CURSOR, pal);
        if n > 0 {
            let a = alpha >> 3;
            let (wq, wg) = ((a + 1) as i32, (2 * a + 1) as i32);
            let weights = (wq, 32 - wq, wg, 64 - wg);
            let colour = ((rgb[0] >> 3) << 11) | ((rgb[1] >> 2) << 5) | (rgb[2] >> 3);
            let asked = rgb[0] | rgb[1] | rgb[2] != 0;
            let skip = self.mem.r32(skip_p);
            for i in 0..n as u32 {
                // Re-read the base every pass, as the shipped loop does.
                let base = self.mem.r32(obj + PAL_CURSOR);
                let at = base.wrapping_add(2 * i);
                let src = self.mem.r16(at) as u32;
                if src == skip {
                    continue;
                }
                let target = if asked { colour & 0xffff } else { src };
                let (s, t) = (pal_split(src), pal_split(target));
                // Three bytes out of sixteen 32x32 planes; the one index that
                // could leave them was declined above.
                let b = if mode == PAL_MODE_GRAY {
                    let grey = (6 * s.0 + 4 * s.1 + 2 * s.2) >> 4;
                    (grey, grey << 1, grey)
                } else {
                    let r = self.mem.r8(tbl_p + pal_blend_at(mode, t.0, s.0)) as u32;
                    let g = self.mem.r8(tbl_p + pal_blend_at(mode, t.1, s.1)) as u32;
                    let bl = self.mem.r8(tbl_p + pal_blend_at(mode, t.2, s.2)) as u32;
                    (r, g << 1, bl)
                };
                let plain = mode == 0 && target == src;
                self.mem.w16(at, pal_blend_out(s, t, b, plain, alpha, weights) as u16);
            }
        }
        self.mem.w8(obj + PAL_DONE, 1);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// `MC_grpPutPixel` — one clipped pixel.
    ///
    /// Declines only where the shipped body would leave this guessing: a 32bpp
    /// surface, and a context carrying a per-pixel hook. A bpp that is neither
    /// 16 nor 32 draws nothing, which is answered rather than declined, because
    /// "nothing" is a thing this can be sure of.
    pub(crate) fn host_put_pixel(&mut self) -> Result<bool, String> {
        let gbm = self.cpu.reg_get(Mode::User, 0);
        let x = self.cpu.reg_get(Mode::User, 1) as i32;
        let y = self.cpu.reg_get(Mode::User, 2) as i32;
        let ctx = self.cpu.reg_get(Mode::User, 3);
        let bpp = self.mem.r32(gbm + GBM_BPP);
        if bpp == 0x20 {
            return Ok(false);
        }
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        let done = |vm: &mut Vm| {
            vm.cpu.reg_set(Mode::User, reg::PC, lr);
            Ok(true)
        };
        // One sign test for both, then the surface, then the context's rect.
        if x < 0 || y < 0 {
            return done(self);
        }
        let inside = x < self.mem.r32(gbm + GBM_W) as i32
            && y < self.mem.r32(gbm + GBM_H) as i32
            && x >= self.mem.r32(ctx + CTX_CLIP[0]) as i32
            && y >= self.mem.r32(ctx + CTX_CLIP[1]) as i32
            && x < self.mem.r32(ctx + CTX_CLIP[2]) as i32
            && y < self.mem.r32(ctx + CTX_CLIP[3]) as i32;
        if !inside || bpp != 0x10 {
            return done(self);
        }
        if self.mem.r32(ctx + CTX_HOOK) != 0 {
            return Ok(false);
        }
        let at = self
            .mem
            .r32(gbm + GBM_PIXELS)
            .wrapping_add((y as u32).wrapping_mul(self.mem.r32(gbm + GBM_PITCH)))
            .wrapping_add(2 * x as u32);
        let colour = self.mem.r32(ctx + CTX_COLOUR);
        let alpha = self.mem.r32(ctx + CTX_ALPHA);
        let v = if alpha as i32 > 0xfe {
            colour & 0xffff
        } else {
            pixel_op(self.mem.r16(at) as u32, colour, alpha)
        };
        self.mem.w16(at, v as u16);
        done(self)
    }

    /// `_midas_funcFntInvalidate` — the four glyph-table bases.
    pub(crate) fn host_fnt_invalidate(&mut self) -> Result<bool, String> {
        let Some(g) = self.font_globals else { return Ok(false) };
        let font = self.cpu.reg_get(Mode::User, 0);
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        if font != 0 {
            let blob = self.mem.r32(font + FNT_BLOB);
            let comp = self.mem.r32(font + FNT_COMP_BYTES);
            self.mem.w32(g.r0, FNT_HEADER);
            self.mem.w32(g.internal_font, blob.wrapping_add(FNT_HEADER));
            let cho = self.mem.r8(blob + 6) as u32;
            let cho_bytes = comp.wrapping_mul(FNT_CHO_ROWS);
            let jung_at = cho.wrapping_mul(cho_bytes).wrapping_add(FNT_HEADER);
            self.mem.w32(g.r0, jung_at);
            self.mem.w32(g.internal_font + 4, blob.wrapping_add(jung_at));
            let jung = self.mem.r8(blob + 7) as u32;
            let jung_bytes = comp.wrapping_mul(FNT_JUNG_ROWS);
            let jong_at = jung.wrapping_mul(jung_bytes).wrapping_add(jung_at);
            let ascii = self.mem.r32(font + FNT_ASCII_OFF).wrapping_add(6);
            self.mem.w32(g.internal_font + 0xc, ascii);
            self.mem.w32(g.r0, jong_at);
            self.mem.w32(g.internal_font + 8, blob.wrapping_add(jong_at));
        }
        self.cpu.reg_set(Mode::User, 0, u32::from(font != 0));
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// `DM_RestorePalette` — the live palette back from the resource blob.
    ///
    /// Declines nothing: every early return the shipped body has is reproduced,
    /// including that the flag is cleared on all of them but the first three.
    pub(crate) fn host_restore_palette(&mut self) -> Result<bool, String> {
        let lcd = self.leaves.lcd_g;
        if lcd == 0 {
            return Ok(false);
        }
        let obj = self.cpu.reg_get(Mode::User, 0);
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        let mut done = || {
            self.cpu.reg_set(Mode::User, reg::PC, lr);
            Ok(true)
        };
        if obj == 0 || self.mem.r8(obj + PAL_DONE) == 0 {
            return done();
        }
        let blob = self.mem.r32(obj + PAL_BLOB);
        if blob == 0 {
            return done();
        }
        self.mem.w32(obj + PAL_BLOB_CURSOR, blob);
        if self.mem.r8(lcd + LCD_BPP16) == 1 {
            let live = self.mem.r32(obj + PAL_LIVE);
            if live != 0 {
                self.mem.w32(obj + PAL_CURSOR, live);
                let n = self.mem.r32(obj + PAL_COUNT) as i32;
                for i in 0..n.max(0) {
                    let at = blob.wrapping_add(PAL_BLOB_FIRST + 2 * i as u32);
                    let v = self.mem.r8(at) as u16 | (self.mem.r8(at + 1) as u16) << 8;
                    self.mem.w16(live.wrapping_add(2 * i as u32), v);
                }
            }
        }
        self.mem.w8(obj + PAL_DONE, 0);
        done()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::potions::{Kind, PotionState};
    use crate::{Config, Vm};

    /// The real guest, loaded but not booted. Everything the 비약 patches touch
    /// — the symbol table, the instruction words they stand in for, the
    /// registers they read — is there the moment the ELF is mapped, and not
    /// booting keeps these tests to a few milliseconds. Shared with `cave.rs`.
    pub(crate) fn loaded_guest() -> Vm {
        let so = std::fs::read("../payload/libchronosMian.so").expect("payload/ is committed");
        Vm::new(
            &so,
            Config {
                vfs: std::sync::Arc::new(Vec::new()),
                storage: std::env::temp_dir().join("cs_patch_test_no_such_dir"),
                screen_w: 800,
                screen_h: 480,
                virtual_clock: true,
                integer_scale: true,
                wipi_w: crate::WIPI_W,
                wipi_h: crate::WIPI_H,
                fill_width: false,
                host: Default::default(),
                audio: crate::audio::Mixer::shared(22_050),
                async_audio: false,
                // A fault is a divergence, and these runs exist to catch divergence.
                tolerate_faults: false,
            },
        )
        .expect("guest loads")
    }

    /// The S25 crash, run as a test: the bad pointer that closed the app twice.
    ///
    /// `cs_crash.txt` recorded `MONSTER_getCastingTime+0x1c` loading through a
    /// map object whose payload pointer held `7`, from `MONSTER_preProcess`, on
    /// two separate runs 96 seconds apart. The load is `ldr ip, [r0, #0x204]`,
    /// so with `r0 = 7` the word is at `0x20b` and the report names the aligned
    /// `0x208` - which is how the register dump and the fault address are
    /// reconciled.
    ///
    /// Both halves matter. Strict is what the probe and every A/B in this repo
    /// run with, and it must still refuse. Tolerant is what ships on the phone,
    /// and it must come back with the guest's own answer for "this thing is not
    /// casting" (-1) rather than ending the run - having counted the fault and
    /// kept a report for `cs_crash.txt`, because the next one of these will be
    /// found the same way this one was.
    #[test]
    fn the_pointer_that_closed_the_app_is_survivable() {
        let mut vm = loaded_guest();
        let f = vm.symbol("MONSTER_getCastingTime").expect("the guest exports it");

        let err = vm.call(f, &[7]).expect_err("strict runs still refuse");
        assert!(err.contains("0x00000208"), "{err}");
        assert!(err.contains("MONSTER_getCastingTime"), "{err}");
        assert_eq!(vm.faults, 0, "a strict run must not count it as survived");

        let mut vm = loaded_guest();
        vm.cfg.tolerate_faults = true;
        let r = vm.call(f, &[7]).expect("a tolerated run returns");
        assert_eq!(r as i32, -1, "the guest's own answer for 'not casting'");
        assert!(vm.faults >= 1, "the fault is counted, not swallowed");
        let report = vm.fault_report.take().expect("the first site keeps a report");
        assert!(report.contains("MONSTER_getCastingTime"), "{report}");
        assert!(report.contains("tolerated"), "{report}");

        // A pointer the guest can actually use takes the same path without
        // faulting at all, so the tolerance is not hiding a load that works.
        let mut vm = loaded_guest();
        let scratch = vm.heap.alloc(0x400).expect("guest heap has room");
        assert_eq!(vm.call(f, &[scratch]).expect("no fault"), u32::MAX);
        assert_eq!(vm.faults, 0);
    }

    /// A tolerated fault reports its site once, however many times it repeats.
    ///
    /// The site this was written for faults three times a frame forever, which
    /// at sixty frames a second is a log line every 5 ms - enough to bury the
    /// perf lines that say what else the build is doing.
    #[test]
    fn a_repeated_fault_is_one_report_and_a_counter() {
        let mut vm = loaded_guest();
        vm.cfg.tolerate_faults = true;
        let f = vm.symbol("MONSTER_getCastingTime").expect("the guest exports it");
        for _ in 0..8 {
            vm.call(f, &[7]).expect("tolerated");
        }
        assert_eq!(vm.faults, 8, "every fault counts");
        vm.fault_report.take();
        for _ in 0..8 {
            vm.call(f, &[7]).expect("tolerated");
        }
        assert!(vm.fault_report.is_none(), "the same site does not report twice");
        assert_eq!(vm.faults, 16);
    }

    /// `Sqrt` is a leaf with no state, so the guest's own copy is the oracle.
    ///
    /// Swept rather than spot-checked because the failure this guards against
    /// is a single digit step of the restoring loop being wrong, which shows up
    /// on a thin band of inputs and nowhere else. The band either side of
    /// `0x4000_0000` matters most: that is where the prologue's `cmn` folds the
    /// range by hand.
    #[test]
    fn the_guest_agrees_with_our_integer_square_root() {
        let mut vm = loaded_guest();
        let sqrt = vm.symbol("Sqrt").expect("Sqrt is in .symtab");
        let mut cases: Vec<u32> = (0..4096).collect();
        cases.extend([
            0x3fff_fffe,
            0x3fff_ffff,
            0x4000_0000,
            0x4000_0001,
            0x7fff_ffff,
            0x8000_0000,
            0xffff_fffe,
            0xffff_ffff,
        ]);
        // Perfect squares and their neighbours, where a restoring sqrt is most
        // likely to land one off.
        for r in [1u32, 2, 3, 255, 256, 257, 1023, 1024, 4095, 4096, 30000, 46340, 65535] {
            let sq = r.wrapping_mul(r);
            cases.extend([sq.wrapping_sub(1), sq, sq.wrapping_add(1)]);
        }
        // A deterministic spread over the whole range.
        let mut s = 0x1234_5678u32;
        for _ in 0..4000 {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            cases.push(s);
        }
        for x in cases {
            let want = vm.call(sqrt, &[x]).expect("Sqrt returns");
            assert_eq!(guest_sqrt(x), want, "Sqrt({x:#x})");
        }
    }

    /// `_atan2` likewise, including the quadrant fixups and the below-the-table
    /// escape — the three constants (+91, −269, −89) are the kind of thing that
    /// looks arbitrary and is easy to transcribe one off.
    ///
    /// The sweep covers both signs of both arguments and both zeroes, because
    /// each corner is a different arm of the original.
    #[test]
    fn the_guest_agrees_with_our_atan2() {
        let mut vm = loaded_guest();
        let atan2 = vm.symbol("_atan2").expect("_atan2 is in .symtab");
        let n_tan_p = vm.symbol("nTan").expect("nTan is in .symtab");
        // 180 entries, one past the symbol's 179 — as `_atan2` itself reads it.
        let n_tan: Vec<i32> =
            (0..180).map(|i| vm.mem.r32(n_tan_p.wrapping_add(i * 4)) as i32).collect();

        let mut coords: Vec<i32> = Vec::new();
        for v in -40..=40 {
            coords.push(v);
        }
        coords.extend([-4096, -1000, -321, -256, -2, 2, 256, 321, 1000, 4096]);
        let mut checked = 0usize;
        for &y in &coords {
            for &x in &coords {
                let want = vm.call(atan2, &[y as u32, x as u32]).expect("_atan2 returns") as i32;
                assert_eq!(guest_atan2(y, x, &n_tan), want, "_atan2(y={y}, x={x})");
                // What the blur actually indexes the sin/cos tables with.
                let folded = if want < 0 { want + 360 } else { want };
                assert!((0..360).contains(&folded), "_atan2(y={y}, x={x}) = {want} escapes 0..359");
                checked += 1;
            }
        }
        assert!(checked > 6000, "the sweep shrank");
    }

    /// The whole blur, against the shipped code, pixel for pixel.
    ///
    /// The probe cannot reach a boss fight, so the usual "replay and diff the
    /// dumps" A/B cannot see this function at all. Calling the guest's own
    /// `DM_RadialBlurLcd565` on a framebuffer we built, with our stand-in
    /// lifted out of the patch table for the duration, is a stronger check
    /// anyway: the oracle is the actual ARM code, not a screenshot of it.
    ///
    /// Sizes are deliberately odd and small — the halving that finds the centre
    /// truncates, and the sample walk clamps against `w-1`/`h-1`, so an even
    /// square would hide both.
    /// Put a NUL-terminated string into guest memory.
    fn put_cstr(vm: &mut Vm, at: u32, s: &[u8]) {
        for (i, b) in s.iter().enumerate() {
            vm.mem.w8(at + i as u32, *b);
        }
        vm.mem.w8(at + s.len() as u32, 0);
    }

    /// Set up `MC_knlGetSystemProperty(name, out, size)` and run the hook.
    ///
    /// The handler is called directly rather than through the interpreter. The
    /// guest's own version reaches `AndroidService::getInstance()`, which needs
    /// platform state `loaded_guest` deliberately does not build — running it
    /// here hangs rather than fails, which is a much worse test than none.
    fn ask_property(vm: &mut Vm, name_p: u32, name: &[u8], out: u32, size: u32) -> bool {
        put_cstr(vm, name_p, name);
        for i in 0..0x40 {
            vm.mem.w8(out + i, 0);
        }
        vm.cpu.reg_set(Mode::User, 0, name_p);
        vm.cpu.reg_set(Mode::User, 1, out);
        vm.cpu.reg_set(Mode::User, 2, size);
        // A distinctive LR/PC pair so the caller can prove the hook returned
        // rather than falling through onto its own instruction again.
        vm.cpu.reg_set(Mode::User, reg::LR, 0x0bad_0000);
        vm.cpu.reg_set(Mode::User, reg::PC, 0x0100_0000);
        vm.host_phone_number().expect("the hook does not fault")
    }

    /// The two sites really are the only way to ask for 데이터 관리.
    ///
    /// `MainMenuOptionKey` picks a screen by return value, so "block the menu"
    /// is "never return 0x64". If a third site ever produced it the hook would
    /// be a hole, and the failure mode is a deleted save.
    #[test]
    fn only_two_instructions_choose_data_manage() {
        let mut vm = loaded_guest();
        let base = vm.symbol("MainMenuOptionKey").expect("in .symtab");
        let size = vm.elf.sizes.get("MainMenuOptionKey").copied().unwrap_or(0);
        assert!(size > 0, "MainMenuOptionKey has a size in .symtab");

        let found: Vec<u32> =
            (0..size).step_by(4).filter(|off| vm.mem.r32(base + off) == 0xe3a0_0064).collect();
        assert_eq!(
            found,
            DATA_MANAGE_SITES.iter().map(|&(o, _)| o).collect::<Vec<_>>(),
            "the `mov r0, #0x64` sites moved or multiplied"
        );
        // And each is followed by the branch into the epilogue, which is what
        // makes advancing PC by one instruction the right move.
        for (off, _) in DATA_MANAGE_SITES {
            let next = vm.mem.r32(base + off + 4);
            assert_eq!(next >> 24, 0xea, "site +{off:#x} is not followed by a plain b");
        }
    }

    // ---- 「레시피2」 ------------------------------------------------------

    fn recipe2_bases(vm: &mut Vm) -> (u32, u32, u32, u32) {
        (
            vm.symbol("NpcUI_DrawMerChant_Main").expect(".symtab"),
            vm.symbol("NpcUI_merchantKeyPressed").expect(".symtab"),
            vm.symbol("NpcUI_setMerchant").expect(".symtab"),
            vm.symbol("getInCodeString").expect(".symtab"),
        )
    }

    /// All fourteen words and both `bl` targets are where the five hooks think,
    /// and changing any one of them refuses the install.
    ///
    /// A menu hook that installs against drifted code is worse than one that
    /// does not install: two of these answer **flags** for a `cmp`, so a wrong
    /// address would silently steer somebody else's predicated instruction.
    #[test]
    fn the_shop_menu_is_the_five_rows_the_recipe2_hooks_assume() {
        let mut vm = loaded_guest();
        let (draw, key, load, strings) = recipe2_bases(&mut vm);
        let sites = patch_recipe2_sites(&mut vm, draw, key, load, strings).expect("shipped guest");
        assert_eq!(
            sites,
            vec![
                (draw + R2_DRAW_BOUND, Patch::Recipe2Rows),
                (key + R2_KEY_UP, Patch::Recipe2WrapUp),
                (key + R2_KEY_DOWN, Patch::Recipe2WrapDown),
                (key + R2_KEY_HIT, Patch::Recipe2HitRows),
                (key + R2_KEY_STATE, Patch::Recipe2State),
            ]
        );
        for (base, off) in [
            (draw, R2_DRAW_BOUND),
            (draw, R2_DRAW_FIRST),
            (draw, R2_DRAW_INC),
            (draw, R2_DRAW_CACHE),
            (draw, R2_DRAW_LOOP),
            (draw, R2_DRAW_SEL),
            (draw, R2_DRAW_CALL),
            (key, R2_KEY_UP),
            (key, R2_KEY_UP_TEST),
            (key, R2_KEY_DOWN),
            (key, R2_KEY_DOWN_SHIFT),
            (key, R2_KEY_DOWN_WRAP),
            (key, R2_KEY_HIT),
            (key, R2_KEY_HIT_STEP),
            (key, R2_KEY_STATE),
            (key, R2_KEY_LOAD),
        ] {
            let was = vm.mem.r32(base + off);
            vm.mem.w32(base + off, was ^ 1);
            assert!(
                patch_recipe2_sites(&mut vm, draw, key, load, strings).is_none(),
                "a changed word at +{off:#x} must refuse"
            );
            vm.mem.w32(base + off, was);
        }
        assert!(patch_recipe2_sites(&mut vm, draw, key, load, strings).is_some());
    }

    fn patch_recipe2_sites(
        vm: &mut Vm,
        draw: u32,
        key: u32,
        load: u32,
        strings: u32,
    ) -> Option<Vec<(u32, Patch)>> {
        recipe2_sites(draw, key, load, strings, &mut vm.mem)
    }

    /// The fifth label pointer lands inside the draw function's own frame, in
    /// a word nothing else reads or writes.
    ///
    /// This is the whole reason one more row is free. The loop caches each
    /// `getInCodeString` result at `sp+0x40 + 4i` and the tail re-reads
    /// `sp+0x50 + 4*sel - 0x10` to redraw the selected one; with five rows the
    /// last of those is `sp+0x50`, which is the final word of `sub sp,sp,#0x54`
    /// and is touched by nothing else in the function.
    #[test]
    fn the_fifth_label_slot_is_inside_the_frame_and_otherwise_unused() {
        let mut vm = loaded_guest();
        let draw = vm.symbol("NpcUI_DrawMerChant_Main").expect(".symtab");
        // Instructions only: the words from the `pop` on are the literal pool.
        let size = 0x1c0;
        // `sub sp, sp, #0x54` at +0xc.
        assert_eq!(vm.mem.r32(draw + 0xc), 0xe24d_d054, "the frame size moved");
        // Cache base `add r2, sp, #0x40` at +0x2c, and the tail's `add r2, sp,
        // #0x50` — five words apart is exactly five rows.
        assert_eq!(vm.mem.r32(draw + 0x2c), 0xe28d_2040, "the cache base moved");
        assert_eq!(vm.mem.r32(draw + R2_DRAW_SEL), R2_DRAW_SEL_WORD);
        // Nothing else in the function addresses sp+0x40..0x53. Every `sp`
        // form in this body is either `[sp, #imm]` or `add rX, sp, #imm`; the
        // ones that fall in that window are the two above.
        for off in (0..size).step_by(4) {
            let w = vm.mem.r32(draw + off);
            let at = draw + off;
            if at == draw + 0x2c || at == draw + R2_DRAW_SEL {
                continue;
            }
            // ldr/str rX, [sp, #imm12]
            if w & 0x0f2f_0000 == 0x050d_0000 {
                assert!(w & 0xfff < 0x40, "+{off:#x} touches the label cache");
            }
            // add rX, sp, #imm (rotate 0). `add sp, sp, #0x54` is the frame
            // teardown, not a pointer, so Rd = sp is not one of these.
            if w & 0x0fff_0f00 == 0x028d_0000 && w >> 12 & 0xf != 13 {
                assert!(w & 0xff < 0x40, "+{off:#x} points into the label cache");
            }
        }
    }

    /// 레시피2 is well-formed EUC-KR and carries no `%`.
    ///
    /// The label goes through `HF_DrawTextOutlineH2`, not `MC_knlSprintk`, but
    /// the same rule as every other host string applies: a `String` round trip
    /// would turn the three syllables into U+FFFD and the font would draw the
    /// result as plausible Hangul.
    #[test]
    fn the_recipe2_label_is_the_three_syllables_and_a_digit() {
        assert_eq!(RECIPE2_LABEL, b"\xb7\xb9\xbd\xc3\xc7\xc7\x32");
        let mut i = 0;
        let mut syllables = 0;
        while i < RECIPE2_LABEL.len() {
            let b = RECIPE2_LABEL[i];
            if b < 0x80 {
                assert_eq!(b, b'2', "the only ASCII in the label is the digit");
                i += 1;
            } else {
                assert!((0x81..=0xfe).contains(&b), "byte {i} is not a lead byte");
                let t = RECIPE2_LABEL[i + 1];
                assert!((0x41..=0xfe).contains(&t), "byte {} is not a trail", i + 1);
                i += 2;
                syllables += 1;
            }
        }
        assert_eq!(syllables, 3, "레시피 is three syllables");
        assert!(!RECIPE2_LABEL.contains(&b'%'));
    }

    /// The label swap is asked for at exactly one return address, and it is
    /// inside the draw loop.
    #[test]
    fn the_label_swap_is_gated_on_the_draw_loops_own_call() {
        let mut vm = loaded_guest();
        let (draw, _, _, strings) = recipe2_bases(&mut vm);
        assert_eq!(
            branch_target(draw + R2_DRAW_CALL, &mut vm.mem, true),
            Some(strings),
            "the label call moved"
        );
        assert_eq!(recipe2_label_lr(draw), draw + R2_DRAW_CALL + 4);
        // And the id the fifth iteration asks for follows from the first id
        // and the bound this answers: 0x2e4 + 1 .. 0x2e9 inclusive.
        assert_eq!(RECIPE2_LABEL_ID, 0x2e4 + RECIPE2_ROWS);
    }

    /// `merchantState` is dispatched through a five-entry jump table, which is
    /// why the fifth row borrows state 2 instead of taking a sixth.
    ///
    /// If this ever grows an entry the remap becomes unnecessary — and if it
    /// ever shrinks, the remap is actively wrong.
    #[test]
    fn the_merchant_state_dispatcher_has_no_room_for_a_sixth_state() {
        let mut vm = loaded_guest();
        let key = vm.symbol("NpcUI_merchantKeyPressed").expect(".symtab");
        // +0x60  ldrsb r3, [r3] / +0x64  cmp r3, #4 / +0x68  addls pc, pc, ...
        assert_eq!(vm.mem.r32(key + 0x60), 0xe1d3_30d0, "the state load moved");
        assert_eq!(vm.mem.r32(key + 0x64), 0xe353_0004, "the state bound moved");
        assert_eq!(vm.mem.r32(key + 0x68), 0x908f_f103, "the jump table moved");
        // The bound the table itself carries, read out of the `cmp`.
        let bound = vm.mem.r32(key + 0x64) & 0xff;
        assert_eq!(bound, 4, "five states, 0..4");
        assert_eq!(u32::from(RECIPE2_STATE_RAW), RECIPE2_ROW + 1, "state is cursor + 1");
        assert!(u32::from(RECIPE2_STATE_RAW) > bound, "a sixth state falls off the table");
        assert!(u32::from(RECIPE2_STATE) <= bound, "the state it borrows is one the table has");
    }

    /// The two `cmp` substitutions leave the flags an ARM `cmp` would.
    ///
    /// Checked against the cases that actually run — the cursor walking the
    /// five rows — plus the signed-overflow corner, since one of the two
    /// consumers is `movgt`, which reads N, V *and* Z.
    #[test]
    fn the_substituted_compares_set_the_flags_the_hardware_would() {
        let mut vm = loaded_guest();
        let mode_was = vm.cpu.reg_get(Mode::User, reg::CPSR) & 0x1f;
        for (a, b) in [
            (0u32, 5u32),
            (4, 5),
            (5, 5),
            (6, 5),
            (1 << 24, 4 << 24),
            (4 << 24, 4 << 24),
            (5 << 24, 4 << 24),
            (0x8000_0000, 1),
            (0, 0x8000_0000),
        ] {
            vm.set_cmp_flags(a, b);
            let cpsr = vm.cpu.reg_get(Mode::User, reg::CPSR);
            let (n, z, c, v) = (
                cpsr >> 31 & 1 == 1,
                cpsr >> 30 & 1 == 1,
                cpsr >> 29 & 1 == 1,
                cpsr >> 28 & 1 == 1,
            );
            let res = a.wrapping_sub(b);
            assert_eq!(n, res >> 31 == 1, "N for {a:#x} - {b:#x}");
            assert_eq!(z, res == 0, "Z for {a:#x} - {b:#x}");
            assert_eq!(c, a >= b, "C for {a:#x} - {b:#x}");
            assert_eq!(v, (a as i32).checked_sub(b as i32).is_none(), "V for {a:#x} - {b:#x}");
            // Signed GT — what `movgt` asks — must agree with plain Rust.
            assert_eq!(!z && n == v, (a as i32) > (b as i32), "GT for {a:#x} - {b:#x}");
            // And the mode bits are untouched, or `RegFile::set` would
            // re-bank the registers underneath the hook.
            assert_eq!(cpsr & 0x1f, mode_was, "the mode bits moved");
        }
    }

    // ---- 펫 「스마트모드」 ---------------------------------------------------

    fn pet_smart_bases(vm: &mut Vm) -> [u32; 8] {
        [
            "Pet_Process",
            "Pet_move_process",
            "HERO_Check_Autotarget",
            "GameMenuCharPetKey",
            "draw_pet_info",
            "draw_char_pet",
            "Pet_set_condition",
            "getInCodeString",
        ]
        .map(|n| vm.symbol(n).expect(".symtab"))
    }

    fn pet_sites(vm: &mut Vm, b: &[u32; 8]) -> Option<Vec<(u32, Patch)>> {
        pet_smart_sites(b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], &mut vm.mem)
    }

    /// Every word the eight hooks stand on or read past, and the three `bl`s
    /// that say what the code around them is for; changing any one refuses.
    #[test]
    fn the_pet_ai_and_its_menu_are_where_the_smart_hooks_assume() {
        let mut vm = loaded_guest();
        let b = pet_smart_bases(&mut vm);
        let sites = pet_sites(&mut vm, &b).expect("shipped guest");
        assert_eq!(sites.len(), 9);
        assert_eq!(sites[0], (b[0] + PET_PICK, Patch::PetSmartPick));
        assert!(sites.contains(&(b[1] + PET_MOVE_GATE, Patch::PetSmartMove)));
        assert!(sites.contains(&(b[1] + PET_MOVE_TREASURE, Patch::PetSmartMove)));
        assert!(sites.contains(&(b[2] + PET_TARGET_GATE, Patch::PetSmartTarget)));
        assert!(sites.contains(&(b[3] + PET_MENU_UP, Patch::PetSmartWrapUp)));
        assert!(sites.contains(&(b[3] + PET_MENU_DOWN, Patch::PetSmartWrapDown)));
        for off in [PET_DESC_PICK, PET_DESC_SHOW, PET_DESC_DRAW] {
            assert!(sites.contains(&(b[4] + off, Patch::PetSmartDesc)), "+{off:#x}");
        }
        for (base, off) in [
            (b[0], PET_PICK),
            (b[0], PET_PICK_CMP),
            (b[0], PET_PICK_TABLE),
            (b[1], PET_MOVE_GATE),
            (b[1], PET_MOVE_STOP),
            (b[1], PET_MOVE_TREASURE),
            (b[1], PET_MOVE_IS_TREASURE),
            (b[2], PET_TARGET_GATE),
            (b[2], PET_TARGET_IS_TREASURE),
            (b[3], PET_MENU_UP),
            (b[3], PET_MENU_DOWN),
            (b[3], PET_MENU_UP_SET),
            (b[3], PET_MENU_DOWN_SET),
            (b[4], PET_DESC_PICK),
            (b[4], PET_DESC_SHOW),
            (b[4], PET_DESC_DRAW),
            (b[4], PET_DESC_CMP),
            (b[4], PET_DESC_TABLE),
            (b[4], PET_DESC_CALL),
            (b[5], PET_LABEL_CALL),
        ] {
            let was = vm.mem.r32(base + off);
            vm.mem.w32(base + off, was ^ 1);
            assert!(pet_sites(&mut vm, &b).is_none(), "a changed word at +{off:#x} must refuse");
            vm.mem.w32(base + off, was);
        }
        assert!(pet_sites(&mut vm, &b).is_some());
    }

    /// The two jump tables really are four arms wide — the whole reason the
    /// fifth value is answered as one of the four — and the `Pet_Process`
    /// table's arms are the ones the module comment names.
    #[test]
    fn the_pet_dispatch_tables_have_four_arms_and_we_know_which_is_which() {
        let mut vm = loaded_guest();
        let b = pet_smart_bases(&mut vm);
        // `cmp r3, #3` bounds both tables at 0..3.
        assert_eq!(vm.mem.r32(b[0] + PET_PICK_CMP) & 0xff, 3);
        assert_eq!(vm.mem.r32(b[4] + PET_DESC_CMP) & 0xff, 3);
        assert!(u32::from(PET_SMART) > 3, "the smart value is the one past the table");
        assert!(PET_TREASURE <= 3 && pet_smart_combat() <= 3);
        // `addls pc, pc, r3, lsl #2`: entry v is at table + 8 + 4v. The
        // treasure arm (2) must call `HERO_Check_Treasure`, the solo arm (0)
        // `HERO_Check_Autotarget`, and the co-op arm (1) neither — it copies
        // the hero's target.
        let table = b[0] + PET_PICK_TABLE;
        let arm =
            |vm: &mut Vm, v: u32| branch_target(table + 8 + 4 * v, &mut vm.mem, false).unwrap();
        let treasure = vm.symbol("HERO_Check_Treasure").unwrap();
        let autotarget = vm.symbol("HERO_Check_Autotarget").unwrap();
        let calls = |vm: &mut Vm, from: u32, words: u32, what: u32| {
            (0..words).any(|i| branch_target(from + 4 * i, &mut vm.mem, true) == Some(what))
        };
        let (a0, a1, a2) = (arm(&mut vm, 0), arm(&mut vm, 1), arm(&mut vm, 2));
        // The arms are laid out 2 < 1 < 0 in the image; each is scanned up to
        // the next one's start so a hit is its own and not its neighbour's.
        assert!(a2 < a1 && a1 < a0, "the arms moved");
        assert!(calls(&mut vm, a2, (a1 - a2) / 4, treasure), "arm 2 is not the treasure arm");
        assert!(calls(&mut vm, a0, 0x20, autotarget), "arm 0 is not the solo arm");
        let co_op = (a0 - a1) / 4;
        assert!(!calls(&mut vm, a1, co_op, treasure) && !calls(&mut vm, a1, co_op, autotarget));
    }

    /// The label and description are well-formed EUC-KR, the description
    /// carries no `%` (it is a `MC_knlSprintk` format), and both fit where the
    /// shipped ones go: the description into the 0xc8-byte per-value buffer.
    #[test]
    fn the_smart_strings_are_euc_kr_and_fit() {
        for (name, s) in [("label", PET_SMART_LABEL), ("desc", PET_SMART_DESC)] {
            let mut i = 0;
            while i < s.len() {
                let c = s[i];
                if c < 0x80 {
                    assert!(c.is_ascii_graphic() || c == b' ', "{name} byte {i}");
                    i += 1;
                } else {
                    assert!((0x81..=0xfe).contains(&c), "{name} byte {i} is not a lead");
                    let t = s[i + 1];
                    assert!((0x41..=0xfe).contains(&t), "{name} byte {} is not a trail", i + 1);
                    i += 2;
                }
            }
            assert!(!s.contains(&b'%'), "{name}");
        }
        assert_eq!(PET_SMART_LABEL, b"\xbd\xba\xb8\xb6\xc6\xae", "스마트");
        assert_eq!(PET_SMART_DESC[0], b'r', "the colour escape every shipped one starts with");
        assert!(PET_SMART_DESC.len() < 0xc8);
        // And the ids they are served under are the fifth of each shipped run.
        assert_eq!(PET_LABEL_ID, 0x12f + 4);
        assert_eq!(PET_DESC_ID, 0x121);
    }

    /// The combat fallback is one of the two fighting arms whatever the knob
    /// says — 2 or 3 would make a 스마트 pet that never fights.
    #[test]
    fn the_smart_fallback_is_always_a_fighting_arm() {
        assert!(pet_smart_combat() <= 1);
    }

    /// The message has to be EUC-KR and NUL-terminated, or the font draws
    /// plausible-looking nonsense instead (the trap `CLAUDE.md` documents).
    #[test]
    fn the_data_manage_message_is_well_formed_euc_kr() {
        let mut i = 0;
        let mut chars = 0;
        while i < DATA_MANAGE_MSG.len() {
            let b = DATA_MANAGE_MSG[i];
            if b == 0x20 || b == 0x2e {
                i += 1;
            } else {
                assert!((0x81..=0xfe).contains(&b), "byte {i} ({b:#04x}) is not a lead byte");
                let t = *DATA_MANAGE_MSG.get(i + 1).expect("lead byte without a trail");
                assert!((0x41..=0xfe).contains(&t), "byte {} ({t:#04x}) is not a trail", i + 1);
                i += 2;
                chars += 1;
            }
        }
        assert_eq!(chars, 11, "이용할 수 없는 기능입니다 is 11 hangul syllables");
        assert!(!DATA_MANAGE_MSG.contains(&b'%'), "setPopUp treats this as a format string");
    }

    /// Serving without moving PC is an infinite loop, not a no-op.
    ///
    /// The run loop `continue`s on `Ok(true)` and does **not** step, so an entry
    /// hook has to jump to LR itself. This shipped wrong once: nothing at boot
    /// asks for `PHONENUMBER`, so the first call would have been the handshake —
    /// the game would have frozen the moment 편지 was opened, with no crash
    /// report and nothing in the log.
    #[test]
    fn serving_the_phone_hook_returns_to_the_caller() {
        let mut vm = loaded_guest();
        let name_p = vm.heap.alloc(0x20).expect("guest heap");
        let out = vm.heap.alloc(0x40).expect("guest heap");

        assert!(ask_property(&mut vm, name_p, b"PHONENUMBER", out, 0x20));
        assert_eq!(
            vm.cpu.reg_get(Mode::User, reg::PC),
            0x0bad_0000,
            "a served entry hook must jump to LR"
        );

        // Declining must leave PC alone — the guest's own instruction runs.
        vm.cpu.reg_set(Mode::User, reg::PC, 0x0100_0000);
        assert!(!ask_property(&mut vm, name_p, b"PHONEMODEL", out, 0x20));
        assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), 0x0100_0000, "declining must not move PC");
    }

    /// `UM_Sleep` is still the busy-wait the sleep hook was written against,
    /// and a guest whose words moved refuses the install instead of sleeping
    /// at the wrong moment.
    #[test]
    fn the_um_sleep_loop_is_where_the_hook_thinks() {
        let mut vm = loaded_guest();
        let base = vm.symbol("UM_Sleep").expect("UM_Sleep is in .symtab");
        let ct = vm.symbol("MC_knlCurrentTime").expect("MC_knlCurrentTime is in .symtab");
        assert_eq!(
            um_sleep_site(base, ct, &mut vm.mem),
            Some(base + UM_SLEEP_LOOP),
            "the hook belongs on the loop's own bl"
        );
        // A different clock target is a different function — refuse.
        assert_eq!(um_sleep_site(base, ct + 4, &mut vm.mem), None);
        // And so is a loop whose duration no longer parks in r7:r6.
        let word = vm.mem.r32(base + 4);
        vm.mem.w32(base + 4, word ^ 1);
        assert_eq!(um_sleep_site(base, ct, &mut vm.mem), None);
        vm.mem.w32(base + 4, word);
        assert!(um_sleep_site(base, ct, &mut vm.mem).is_some(), "restored");
    }

    /// Serving a sleep must wait out only what is *left*, never restart the
    /// clock — the hook fires on every loop iteration, and sleeping the full
    /// duration twice is the bug that makes a 50 ms hit-stop take 100.
    ///
    /// Under the virtual clock the wait is a clock advance, which is also what
    /// keeps this test instant, and what a probe replay that reaches a
    /// hit-stop needs anyway: the shipped loop against a frozen clock would
    /// spin into the run budget and die.
    #[test]
    fn the_sleep_hook_waits_out_only_the_remainder_and_declines() {
        let mut vm = loaded_guest();
        let table = vm.heap.alloc(8).expect("guest heap");
        let slot = vm.heap.alloc(8).expect("guest heap");
        vm.mem.w32(table, slot);
        vm.cpu.reg_set(Mode::User, 4, table);
        vm.cpu.reg_set(Mode::User, 5, 0);
        vm.cpu.reg_set(Mode::User, reg::PC, 0x0100_0000);

        let put_start = |vm: &mut Vm, start: i64| {
            let s = vm.mem.r32(table);
            vm.mem.w32(s, start as u32);
            vm.mem.w32(s + 4, (start >> 32) as u32);
        };
        let ask = |vm: &mut Vm, ms: u32| {
            vm.cpu.reg_set(Mode::User, 7, ms);
            vm.cpu.reg_set(Mode::User, 6, 0);
        };

        // A sleep that just started waits the whole 50 ms.
        let t0 = vm.now_ms();
        put_start(&mut vm, t0);
        ask(&mut vm, 50);
        assert_eq!(vm.host_um_sleep(), Ok(false), "the guest's own loop must still exit");
        assert_eq!(vm.now_ms() - t0, 50, "the virtual clock advances by the wait");
        assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), 0x0100_0000, "declining must not move PC");
        assert_eq!((vm.sleep_host, vm.sleep_ms), (1, 50));

        // 30 ms already gone: only the 20 left are waited.
        let t1 = vm.now_ms();
        put_start(&mut vm, t1 - 30);
        ask(&mut vm, 50);
        assert_eq!(vm.host_um_sleep(), Ok(false));
        assert_eq!(vm.now_ms() - t1, 20);

        // Nothing left: decline without touching the clock, so the loop's own
        // compare — the code that actually owns the exit — decides.
        let t2 = vm.now_ms();
        put_start(&mut vm, t2 - 50);
        ask(&mut vm, 50);
        assert_eq!(vm.host_um_sleep(), Ok(false));
        assert_eq!(vm.now_ms(), t2, "an elapsed sleep is not ours");
        assert_eq!(vm.sleep_host, 2, "and it is not counted as served");

        // A start in the future is garbage; leave it to the guest.
        let future = vm.now_ms() + 1000;
        put_start(&mut vm, future);
        ask(&mut vm, 50);
        assert_eq!(vm.host_um_sleep(), Ok(false));
        assert_eq!(vm.now_ms(), t2);
    }

    /// The whole function, through the interpreter, with the patch installed:
    /// a real `UM_Sleep(70)` must come home with the virtual clock 70 ms on —
    /// against a frozen clock the shipped loop would spin into the run budget,
    /// so returning at all *is* the patch working end to end.
    #[test]
    fn a_real_um_sleep_call_comes_home_through_the_hook() {
        let mut vm = loaded_guest();
        let base = vm.symbol("UM_Sleep").expect("UM_Sleep is in .symtab");
        let t0 = vm.now_ms();
        vm.call(base, &[70, 0]).expect("UM_Sleep returns");
        assert_eq!(vm.now_ms() - t0, 70, "the wait became a clock advance");
        assert_eq!(vm.sleep_host, 1);
    }

    /// `PHONENUMBER` is answered; the eight other names the guest knows are not.
    ///
    /// The negative half is the point. This hook sits on a function the whole
    /// platform layer funnels through — the device profile, the IME, the auth
    /// callback — so answering one name too many would corrupt something far
    /// from here.
    #[test]
    fn the_phone_hook_answers_only_its_own_property() {
        let mut vm = loaded_guest();
        let name_p = vm.heap.alloc(0x20).expect("guest heap");
        let out = vm.heap.alloc(0x40).expect("guest heap");

        assert!(ask_property(&mut vm, name_p, b"PHONENUMBER", out, 0x20));
        assert_eq!(vm.mem.cstr_bytes(out), vm.identity.as_bytes());
        // The guest prepends '0' when the number lacks one; ours already has it,
        // so what the server sees is exactly what `identity.rs` minted.
        assert!(vm.identity.starts_with('0'), "identity must already be 0-prefixed");

        for other in [
            &b"PHONEMODEL"[..],
            b"MANUFACTURER",
            b"IMEI",
            b"IMSI",
            b"AIRPLANE_MODE",
            b"ROAMING_AREA",
            b"DS_LOCK",
            b"PLATFORMVERSION",
        ] {
            let served = ask_property(&mut vm, name_p, other, out, 0x20);
            assert!(!served, "{} must be left to the guest", String::from_utf8_lossy(other));
            assert_eq!(vm.mem.r8(out), 0, "declining must not write anything");
        }
    }

    /// A number that does not fit is refused outright.
    ///
    /// Truncating would be worse than declining: a half-written number is a
    /// *different* player to the server, so a save slot and a mailbox would
    /// silently belong to someone else.
    #[test]
    fn the_phone_hook_refuses_a_buffer_it_would_overrun() {
        let mut vm = loaded_guest();
        let name_p = vm.heap.alloc(0x20).expect("guest heap");
        let out = vm.heap.alloc(0x40).expect("guest heap");

        let tight = vm.identity.len() as u32; // no room for the NUL
        assert!(!ask_property(&mut vm, name_p, b"PHONENUMBER", out, tight));
        assert_eq!(vm.mem.r8(out), 0, "a refused call must not write");

        assert!(ask_property(&mut vm, name_p, b"PHONENUMBER", out, tight + 1));
        assert_eq!(vm.mem.cstr_bytes(out), vm.identity.as_bytes());
    }

    /// The name the hook keys on has to be the one the guest actually passes.
    #[test]
    fn the_property_name_is_the_one_getsktphonenumber_asks_for() {
        let vm = loaded_guest();
        // `getSKTPhoneNumber` loads it GOT-relative; the string itself is what
        // matters and it is in .rodata.
        let hay = std::fs::read("../payload/libchronosMian.so").expect("payload/ is committed");
        let needle = [PHONE_PROPERTY, b"\0"].concat();
        assert!(
            hay.windows(needle.len()).any(|w| w == needle),
            "PHONENUMBER is not a string in the shipped binary"
        );
        drop(vm);
    }

    /// Both 「내 ID」 hooks sit on the instruction they were written against,
    /// and the words they *resume into* are still there too.
    ///
    /// Each hook stands in for one instruction and then hands control back into
    /// the middle of a sequence — the key hook lets the guest's `cmp r3, #8`
    /// run, and the draw hook resumes at the grey test. A build where those
    /// moved would not be a no-op, it would be a jump into something else, so
    /// the site functions refuse and this pins what they check.
    #[test]
    fn the_my_id_hooks_are_on_the_instructions_they_were_written_against() {
        let mut vm = loaded_guest();
        let key = vm.symbol("MainMenuFrame_KeyPressed").expect("in .symtab");
        let draw = vm.symbol("MainMenuFrame_drawMainMenu").expect("in .symtab");

        assert_eq!(my_id_key_site(key, &mut vm.mem), Some(key + MY_ID_COMMIT));
        assert_eq!(my_id_draw_site(draw, &mut vm.mem), Some(draw + MY_ID_GREY));

        // `str r2, [r1]` — the commit — with the two loads that fill r1/r2 and
        // the compare that runs after we skip it.
        assert_eq!(vm.mem.r32(key + MY_ID_COMMIT - 0xc), MY_ID_LOAD_CURSOR_WORD);
        assert_eq!(vm.mem.r32(key + MY_ID_COMMIT - 8), MY_ID_LOAD_STATE_WORD);
        assert_eq!(vm.mem.r32(key + MY_ID_COMMIT), MY_ID_COMMIT_WORD);
        assert_eq!(vm.mem.r32(key + MY_ID_COMMIT + 4), MY_ID_AFTER_WORD);
        // That compare must branch away rather than fall through, or skipping
        // the store would leave the menu in some other arm.
        assert_eq!(
            vm.mem.r32(key + MY_ID_COMMIT + 8) >> 24,
            0x1a,
            "the instruction after `cmp r3,#8` is not a `bne`"
        );

        // The four words of the grey test.
        assert_eq!(vm.mem.r32(draw + MY_ID_GREY), MY_ID_GREY_WORD);
        assert_eq!(vm.mem.r32(draw + MY_ID_GREY + 4), MY_ID_GREY_NE_WORD);
        assert_eq!(vm.mem.r32(draw + MY_ID_GREY + 8), MY_ID_GREY_EQ_WORD);
        assert_eq!(vm.mem.r32(draw + MY_ID_GREY_TEST), MY_ID_GREY_TEST_WORD);
        // Resuming at the test only works because the two skipped words set no
        // flags — `movne`/`andeq` are both S=0 — and the test makes its own.
        for off in [MY_ID_GREY + 4, MY_ID_GREY + 8] {
            assert_eq!(vm.mem.r32(draw + off) & (1 << 20), 0, "+{off:#x} sets flags");
        }
        // And the tile index is in r7 two instructions earlier: `add r7, r4, r2`
        // (column + row*3), which is also what `getImage` is handed.
        assert_eq!(vm.mem.r32(draw + MY_ID_GREY - 0x10), 0xe084_7002, "r7 is not the tile index");

        // A drifted word must refuse, not install anyway.
        let saved = vm.mem.r32(key + MY_ID_COMMIT);
        vm.mem.w32(key + MY_ID_COMMIT, 0xe1a0_0000); // nop
        assert_eq!(my_id_key_site(key, &mut vm.mem), None, "a changed word must refuse");
        vm.mem.w32(key + MY_ID_COMMIT, saved);
        assert!(my_id_key_site(key, &mut vm.mem).is_some());
    }

    /// `_menuState` is reachable from exactly one literal in that function, so
    /// the store we stand in for is the only way this screen sets it.
    ///
    /// This is the load-bearing claim: skipping the store is only "the menu
    /// stays where it was" if nothing else in the same key event writes it.
    #[test]
    fn the_menu_commits_to_a_tile_in_exactly_one_place() {
        let mut vm = loaded_guest();
        let base = vm.symbol("MainMenuFrame_KeyPressed").expect("in .symtab");
        let size = vm.elf.sizes.get("MainMenuFrame_KeyPressed").copied().unwrap_or(0);
        assert!(size > 0);
        let state = vm.symbol("_menuState").expect("in .symtab");
        let got = vm.symbol("_GLOBAL_OFFSET_TABLE_").unwrap_or(0);
        assert!(got != 0, "the GOT symbol is what makes this scan possible");

        // Every `ldr rX, [pc, #imm]` in the function whose literal is the GOT
        // offset of `_menuState`.
        let mut loads = Vec::new();
        for off in (0..size).step_by(4) {
            let at = base + off;
            let w = vm.mem.r32(at);
            if w & 0x0e5f_0000 != 0x041f_0000 || (w >> 20) & 1 == 0 {
                continue;
            }
            let imm = w & 0xfff;
            let lit = if (w >> 23) & 1 == 1 { at + 8 + imm } else { at + 8 - imm };
            let slot = got.wrapping_add(vm.mem.r32(lit));
            if vm.mem.r32(slot) == state {
                loads.push(off);
            }
        }
        assert_eq!(
            loads.len(),
            1,
            "_menuState is reachable from {} literals in this function; \
             skipping one store no longer means the menu stays put",
            loads.len()
        );
    }

    /// The tile's two strings go into `MC_knlSprintk` as **format strings**
    /// (`setPopUp` and the label draw), and they are drawn by a renderer that
    /// eats twelve bytes silently.
    #[test]
    fn the_my_id_strings_are_safe_to_hand_the_guest() {
        for (what, s) in [
            ("label", MY_ID_LABEL),
            ("title", MY_ID_TITLE),
            ("none", MY_ID_NONE),
            ("set", ID_SET_MSG),
            ("taken", ID_TAKEN_MSG),
            ("refused", ID_REFUSED_MSG),
            ("offline", ID_OFFLINE_MSG),
        ] {
            assert!(!s.contains(&b'%'), "{what} would be read as a format string");
            assert!(!s.contains(&0), "{what} has an embedded NUL");
            // `;` is the renderer's line break and is legitimate at the end of
            // the title; nothing else from the escape table may appear.
            for &b in b"&|${}opqrst" {
                assert!(!s.contains(&b), "{what} contains the zero-width escape {:?}", b as char);
            }
            // Well-formed EUC-KR: every non-ASCII byte starts a pair.
            let mut i = 0;
            while i < s.len() {
                if s[i] < 0x80 {
                    i += 1;
                    continue;
                }
                assert!((0x81..=0xfe).contains(&s[i]), "{what}: {:#04x} is not a lead", s[i]);
                let t = *s.get(i + 1).unwrap_or_else(|| panic!("{what}: lead without a trail"));
                assert!((0x41..=0xfe).contains(&t), "{what}: {t:#04x} is not a trail");
                i += 2;
            }
        }
        assert_eq!(MY_ID_TITLE.last(), Some(&b';'), "the address needs its own line");
        // The unclaimed text is two lines like the claimed one, and its second
        // line is the touch target — if it collapsed to one line there would be
        // nothing under the title to press.
        assert_eq!(
            MY_ID_NONE.iter().filter(|&&b| b == b';').count(),
            1,
            "the unclaimed popup must be exactly two lines"
        );
        assert_ne!(MY_ID_NONE.last(), Some(&b';'), "its second line must not be empty");
        // What gets spliced on is the wire address, and it must not be able to
        // reintroduce a `%` — `wire_number` only ever copies digits and may
        // prepend '0'.
        let wire = crate::identity::wire_number(b"01077778888");
        assert!(wire.iter().all(u8::is_ascii_digit), "the address is digits only");
    }

    /// The IME contract the ID input stands on, read back out of the binary.
    ///
    /// Three separate claims, each of which would be a silent wrong ID if it
    /// drifted: the close callback accepts a cancel as if it were a commit, the
    /// edit buffer and caret are where `MX_imeGetString` says they are, and the
    /// property numbers are the ones that set text and length.
    #[test]
    fn the_ime_is_the_shape_the_id_input_assumes() {
        let mut vm = loaded_guest();
        let close = vm.symbol("onCloseIME_MainFrame").expect("in .symtab");
        // `cmp r0, #1` / `bls` — **0 (done) and 1 (cancelled) both fall through**
        // into the body. That is why `host_id_input_done` reads r0 itself; a
        // hook that did not would claim whatever the field was seeded with.
        assert_eq!(vm.mem.r32(close + 8), 0xe350_0001, "the flag test moved");
        assert_eq!(vm.mem.r32(close + 0x10) >> 24, 0x9a, "not a `bls`");
        assert_eq!(vm.mem.r32(close + 0x14), 0xe8bd_81f0, "the early return moved");

        // `MX_imeGetString`: `add r6, r4, #0x174` then `ldr r2, [r4, #0x274]`.
        // Both offsets are decoded rather than trusted, because zeroing the
        // wrong window would scribble on the instance.
        let get = vm.symbol("MX_imeGetString").expect("in .symtab");
        let add_buf = vm.mem.r32(get + 0x18);
        assert_eq!(add_buf & 0xffff_f000, 0xe284_6000, "not `add r6, r4, #imm`");
        // Rotate-immediate: 0x174 is encoded as 0x5d rotated right by 2*11.
        let imm = add_buf & 0xff;
        let rot = ((add_buf >> 8) & 0xf) * 2;
        assert_eq!(imm.rotate_right(rot), IME_BUF, "the edit buffer moved");
        let ldr_caret = vm.mem.r32(get + 0x28);
        assert_eq!(ldr_caret & 0xffff_f000, 0xe594_2000, "not `ldr r2, [r4, #imm]`");
        assert_eq!(ldr_caret & 0xfff, IME_CARET, "the caret moved");
        assert!(IME_BUF_LEN >= 0x64, "the buffer is smaller than the guest's own copy");

        // `MX_imeSetProperty`'s jump table: entry 2 is the one that `strcpy`s
        // into both copies, entry 3 the one that stores a length.
        let prop = vm.symbol("MX_imeSetProperty").expect("in .symtab");
        // `add pc, pc, r0, lsl #2` sits at +0x14, and PC reads as +8, so entry
        // zero is the word at +0x1c.
        let table = prop + 0x1c;
        let target = |vm: &mut Vm, n: u32| -> u32 {
            let at = table + 4 * n;
            let off = ((vm.mem.r32(at) << 8) as i32 >> 8) as u32;
            at.wrapping_add(8).wrapping_add(off << 2)
        };
        let text = target(&mut vm, IME_PROP_TEXT);
        assert_eq!(vm.mem.r32(text), 0xe351_0000, "property 2 does not start `cmp r1, #0`");
        // `add r4, r6, #0x174` / `add r0, r6, #0x74` — it copies into **both**
        // the edit buffer and the restore copy, which is why setting it to an
        // empty string leaves the field looking empty.
        let dec_add = |w: u32| (w & 0xff).rotate_right(((w >> 8) & 0xf) * 2);
        assert_eq!(vm.mem.r32(text + 0x10) & 0xffff_f000, 0xe286_4000);
        assert_eq!(dec_add(vm.mem.r32(text + 0x10)), IME_BUF, "property 2 is not the text setter");
        assert_eq!(vm.mem.r32(text + 0x14) & 0xffff_f000, 0xe286_0000);
        assert_eq!(dec_add(vm.mem.r32(text + 0x14)), 0x74, "property 2 is not the text setter");
        let len = target(&mut vm, IME_PROP_MAXLEN);
        assert_eq!(vm.mem.r32(len + 0xc), 0xe583_1278, "property 3 is not the length setter");
    }

    /// The popup's box and its OK button are where we think they are.
    ///
    /// `host_my_id_popup_touch` treats "inside the box but above OK" as a press
    /// on the address. Both rectangles come from the guest, and if either moved
    /// the touch target would silently drift onto the OK button — the popup
    /// would then reopen the keyboard instead of closing, with no way out.
    #[test]
    fn the_popup_box_and_its_button_are_where_the_touch_target_assumes() {
        let mut vm = loaded_guest();
        let draw = vm.symbol("drawPopWindow").expect("in .symtab");
        let key = vm.symbol("PopWindowKey").expect("in .symtab");

        // `drawPopWindow`: sub r0,r0,#0x58 / sub r1,r1,#0x32 / mov r2,#0xb0.
        assert_eq!(vm.mem.r32(draw + 0x38), 0xe2400058, "box x offset moved");
        assert_eq!(vm.mem.r32(draw + 0x3c), 0xe2411032, "box y offset moved");
        assert_eq!(vm.mem.r32(draw + 0x28), 0xe3a020b0, "box width moved");
        assert_eq!(-POP_BOX_DX, 0x58);
        assert_eq!(-POP_BOX_DY, 0x32);
        assert_eq!(POP_BOX_W, 0xb0);
        // and both halves come from `_lcdG+0x4c` / `+0x50`, halved.
        assert_eq!(vm.mem.r32(draw + 0x20), 0xe594004c, "width is not _lcdG+0x4c");
        assert_eq!(vm.mem.r32(draw + 0x24), 0xe5941050, "height is not _lcdG+0x50");

        // `PopWindowKey`: the OK rect is `w/2 - 0x14, h/2 + 0x1e, 0x28, 0x12`.
        assert_eq!(vm.mem.r32(key + 0x8c), 0xe2422014, "OK x offset moved");
        assert_eq!(vm.mem.r32(key + 0x90), 0xe283301e, "OK y offset moved");
        assert_eq!(POP_OK_DY, 0x1e, "our target must stop where OK starts");
        // The touch arm is selected by `cmp r0, #0x13`.
        assert_eq!(vm.mem.r32(key + 0x2c), 0xe3500013, "the touch event code moved");
        assert_eq!(TOUCH_EVENT, 0x13);
        // And the caller closes the popup exactly when this answers nonzero,
        // which is what makes answering 1 mean "dismissed".
        let mm = vm.symbol("MainMenuFrame_KeyPressed").expect("in .symtab");
        assert_eq!(vm.mem.r32(mm + 0x250), 0xebffe272, "not the `bl PopWindowKey`");
        assert_eq!(vm.mem.r32(mm + 0x254), 0xe3500000, "no `cmp r0, #0` after it");
        assert_eq!(vm.mem.r32(mm + 0x258), 0x15c8708c, "no conditional close after it");
    }

    /// The bypass sits on `getSKTPhoneNumber`'s entry, and the guest's own
    /// rebuild is what it exists to skip.
    ///
    /// This is the hook that stopped 홍길동 arriving as `0홍길동`. Without it
    /// the tile, the handshake and the control-channel claim disagree about the
    /// player's own name.
    #[test]
    fn the_identity_reaches_the_server_the_way_it_was_typed() {
        let mut vm = loaded_guest();
        let base = vm.symbol("getSKTPhoneNumber").expect("in .symtab");
        assert_eq!(vm.mem.r32(base), SKT_PHONE_WORD, "the prologue moved");
        assert_eq!(vm.mem.r32(base + 4), SKT_PHONE_SUB_WORD, "the frame size moved");
        // `mov sl, r0` — the destination is the first argument.
        assert_eq!(vm.mem.r32(base + 0x44), 0xe1a0a000, "r0 is not the destination");
        // `str r4, [sb]` right after the sprintk — the length is the second.
        assert_eq!(vm.mem.r32(base + 0x164), 0xe5894000, "r1 is not the length out-param");

        // And the transform we are skipping really does prepend a `'0'` to a
        // name — that is the whole reason this hook exists.
        let name = [0xC8, 0xAB, 0xB1, 0xE6, 0xB5, 0xBF]; // 홍길동
        assert_eq!(
            crate::identity::wire_number(&name)[0],
            b'0',
            "the shipped rebuild no longer prepends; this hook may be unnecessary"
        );
        assert_eq!(&crate::identity::wire_number(&name)[1..], &name[..]);
        // A number that already starts with '0' and has no eaten zero passes
        // through either way, which is why no live mailbox key moves.
        for n in [&b"01077778888"[..], b"01098765432", b"01055556666"] {
            assert_eq!(crate::identity::wire_number(n), n.to_vec(), "{:?} moves", n);
        }
    }

    /// A claimed name goes out byte for byte — no `'0'`, right length, and the
    /// hook returns to its caller.
    #[test]
    fn a_claimed_name_is_written_verbatim_with_its_length() {
        let mut vm = loaded_guest();
        let garum = [0xB0u8, 0xA1, 0xB8, 0xA7]; // 가름
        vm.ident.propose(&garum).expect("a legal name");
        vm.ident.confirm();
        assert_eq!(vm.ident.wire(), &garum[..]);

        let out = vm.heap.alloc(0x40).expect("guest heap");
        let len_out = vm.heap.alloc(4).expect("guest heap");
        for i in 0..0x20 {
            vm.mem.w8(out + i, 0xEE); // poison, so a short write shows up
        }
        vm.mem.w32(len_out, 0xDEAD_BEEF);
        vm.cpu.reg_set(Mode::User, 0, out);
        vm.cpu.reg_set(Mode::User, 1, len_out);
        vm.cpu.reg_set(Mode::User, reg::LR, 0x1234_5678);
        vm.cpu.reg_set(Mode::User, reg::PC, 0x1111_0000);

        assert!(vm.host_skt_phone_number().expect("no fault"), "the hook must serve");
        assert_eq!(vm.mem.cstr_bytes(out), garum.to_vec(), "the name was reshaped");
        assert_eq!(vm.mem.r8(out + garum.len() as u32), 0, "no NUL");
        assert_eq!(vm.mem.r32(len_out), garum.len() as u32, "wrong length");
        assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), 0x1234_5678, "did not return");

        // Serving means returning: a handler that leaves PC alone spins on its
        // own instruction forever (`CLAUDE.md`, and versionCode 39 shipped it).
        //
        // And a name that cannot fit is declined rather than truncated — half a
        // name is a *different player* to the server.
        let long: Vec<u8> = vec![b'A'; crate::identity::HANDSHAKE_FIELD];
        vm.ident.propose(&long).ok();
        if vm.ident.pending().is_some() {
            vm.ident.confirm();
            vm.cpu.reg_set(Mode::User, reg::PC, 0x1111_0000);
            assert!(
                !vm.host_skt_phone_number().expect("no fault"),
                "an over-long name must decline"
            );
            assert_eq!(
                vm.cpu.reg_get(Mode::User, reg::PC),
                0x1111_0000,
                "declining must not move PC"
            );
        }
    }

    /// The letter's recipient input is the one this hook was written against,
    /// and the strings it replaces are the ones it thinks they are.
    #[test]
    fn the_letter_asks_for_a_number_in_exactly_the_places_we_replace() {
        let mut vm = loaded_guest();
        let send = vm.symbol("MainMenuLetterSendKey").expect("in .symtab");
        assert!(letter_input_site(send, &mut vm.mem), "the InputInit call moved");
        // `strh r5, [r3, #0x4e]` — the last of the eight stores that clear the
        // recipient field, which is what says this call is the recipient one.
        assert_eq!(vm.mem.r32(send + LETTER_INPUT_CALL - 4), LETTER_INPUT_CALL_PREV);
        // All eight words before the call are `strh r5, [r3, #imm]`, clearing
        // `_mailBox+0x42 .. +0x50`. STRH immediate splits its offset across two
        // nibbles, so the mask keeps `cond/opcode/Rn/Rd` and the `1011` that
        // says halfword, and drops both halves of the offset.
        let mut cleared = Vec::new();
        for k in 1..=8u32 {
            let w = vm.mem.r32(send + LETTER_INPUT_CALL - 4 * k);
            assert_eq!(
                w & 0xffff_f0f0,
                0xe1c3_50b0,
                "the word {k} before the call is not a `strh r5, [r3, #imm]` ({w:#010x})"
            );
            cleared.push(((w >> 4) & 0xf0) | (w & 0xf));
        }
        cleared.sort_unstable();
        assert_eq!(
            cleared,
            vec![0x42, 0x44, 0x46, 0x48, 0x4a, 0x4c, 0x4e, 0x50],
            "the cleared field is not the recipient's"
        );

        // `InputInit`'s mode-0 arm: mask 8, cap 0xb, and the `"010"` default.
        let init = vm.symbol("InputInit").expect("in .symtab");
        assert_eq!(vm.mem.r32(init + 0x6c), 0xe3a01008, "the letter mask is not 8");
        assert_eq!(vm.mem.r32(init + 0x74), 0xe3a0100b, "the letter cap is not 0xb");
        assert_eq!(vm.mem.r32(init + 0x84), 0xe3a00002, "property 2 is not the initial text");
        assert_eq!(IME_PROP_MASK, 0);
        assert_eq!(IME_MASK_NAME, 0xe, "the pet arm's mask is what we widen to");
        assert_eq!(vm.mem.r32(init + 0xa8), 0xe3a0100e, "the pet mask is not 0xe");

        // The default really is `"010"`: `add r1, r4, lit` off the GOT.
        let got = vm.symbol("_GLOBAL_OFFSET_TABLE_").expect("in .symtab");
        let ldr = vm.mem.r32(init + 0x80);
        let lit = vm.mem.r32(init + 0x80 + 8 + (ldr & 0xfff));
        let at = got.wrapping_add(lit);
        assert_eq!(vm.mem.cstr_bytes(at), b"010".to_vec(), "the shipped default is not 010");

        // The two strings we swap say what we think they say.
        // Ask the guest rather than indexing a table address written down here
        // — but our own `getInCodeString` hook is live by default now, and it
        // would answer with the replacement and make this assertion circular.
        // So lift it for the duration, exactly as the radial-blur test lifts
        // its own patch to use the shipped body as the answer key.
        let get = vm.symbol("getInCodeString").expect("in .symtab");
        let saved = vm.patches.clone();
        vm.patches.retain(|(_, p)| !matches!(p, Patch::InCodeString));
        assert!(vm.patches.len() < saved.len(), "the string hook was not installed");
        for (id, needle) in
            [(LETTER_ASK_ID_ID, &b"\xc0\xfc\xc8\xad"[..]), (LETTER_NO_ID_ID, b"\xc6\xf9")]
        {
            let p = vm.call(get, &[id]).expect("the shipped lookup");
            let s = vm.mem.cstr_bytes(p);
            assert!(
                s.windows(needle.len()).any(|w| w == needle),
                "string {id:#x} is not the phone-number one: {s:x?}"
            );
        }
        vm.patches = saved;
        // And ours are well-formed EUC-KR with no `%`.
        for (what, s) in [("ask", LETTER_ASK_ID), ("none", LETTER_NO_ID)] {
            assert!(!s.contains(&b'%'), "{what} would be read as a format string");
            let mut i = 0;
            while i < s.len() {
                if s[i] < 0x80 {
                    i += 1;
                    continue;
                }
                assert!((0x81..=0xfe).contains(&s[i]), "{what}: bad lead");
                assert!((0x41..=0xfe).contains(s.get(i + 1).expect("trail")), "{what}: bad trail");
                i += 2;
            }
        }
    }

    /// Every knob read through [`knob`] must also have an `option_env!` arm.
    ///
    /// `knob()` reads the environment first and a hand-maintained `match` of
    /// `option_env!` arms second. **An APK has no environment**, so a knob
    /// missing from that list takes its compiled default on a device no matter
    /// what the build sets — the switch exists in the source, is documented,
    /// and does nothing.
    ///
    /// This has now happened twice for real. `CS_IME_H`/`CS_IME_DEBUG` shipped
    /// completely inert. Then `CS_PHONE`, `CS_DATA_MANAGE` and `CS_SMS_POPUP`
    /// shipped the same way — the identity hook, the 데이터 관리 block and the
    /// SMS-consent skip all had no way back. The defaults happened to be the
    /// wanted values, so nothing looked wrong; the cost was that there was no
    /// A/B knob for any of them.
    ///
    /// `cs_core/build.rs` cannot cover this: it scans for `option_env!`
    /// literals, so a knob missing from the list has no literal to scan.
    #[test]
    fn every_knob_we_read_is_also_baked_in() {
        // **Every** source file in the crate, read off disk rather than from a
        // list written here: `knob` is not local to this one (the run loop
        // reads `CS_WATCH_MEM` from lib.rs, the code cave its feature knobs
        // from cave.rs, the chest its size from vault.rs), and a hand-kept
        // list of files to scan would be one more thing that silently loses an
        // entry — which is the exact failure this test exists to catch.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut src = String::new();
        let mut files = 0;
        for e in std::fs::read_dir(&dir).expect("the crate's own source") {
            let p = e.expect("readable").path();
            if p.extension().is_some_and(|x| x == "rs") {
                src.push_str(&std::fs::read_to_string(&p).expect("readable"));
                files += 1;
            }
        }
        assert!(files > 10, "only {files} source files found; the scan is not working");
        let src = &src;
        let names = |marker: &str| -> std::collections::BTreeSet<String> {
            let mut out = std::collections::BTreeSet::new();
            for (at, _) in src.match_indices(marker) {
                let rest = src[at + marker.len()..].trim_start();
                let Some(rest) = rest.strip_prefix('(') else { continue };
                let rest = rest.trim_start();
                let Some(rest) = rest.strip_prefix('"') else { continue };
                let Some(end) = rest.find('"') else { continue };
                out.insert(rest[..end].to_owned());
            }
            out
        };
        let read = names("knob");
        let baked = names("option_env!");
        let missing: Vec<_> = read.difference(&baked).cloned().collect();
        assert!(
            missing.is_empty(),
            "read through knob() but not baked in — inert in a release APK: {missing:?}"
        );
        // The other direction only costs a stale rebuild, but a dead arm means
        // the list has drifted from what the code reads. `CS_STRIKE_Y` is read
        // with its own inline `env::var` + `option_env!` pair rather than
        // through `knob()`, so count that as a reader too.
        let mut readers = read.clone();
        readers.extend(names("std::env::var"));
        readers.extend(names("env::var"));
        let stale: Vec<_> = baked.difference(&readers).cloned().collect();
        assert!(stale.is_empty(), "an option_env! arm nothing reads: {stale:?}");
        assert!(read.len() > 20, "the scan found {} knobs; it is not working", read.len());
    }

    #[test]
    fn the_guest_agrees_with_our_radial_blur() {
        let mut vm = loaded_guest();
        // The shipped configuration: centred on the screen, no falloff, full
        // ray. `CS_BOSS_ZOOM_R` and `CS_BOSS_ZOOM_STRENGTH` are deliberately a
        // different image, and the tests below are the ones that pin *those*.
        vm.zoom_r_pct = 0;
        vm.zoom_strength = 100;
        let blur = vm.symbol("DM_RadialBlurLcd565").expect("in .symtab");
        let lcd_g = vm.symbol("_lcdG").expect("in .symtab");

        for &(w, h) in &[(37i32, 23i32), (16, 16), (64, 31), (9, 40)] {
            let bytes = (w * h) as u32 * 2;
            let src_px = vm.heap.alloc(bytes).expect("guest heap");
            let dst_px = vm.heap.alloc(bytes).expect("guest heap");
            let src_fb = vm.heap.alloc(0x18).expect("guest heap");
            let dst_fb = vm.heap.alloc(0x18).expect("guest heap");
            for (fb, px) in [(src_fb, src_px), (dst_fb, dst_px)] {
                vm.mem.w32(fb, w as u32);
                vm.mem.w32(fb + 4, h as u32);
                // `DM_GET_FRAME_BUFFER_INFO` divides this by the width, so it
                // has to be a real stride or the guest divides by zero.
                vm.mem.w32(fb + 8, (w * 2) as u32);
                vm.mem.w32(fb + 0xc, 0);
                vm.mem.w32(fb + 0x10, px);
            }
            vm.mem.w32(lcd_g + 8, dst_fb);
            vm.mem.w8(lcd_g + 0x8a, 1);

            // Something with structure in all three channels, so a mistake in
            // any one of the 565 lanes shows up.
            let mut s = 0x9e37_79b9u32;
            let input: Vec<u8> = (0..bytes)
                .map(|_| {
                    s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (s >> 19) as u8
                })
                .collect();

            // Every step the effect actually runs: the counter starts at 4 and
            // `Map_drawMiddleObject` adds 4 until it passes 0x27.
            for counter in [4u32, 8, 16, 24, 36, 40] {
                vm.mem.w32(lcd_g + 0x8c, counter);

                vm.mem.write_bytes(src_px, &input);
                vm.mem.write_bytes(dst_px, &vec![0u8; bytes as usize]);
                let keep = vm.patches.clone();
                vm.patches.retain(|&(_, p)| !matches!(p, Patch::RadialBlur));
                vm.call(blur, &[src_fb]).expect("the guest's own blur runs");
                vm.patches = keep;
                let want_src = vm.mem.slice(src_px, bytes).unwrap().to_vec();
                let want_dst = vm.mem.slice(dst_px, bytes).unwrap().to_vec();

                vm.mem.write_bytes(src_px, &input);
                vm.mem.write_bytes(dst_px, &vec![0u8; bytes as usize]);
                vm.cpu.reg_set(Mode::User, 0, src_fb);
                vm.cpu.reg_set(Mode::User, reg::LR, 0);
                assert!(vm.host_radial_blur().unwrap(), "{w}x{h} counter {counter} declined");
                let got_src = vm.mem.slice(src_px, bytes).unwrap().to_vec();
                let got_dst = vm.mem.slice(dst_px, bytes).unwrap().to_vec();

                assert_eq!(got_src, want_src, "{w}x{h} counter {counter}: source differs");
                assert_eq!(got_dst, want_dst, "{w}x{h} counter {counter}: offscreen differs");
                // A blur that returned the input unchanged would pass the two
                // assertions above only if the guest did nothing either, which
                // would make this test vacuous.
                assert_ne!(got_src, input, "{w}x{h} counter {counter}: nothing happened");
            }
        }
    }

    /// The retimed zoom's sub-step must not be a second implementation of the
    /// ray length. At a whole eighth it has to come out the same bytes as the
    /// counter on its own — which is what lets the shipped five frames, and the
    /// test above that pins them to the guest's own code, stand unchanged.
    #[test]
    fn an_eighth_step_on_a_whole_number_is_the_shipped_ray_length() {
        let mut vm = loaded_guest();
        // The shipped whole-screen effect at its full ray; see above.
        vm.zoom_r_pct = 0;
        vm.zoom_strength = 100;
        let lcd_g = vm.symbol("_lcdG").expect("in .symtab");
        let (w, h) = (37i32, 23i32);
        let bytes = (w * h) as u32 * 2;
        let src_px = vm.heap.alloc(bytes).expect("guest heap");
        let dst_px = vm.heap.alloc(bytes).expect("guest heap");
        let src_fb = vm.heap.alloc(0x18).expect("guest heap");
        let dst_fb = vm.heap.alloc(0x18).expect("guest heap");
        for (fb, px) in [(src_fb, src_px), (dst_fb, dst_px)] {
            vm.mem.w32(fb, w as u32);
            vm.mem.w32(fb + 4, h as u32);
            vm.mem.w32(fb + 8, (w * 2) as u32);
            vm.mem.w32(fb + 0xc, 0);
            vm.mem.w32(fb + 0x10, px);
        }
        vm.mem.w32(lcd_g + 8, dst_fb);
        vm.mem.w8(lcd_g + 0x8a, 1);

        let mut s = 0x1234_5678u32;
        let input: Vec<u8> = (0..bytes)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (s >> 19) as u8
            })
            .collect();

        let run = |vm: &mut Vm, counter: i32, fine: i32| -> Vec<u8> {
            vm.mem.w32(lcd_g + 0x8c, counter as u32);
            vm.zoom_fine = fine;
            vm.mem.write_bytes(src_px, &input);
            vm.cpu.reg_set(Mode::User, 0, src_fb);
            vm.cpu.reg_set(Mode::User, reg::LR, 0);
            assert!(vm.host_radial_blur().unwrap(), "counter {counter} fine {fine} declined");
            vm.mem.slice(src_px, bytes).unwrap().to_vec()
        };
        for counter in [ZOOM_FIRST, 24, 30, ZOOM_END - 1] {
            let shipped = run(&mut vm, counter, 0);
            let eighths = run(&mut vm, counter, counter * ZOOM_SUB);
            assert_eq!(eighths, shipped, "counter {counter} moved when it should not have");
            // And an eighth that is *not* whole has to be able to differ, or
            // the sub-step is doing nothing and the smoothing is imaginary.
            let between = run(&mut vm, counter, counter * ZOOM_SUB + ZOOM_SUB / 2);
            assert_ne!(between, shipped, "counter {counter}: half a step changed nothing");
        }
    }

    /// The timer this retimes, as the shipped code writes it: five words around
    /// the step, the value a fresh effect is armed with, the compare that ends
    /// it, and that the `bl` beside them still reaches the blur whose ray
    /// length that counter scales. Every constant the retime spans is decoded
    /// from those words rather than trusted.
    #[test]
    fn the_boss_kill_zoom_is_still_the_counter_this_retimes() {
        let mut vm = loaded_guest();
        let base = vm.symbol("Map_drawMiddleObject").expect("in .symtab");
        let blur = vm.symbol("DM_RadialBlurLcd").expect("in .symtab");
        let site = boss_zoom_site(base, blur, &mut vm.mem).expect("the shipped words");
        assert_eq!(site, base + ZOOM_STEP);

        // `mov r2, #0x14` and `cmp r3, #0x27`: the range the retime spreads out
        // is these two, not a pair of numbers written down beside them.
        assert_eq!(ZOOM_ARM_WORD & 0xfff, ZOOM_FIRST as u32, "the first counter drifted");
        assert_eq!(ZOOM_CMP_WORD & 0xfff, (ZOOM_END - 1) as u32, "the end compare drifted");
        // `add r3, r3, #4` over 0x14..=0x27 is the five frames this replaces.
        assert_eq!(ZOOM_STEP_WORD & 0xfff, 4);
        assert_eq!((ZOOM_END - ZOOM_FIRST + 3) / 4, 5, "the shipped effect is five frames");
        // The load and the store are the same field, and it is the one the blur
        // reads (`DM_RadialBlurLcd565+0x168`).
        assert_eq!(ZOOM_LOAD_WORD & 0xfff, 0x8c);
        assert_eq!(ZOOM_STORE_WORD & 0xfff, 0x8c);

        // A guest that drifted must install nothing rather than step something
        // else: flip the one instruction the hook stands in for.
        vm.mem.w32(site, 0xe283_3008);
        assert!(boss_zoom_site(base, blur, &mut vm.mem).is_none());
    }

    /// The retimed counter walks the shipped range, only ever forwards, and
    /// stops — the last of those being the one that matters, because
    /// `hero[0x899]` locks movement and skills until it does.
    #[test]
    fn the_retimed_zoom_walks_the_shipped_range_and_stops() {
        let mut vm = loaded_guest();
        let step = 16i64;
        for ms in [700i32, 2200, 5000] {
            vm.zoom_ms = ms;
            vm.zoom_last = 0;
            vm.zoom_fine = 0;
            // The straight ramp over the shipped range — the curve and the low
            // start are timing, and get their own test.
            vm.zoom_from = ZOOM_FIRST;
            vm.zoom_curve = 1;
            // What `+0x210` arms a fresh effect with.
            let mut counter = ZOOM_FIRST;
            let mut frames = 0;
            let mut seen = Vec::new();
            while counter < ZOOM_END {
                vm.cpu.reg_set(Mode::User, 3, counter as u32);
                vm.cpu.reg_set(Mode::User, reg::PC, 0x1000);
                assert!(vm.host_boss_zoom().unwrap());
                assert_eq!(
                    vm.cpu.reg_get(Mode::User, reg::PC),
                    0x1004,
                    "only the `add` is skipped"
                );
                let next = vm.cpu.reg_get(Mode::User, 3) as i32;
                assert!(next >= counter, "{ms} ms: the counter went backwards");
                assert!(next <= ZOOM_END, "{ms} ms: the counter overshot the end test");
                assert_eq!(vm.zoom_fine / ZOOM_SUB, next, "the fine value left for the blur");
                seen.push(next);
                counter = next;
                frames += 1;
                assert!(frames < 10_000, "{ms} ms: never ended");
                vm.clock_ms += step;
            }
            // It ended because time ran out, not because it ran out of frames:
            // the wall clock it spanned is the duration asked for, to within
            // the one frame that carries it past the end.
            let spanned = (frames as i64 - 1) * step;
            assert!(
                (spanned - ms as i64).abs() <= step,
                "{ms} ms: {frames} frames of {step} ms spanned {spanned} ms"
            );
            assert_eq!(seen[0], ZOOM_FIRST, "the first frame is the shipped one");
            assert!(seen.windows(2).all(|p| p[1] - p[0] <= 1), "{ms} ms: the counter jumped");
        }
    }

    /// The world gets the shipped number of ticks, no more and no fewer.
    ///
    /// Stretching the zoom alone leaves the scene running at full speed under
    /// it. The shipped effect ran the world **five** times across its whole life
    /// because each of its five frames was a stall, and this reproduces that —
    /// so what is pinned here is the count, that the ticks land one per
    /// `ZOOM_SHIPPED_STEP` of counter, and that the hook is inert either side.
    #[test]
    fn the_world_takes_the_same_five_steps_it_always_did() {
        let mut vm = loaded_guest();
        vm.zoom_ms = 2200;
        vm.zoom_last = 0;
        vm.zoom_fine = 0;
        // Straight ramp over the shipped range and the shipped tick count, so
        // the ticks are expected at the counters `add r3, r3, #4` produced.
        // Under a curve they stay evenly spread in *time*, which the next test
        // pins; `CS_BOSS_ZOOM_TICKS` moves the count itself.
        vm.zoom_from = ZOOM_FIRST;
        vm.zoom_curve = 1;
        vm.zoom_ticks = ZOOM_WORLD_TICKS;

        // The world tick, as `MainFrame_Process` reaches it: return to `lr` if
        // the host held it shut, run the guest's own body if it did not.
        let world = |vm: &mut Vm| -> bool {
            vm.cpu.reg_set(Mode::User, reg::LR, 0x2000);
            vm.cpu.reg_set(Mode::User, reg::PC, 0x1000);
            let held = vm.host_zoom_world().unwrap();
            if held {
                assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), 0x2000, "held = returned");
            }
            held
        };
        assert!(!world(&mut vm), "outside the effect the world is the guest's");

        let step = 16i64;
        let mut counter = ZOOM_FIRST;
        let (mut ticks, mut held, mut frames, mut at) = (0, 0, 0, Vec::new());
        while counter < ZOOM_END {
            // Process runs before Render — that is the order this rides on.
            let running = vm.zoom_running;
            if world(&mut vm) {
                held += 1;
            } else if running {
                ticks += 1;
                at.push(vm.clock_ms - vm.zoom_t0);
            }
            vm.cpu.reg_set(Mode::User, 3, counter as u32);
            vm.cpu.reg_set(Mode::User, reg::PC, 0x1000);
            assert!(vm.host_boss_zoom().unwrap());
            counter = vm.cpu.reg_get(Mode::User, 3) as i32;
            vm.clock_ms += step;
            frames += 1;
        }

        assert_eq!(ticks, 5, "the shipped effect ran the world five times");
        // Evenly across the duration — which for this straight ramp is also the
        // counters `add r3, r3, #4` produced (20, 24, 28, 32, 36). The due
        // bucket is computed on the render before the tick, so each lands
        // within a frame of its mark.
        for (k, &t) in at.iter().enumerate() {
            let want = 2200 * k as i64 / 5;
            assert!((t - want).abs() <= 2 * step, "tick {k} at {t} ms, expected about {want}");
        }
        assert!(held > frames - 10, "{held} of {frames} frames held; the world barely slowed");
        // Released with the effect, on the same frame the guest's `ble` fails.
        assert!(!vm.zoom_running);
        assert!(!world(&mut vm), "the world is the guest's again");
    }

    /// The bubble reaches the boss and stops before the rest of the screen.
    ///
    /// This is the one part of the effect that is **not** the shipped image, so
    /// what it has to earn is: everything past the radius is left byte for byte
    /// alone (no one-sample resample, which would shimmer), the centre really
    /// moves to where the boss was, and `CS_BOSS_ZOOM_R=0` puts the shipped
    /// whole-screen effect back.
    #[test]
    fn the_bubble_stops_before_it_reaches_the_rest_of_the_screen() {
        let mut vm = loaded_guest();
        // Geometry, not intensity: the ray stays full length so "inside moved"
        // does not quietly depend on how hard `CS_BOSS_ZOOM_STRENGTH` is set.
        vm.zoom_strength = 100;
        let lcd_g = vm.symbol("_lcdG").expect("in .symtab");
        let (w, h) = (120i32, 80i32);
        let bytes = (w * h) as u32 * 2;
        let src_px = vm.heap.alloc(bytes).expect("guest heap");
        let dst_px = vm.heap.alloc(bytes).expect("guest heap");
        let src_fb = vm.heap.alloc(0x18).expect("guest heap");
        let dst_fb = vm.heap.alloc(0x18).expect("guest heap");
        for (fb, px) in [(src_fb, src_px), (dst_fb, dst_px)] {
            vm.mem.w32(fb, w as u32);
            vm.mem.w32(fb + 4, h as u32);
            vm.mem.w32(fb + 8, (w * 2) as u32);
            vm.mem.w32(fb + 0xc, 0);
            vm.mem.w32(fb + 0x10, px);
        }
        vm.mem.w32(lcd_g + 8, dst_fb);
        vm.mem.w8(lcd_g + 0x8a, 1);
        vm.mem.w32(lcd_g + 0x8c, 0x24);

        let mut s = 0xdead_beefu32;
        let input: Vec<u8> = (0..bytes)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (s >> 19) as u8
            })
            .collect();
        let run = |vm: &mut Vm| -> Vec<u8> {
            vm.mem.write_bytes(src_px, &input);
            vm.cpu.reg_set(Mode::User, 0, src_fb);
            vm.cpu.reg_set(Mode::User, reg::LR, 0);
            assert!(vm.host_radial_blur().unwrap(), "declined");
            vm.mem.slice(src_px, bytes).unwrap().to_vec()
        };

        // A boss a quarter of the way across, and a bubble of a quarter of the
        // height — small enough that most of this surface is outside it.
        let (bx, by) = (30i32, 40i32);
        vm.zoom_r_pct = 25;
        vm.zoom_centre = Some((bx, by));
        let bubble = run(&mut vm);
        let fall = h * 25 / 100;

        let (mut touched, mut untouched) = (0, 0);
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) as usize;
                let d = (((x - bx).pow(2) + (y - by).pow(2)) as f64).sqrt();
                let same = bubble[i * 2..i * 2 + 2] == input[i * 2..i * 2 + 2];
                if d >= fall as f64 + 1.5 {
                    assert!(same, "({x},{y}) is {d:.1} from the centre and was rewritten");
                    untouched += 1;
                } else if d < fall as f64 * 0.7 && !same {
                    touched += 1;
                }
            }
        }
        assert!(untouched > 5000, "the bubble did not stop: {untouched} pixels left alone");
        assert!(touched > 200, "the bubble did nothing: {touched} pixels changed");

        // And it is centred where the boss was: the sharp point sits on it, so
        // the pixels around the far corner move more than the ones around it.
        let moved = |cx: i32, cy: i32| -> usize {
            (-3..=3)
                .flat_map(|dy| (-3..=3).map(move |dx| (dx, dy)))
                .filter(|&(dx, dy)| {
                    let i = (((cy + dy) * w) + cx + dx) as usize;
                    bubble[i * 2..i * 2 + 2] != input[i * 2..i * 2 + 2]
                })
                .count()
        };
        assert!(moved(bx, by) < moved(bx + fall / 2, by), "the centre should be the still point");

        // Zero reach and no centre is the shipped effect: the far corner is
        // blurred again.
        vm.zoom_r_pct = 0;
        vm.zoom_centre = None;
        let shipped = run(&mut vm);
        let corner = ((h - 1) * w + w - 1) as usize;
        assert_eq!(&bubble[corner * 2..corner * 2 + 2], &input[corner * 2..corner * 2 + 2]);
        assert_ne!(&shipped[corner * 2..corner * 2 + 2], &input[corner * 2..corner * 2 + 2]);
    }

    /// The blur is applied exactly as many times as it was asked for.
    ///
    /// The count is the whole of why the two-second version washed the screen
    /// out: the blur compounds, so applications are intensity. Pinned here
    /// because it is invisible from the outside — the effect looks *plausible*
    /// at any count, right up to the point where it is white.
    #[test]
    fn the_blur_is_applied_as_many_times_as_asked_and_no_more() {
        let mut vm = loaded_guest();
        for steps in [5i64, 10, 20] {
            vm.zoom_ms = 2200;
            vm.zoom_steps = steps;
            vm.zoom_ticks = ZOOM_WORLD_TICKS;
            vm.zoom_from = 6;
            vm.zoom_curve = 2;
            vm.zoom_last = 0;
            vm.zoom_fine = 0;
            vm.zoom_running = false;

            // The first application happens before this hook ever runs: it is
            // four instructions earlier in the same frame. Count it here.
            let (mut applied, mut ticks) = (1, 0);
            let mut counter = vm.zoom_from;
            let mut frames = 0;
            while counter < ZOOM_END {
                if vm.zoom_running {
                    if vm.zoom_blur_due != vm.zoom_blur_done {
                        applied += 1;
                        vm.zoom_blur_done = vm.zoom_blur_due;
                    }
                    if vm.zoom_world_due != vm.zoom_world_bucket {
                        ticks += 1;
                        vm.zoom_world_bucket = vm.zoom_world_due;
                    }
                }
                vm.cpu.reg_set(Mode::User, 3, counter as u32);
                vm.cpu.reg_set(Mode::User, reg::PC, 0x1000);
                assert!(vm.host_boss_zoom().unwrap());
                counter = vm.cpu.reg_get(Mode::User, 3) as i32;
                vm.clock_ms += 16;
                frames += 1;
                assert!(frames < 10_000);
            }
            assert_eq!(applied, steps as i32, "{steps} steps asked for");
            // And the world keeps its own count whatever the blur does.
            assert_eq!(ticks, ZOOM_WORLD_TICKS as i32, "{steps} steps: world ticks moved");
            assert!(frames > applied, "every frame applied the blur; nothing was held");
        }
    }

    /// The curve and the low start are *timing*, not a new effect.
    ///
    /// What has to hold whatever they are set to: the sweep starts on
    /// `CS_BOSS_ZOOM_FROM`, only ever goes forwards, ends on the value the
    /// guest's `cmp #0x27` refuses, and the world still gets five ticks — now
    /// spread over the time rather than the counter, because with a curve those
    /// are no longer the same thing. And the curve has to actually bend: the
    /// point of it is that the hard smear is on screen for less of the run.
    #[test]
    fn the_curve_only_moves_when_each_counter_is_on_screen() {
        let mut vm = loaded_guest();
        let (ms, step) = (2200i32, 16i64);
        let mut half_of = Vec::new();
        for (from, curve) in [(ZOOM_FIRST, 1), (ZOOM_FIRST, 2), (6, 1), (6, 2), (1, 4)] {
            vm.zoom_ms = ms;
            vm.zoom_from = from;
            vm.zoom_curve = curve;
            vm.zoom_ticks = ZOOM_WORLD_TICKS;
            vm.zoom_last = 0;
            vm.zoom_fine = 0;
            vm.zoom_world_bucket = -1;

            let mut counter = from;
            let (mut seen, mut ticks, mut frames) = (Vec::new(), 0, 0);
            while counter < ZOOM_END {
                let running = vm.zoom_running;
                vm.cpu.reg_set(Mode::User, reg::LR, 0x2000);
                vm.cpu.reg_set(Mode::User, reg::PC, 0x1000);
                if !vm.host_zoom_world().unwrap() && running {
                    ticks += 1;
                }
                vm.cpu.reg_set(Mode::User, 3, counter as u32);
                vm.cpu.reg_set(Mode::User, reg::PC, 0x1000);
                assert!(vm.host_boss_zoom().unwrap());
                let next = vm.cpu.reg_get(Mode::User, 3) as i32;
                assert!(next >= counter, "from {from} curve {curve}: went backwards");
                assert!(next <= ZOOM_END, "from {from} curve {curve}: overshot");
                seen.push(next);
                counter = next;
                frames += 1;
                assert!(frames < 10_000, "from {from} curve {curve}: never ended");
                vm.clock_ms += step;
            }
            assert_eq!(seen[0], from, "the sweep starts where it was told to");
            assert_eq!(*seen.last().unwrap(), ZOOM_END, "and ends on the guest's end test");
            assert_eq!(ticks, ZOOM_WORLD_TICKS as i32, "from {from} curve {curve}: world ticks");
            // Where the counter had got to halfway through the duration.
            half_of.push(seen[seen.len() / 2]);
        }
        // A curve has to bend the same range, or it is doing nothing: halfway
        // through, `t^2` must be well short of where the straight ramp is.
        assert!(half_of[1] < half_of[0] - 3, "curve 2 did not slow the first half: {half_of:?}");
        assert!(half_of[3] < half_of[2] - 3, "curve 2 did not slow the first half: {half_of:?}");
        // And a low start really is a gentler opening: the shipped 0x14 already
        // smears the corners half as far as the effect ever does.
        assert!(half_of[2] < half_of[0], "a low start should sit lower: {half_of:?}");
    }

    /// The tables the blur reads are the shape the port assumes, and are *not*
    /// the textbook functions — `nSin[90]` is 1023, so a computed table would
    /// differ. This is what makes reading them out of guest memory necessary
    /// rather than tidy.
    #[test]
    fn the_trig_tables_are_the_shape_the_radial_blur_assumes() {
        let mut vm = loaded_guest();
        for (name, len) in [("nCos", 360u32), ("nSin", 360)] {
            let size = *vm.elf.sizes.get(name).expect("sized in .symtab");
            assert_eq!(size, len * 4, "{name} changed length");
        }
        let n_sin = vm.symbol("nSin").unwrap();
        let n_cos = vm.symbol("nCos").unwrap();
        assert_eq!(vm.mem.r32(n_cos) as i32, 1024, "Q10 scale");
        assert_eq!(vm.mem.r32(n_sin) as i32, 0);
        // The quirk itself: exactly 1.0 would be 1024.
        assert_eq!(vm.mem.r32(n_sin + 90 * 4) as i32, 1023, "nSin[90] is not exact");
        // `nTan` is searched with a count of 180 but is 179 entries long.
        assert_eq!(*vm.elf.sizes.get("nTan").expect("nTan"), 179 * 4, "nTan changed length");
    }

    #[test]
    fn every_item_level_site_still_matches_its_instruction() {
        let mut vm = loaded_guest();
        let sites = item_level_sites(&vm.elf.symbols, &mut vm.mem);
        assert_eq!(sites.len(), ITEM_LEVEL_SITES.len(), "a hook site drifted");
        // The tooltip's load is not among them — it keeps printing the real
        // requirement so `host_item_info_level` can rule it through.
        let tooltip = vm.symbol("drawItemInfo").unwrap() + 0xc78;
        assert!(sites.iter().all(|&(at, _)| at != tooltip));
        // And it is still the instruction that patch expects to find.
        assert_eq!(vm.mem.r32(tooltip), 0xe1d4_20d7, "drawItemInfo's ldrsb drifted");
    }

    /// The rename hook is a mid-function hook on nine verified words, so the
    /// only thing left to pin is that they are the *right* nine: the block must
    /// resolve, and it must be the last instruction of the commit — the name
    /// written and `nameTitleIdx` already back to -1 — so that saving there and
    /// declining cannot leave anything half done.
    #[test]
    fn the_pet_rename_hook_sits_on_the_end_of_the_name_commit() {
        let mut vm = loaded_guest();
        let (base, ime_end, sprintk, get_hero) = (
            vm.symbol("MainFrame_KeyPressed").unwrap(),
            vm.symbol("MX_imeEnd").unwrap(),
            vm.symbol("MC_knlSprintk").unwrap(),
            vm.symbol("MP_getHero").unwrap(),
        );
        assert_eq!(
            pet_rename_site(base, ime_end, sprintk, get_hero, &mut vm.mem),
            Some(base + PET_COMMIT_END),
        );
        // `mvn r1, #0` / `strh r1, [r2]` is nameTitleIdx = -1, and both are
        // behind us at the hook — as is the yes/no cursor's `strb r7, [r3]`.
        assert_eq!(vm.mem.r32(base + PET_COMMIT_END - 0xc), 0xe3e0_1000, "mvn r1, #0 drifted");
        assert_eq!(vm.mem.r32(base + PET_COMMIT_END - 8), 0xe1c2_10b0, "strh drifted");
        assert_eq!(vm.mem.r32(base + PET_COMMIT_END - 4), 0xe5c3_7000, "strb drifted");
        // Nothing else in the function writes the pet's name: the `+0x620` half
        // of `pet + 0x62d` appears once.
        let size = *vm.elf.sizes.get("MainFrame_KeyPressed").expect("sized");
        let adds = (0..size / 4)
            .map(|i| base + i * 4)
            .filter(|&a| vm.mem.r32(a) & 0xfff0_0fff == PET_COMMIT_WORDS[2].1 & 0xfff0_0fff)
            .count();
        assert_eq!(adds, 1, "a second pet-name write appeared in MainFrame_KeyPressed");
    }

    /// The two MIDAS leaves, against the shipped ARM code that is their
    /// specification.
    ///
    /// A probe replay cannot reach the naming keyboard, and these two are called
    /// from 24 places for rounded rectangles that a dump comparison would cover
    /// only incidentally. So the guest's own body is the oracle: take the patch
    /// out of the table, run `_midas_drawLineH` / `_midas_drawPixel`, then run the
    /// host on the same buffer and compare every byte.
    ///
    /// The cases are chosen for the parts that are easy to get wrong: all three
    /// `*_midas_drawDIR` rotations, a zero-length line (which still draws **one**
    /// pixel, at `x`), runs that start or end outside the surface, negative
    /// coordinates, and 32bpp as well as 16.
    #[test]
    fn the_guest_agrees_with_our_midas_leaves() {
        let mut vm = loaded_guest();
        let dir_p = vm.symbol("_midas_drawDIR").expect("in .symtab");
        let entries: Vec<(u32, MidasOp)> = [
            ("_midas_drawPixel", MidasOp::Pixel),
            ("_midas_drawLineH", MidasOp::LineH),
            ("_midas_drawLineV", MidasOp::LineV),
            ("_midas_fillRect", MidasOp::FillRect),
        ]
        .iter()
        .map(|&(n, op)| (vm.symbol(n).unwrap_or_else(|| panic!("{n} in .symtab")), op))
        .collect();
        let (w, h) = (23i32, 17i32);

        let mut checked = 0;
        let mut drew = 0;
        for &bpp in &[16u32, 32] {
            let bytes = (w * h) as u32 * bpp / 8;
            let px = vm.heap.alloc(bytes).expect("guest heap");
            let g = vm.heap.alloc(0x18).expect("guest heap");
            vm.mem.w32(g, w as u32);
            vm.mem.w32(g + 4, h as u32);
            vm.mem.w32(g + 8, (w * bpp as i32 / 8) as u32);
            vm.mem.w32(g + 0xc, bpp);
            vm.mem.w32(g + 0x10, px);

            for &dir in &[0i32, 1, -1] {
                vm.mem.w32(dir_p, dir as u32);
                for &(x, y, len) in &[
                    (0i32, 0i32, 0i32), // one pixel at the origin
                    (3, 5, 0),          // zero length still draws one
                    (2, 4, 9),          // a plain run
                    (-4, 6, 8),         // starts before the surface
                    (w - 3, 7, 9),      // runs off the far edge
                    (5, -1, 4),         // one row/column outside
                    (5, h, 4),          // and outside the other way
                    (-20, 3, 4),        // entirely outside
                    (0, h - 1, w - 1),  // the whole last row
                ] {
                    for &(f, op) in &entries {
                        let args: &[u32] = match op {
                            MidasOp::Pixel => &[g, x as u32, y as u32, 0xabcd_1234],
                            MidasOp::LineH | MidasOp::LineV => {
                                &[g, x as u32, y as u32, len as u32, 0xabcd_1234]
                            }
                            // A rectangle, so the second extent is exercised too.
                            MidasOp::FillRect => {
                                &[g, x as u32, y as u32, len as u32, 3, 0xabcd_1234]
                            }
                        };
                        vm.mem.write_bytes(px, &vec![0u8; bytes as usize]);
                        let keep = vm.patches.clone();
                        vm.patches.retain(|&(_, p)| !matches!(p, Patch::MidasLine(_)));
                        vm.call(f, args).expect("the guest's own leaf runs");
                        vm.patches = keep;
                        let want = vm.mem.slice(px, bytes).unwrap().to_vec();

                        vm.mem.write_bytes(px, &vec![0u8; bytes as usize]);
                        for (i, a) in args.iter().take(4).enumerate() {
                            vm.cpu.reg_set(Mode::User, i as u8, *a);
                        }
                        // Lay the stack arguments out exactly as `Vm::call` did
                        // for the guest above — from `[sp]` up. Writing them
                        // anywhere else would make this test agree with whatever
                        // the hook happens to read, which is how a wrong offset
                        // survived once already: the guest reads its fifth at
                        // `[sp+0x18]`, but only after a `push` this hook precedes.
                        if args.len() > 4 {
                            let words = args.len() - 4;
                            let sp = (vm.cpu.reg_get(Mode::User, reg::SP) & !7)
                                - ((words * 4 + 7) & !7) as u32;
                            vm.cpu.reg_set(Mode::User, reg::SP, sp);
                            for (i, a) in args[4..].iter().enumerate() {
                                vm.mem.w32(sp + (i * 4) as u32, *a);
                            }
                        }
                        vm.cpu.reg_set(Mode::User, reg::LR, 0);
                        let case = format!("{bpp}bpp dir {dir} ({x},{y}) len {len} {op:?}");
                        assert!(vm.host_midas_line(op).unwrap(), "{case} declined");
                        let got = vm.mem.slice(px, bytes).unwrap().to_vec();
                        assert_eq!(got, want, "{case} differs");
                        checked += 1;
                        drew += usize::from(want.iter().any(|&b| b != 0));
                    }
                }
            }
        }
        // Without this the test would pass just as well if every case drew
        // nothing at all, host and guest alike.
        assert_eq!(checked, 2 * 3 * 9 * 4, "a case was skipped");
        assert!(drew > checked / 2, "only {drew} of {checked} cases drew anything");
    }

    /// The shipped file declares twelve rows and a page holds twenty, so every
    /// row we add has to fit in the eight that are left — otherwise
    /// `host_cash_potion_list` refuses to inject at all and the items silently
    /// stop existing.
    #[test]
    fn the_shelf_still_fits_one_shop_page() {
        /// Rows `NPC/MERCHANT_2` declares.
        const SHIPPED: u32 = 12;
        assert!(
            SHIPPED + crate::potions::SHELF.len() as u32 <= MERCH_PAGE,
            "the shelf no longer fits a page; host_cash_potion_list would inject nothing"
        );
    }

    /// Why a rename works outside a cutscene: the keyboard is not part of any
    /// game frame. One byte of `_midas_internalImeInstance` switches the paint
    /// and the event route, and both switches sit above the frame loop — this
    /// pins the two branches that make that true, plus the fact that the OK key
    /// is what releases the overlay (so the player cannot leave the naming
    /// half finished, which is what lets the save hook be unconditional).
    #[test]
    fn the_naming_overlay_is_switched_above_the_frame_loop() {
        let mut vm = loaded_guest();
        let [flush, paint, auth, ime_event, game_event, touch_jni, clet, key, release, ime_flush, ime_end, timer] =
            [
                "CUSTOM_grpFlushLcd",
                "imeLocal_QWERTY_Paint",
                "clsidAuthHandle",
                "imeLocal_onHandleEvent",
                "onHandleEvent",
                "Java_co_kr_eamobile_CletEntry_nativeTouchHandleEvent",
                "handleCletEvent",
                "imeLocal_onHandleEventKey",
                "imeLocal_Release",
                "imeLocal_flush",
                "MX_imeEnd",
                "timerCB",
            ]
            .map(|n| vm.symbol(n).unwrap_or_else(|| panic!("{n} missing")));
        // The flush paints the keyboard, and the flush is `timerCB`'s, not a
        // game frame's.
        assert_eq!(branch_target(flush + 0x64, &mut vm.mem, true), Some(paint));
        assert_eq!(branch_target(timer + 0x60, &mut vm.mem, true), Some(flush));
        // The platform event handler routes to the IME instead of the game.
        assert_eq!(branch_target(auth + 0x124, &mut vm.mem, false), Some(ime_event));
        assert_eq!(branch_target(auth + 0x134, &mut vm.mem, false), Some(game_event));
        // And our touches reach that handler.
        assert_eq!(branch_target(touch_jni + 0x3c, &mut vm.mem, true), Some(clet));
        // The OK key (-5) flushes the composition and then releases, before the
        // close callback ever runs.
        assert_eq!(branch_target(key + 0x204, &mut vm.mem, true), Some(ime_flush));
        assert_eq!(branch_target(key + 0x20c, &mut vm.mem, true), Some(release));
        // `imeLocal_Release` is the only thing that clears the gate byte, and
        // `MX_imeEnd` is a four-byte tail call to it.
        assert_eq!(branch_target(ime_end, &mut vm.mem, false), Some(release));
    }

    /// `open_pet_rename` pre-fills the input line by calling `MX_imeSetProperty`
    /// with property 2 — the same call `InputInit`'s pet branch makes, with a
    /// different pointer. This pins that: mode 1's last act is property 2, and
    /// the limit it sets first is 14 bytes.
    #[test]
    fn the_naming_mode_takes_its_initial_string_through_property_two() {
        let mut vm = loaded_guest();
        let (init, set_prop) =
            (vm.symbol("InputInit").unwrap(), vm.symbol("MX_imeSetProperty").unwrap());
        assert_eq!(vm.mem.r32(init + 0xa8), 0xe3a0_100e, "the 14-byte limit drifted");
        assert_eq!(
            vm.mem.r32(init + 0xcc),
            0xe3a0_0000 | IME_PROP_TEXT,
            "mode 1 no longer ends on property 2"
        );
        assert_eq!(branch_target(init + 0xd4, &mut vm.mem, false), Some(set_prop));
    }

    /// The keyboard is narrowed at **one** of the four IME calls to
    /// `__knlGR_getScreenFrameBuffer`, and this pins which — and that the other
    /// three are real call sites that were left alone on purpose rather than
    /// missed.
    ///
    /// Narrowing more than the layout would be wrong in two different ways at
    /// once: the paint would clip its own rounded rectangles at the narrowed
    /// width, and a narrowed hit test would disagree with a table that is in
    /// screen coordinates.
    #[test]
    fn only_the_keyboard_layout_is_told_the_screen_is_narrower() {
        let mut vm = loaded_guest();
        let get_fb = vm.symbol("__knlGR_getScreenFrameBuffer").expect("in .symtab");
        let bases: Vec<Option<u32>> = IME_SURFACE_NAMES.iter().map(|n| vm.symbol(n)).collect();
        // The layout's own site, resolved exactly the way the installer does.
        let sites = ime_surface_sites(&bases, get_fb, &mut vm.mem);
        assert_eq!(sites.len(), 1, "the layout's call to the framebuffer drifted");
        let init = vm.symbol("imeLocal_button_init").unwrap();
        assert_eq!(sites[0], init + IME_SURFACE_SITES[0].1);
        // And the three that are deliberately not hooked are still there, so a
        // future reader can tell "left alone" from "no longer exists".
        for (name, bl) in [
            ("imeLocal_QWERTY_Paint", 0x8u32),
            ("imeLocal_onHandleEventTouch", 0x40),
            ("imeLocal_getQERTYindex", 0x38),
        ] {
            let at = vm.symbol(name).unwrap_or_else(|| panic!("{name} missing")) + bl;
            assert_eq!(branch_target(at, &mut vm.mem, true), Some(get_fb), "{name} drifted");
        }
    }

    /// The hit test reads the key table as **plain top-left screen pixels**.
    ///
    /// This is the whole licence for centring the narrowed keyboard by adding a
    /// constant to every `x`: the touch arrives in screen coordinates and is
    /// compared against the table untransformed, so moving the table moves the
    /// buttons and their hit boxes by the same amount. The `w - x` / `h - y`
    /// forms in the same function are the ±90° layouts', reached only when
    /// `ime[0x296]` is ±0x5a, and this keyboard never sets it — so they are
    /// pinned here too, as the thing that must stay unreachable.
    ///
    /// It is worth being exact about, because reading those `rsb`s as the general
    /// case is what put a coordinate-shifting patch on both hit tests in the
    /// first place, and that patch would now be wrong.
    #[test]
    fn the_key_table_is_read_as_plain_screen_pixels() {
        let mut vm = loaded_guest();
        let touch = vm.symbol("imeLocal_onHandleEventTouch").expect("in .symtab");
        let idx = vm.symbol("imeLocal_getQERTYindex").expect("in .symtab");
        // The four compares that decide a hit, all against the raw touch.
        for (off, word) in [
            (0xbcu32, 0xe150_0009u32),
            (0xd4, 0xe155_0009),
            (0xec, 0xe150_0006),
            (0x10c, 0xe155_0007),
        ] {
            assert_eq!(vm.mem.r32(touch + off), word, "hit compare at +{off:#x} drifted");
        }
        // The far-edge values exist, but only the rotation branches select them.
        assert_eq!(vm.mem.r32(touch + 0x58), 0xe064_0000, "w - x is not where it was");
        assert_eq!(vm.mem.r32(touch + 0x74), 0xe060_2002, "h - y is not where it was");
        assert_eq!(vm.mem.r32(touch + 0x5c), 0xe198_c0f3, "the rotation is not ime[0x296]");
        assert_eq!(vm.mem.r32(touch + 0x98), 0xe35c_005a, "cmp rot, #90 drifted");
        assert_eq!(vm.mem.r32(touch + 0x120), 0xe37c_005a, "cmn rot, #90 drifted");
        assert_eq!(vm.mem.r32(idx + 0x90), 0xe358_005a, "cmp rot, #90 drifted");
        assert_eq!(vm.mem.r32(idx + 0x11c), 0xe378_005a, "cmn rot, #90 drifted");
        // Both walkers take the record count from the mode byte, and it is the
        // pair this module hands the translation and the debug dump.
        for (base, qwerty, numeric) in
            [(touch, 0x03a0_a01fu32, 0x13a0_a012u32), (idx, 0x03a0_901f, 0x13a0_9012)]
        {
            let off = if base == touch { 0x80 } else { 0x6c };
            assert_eq!(vm.mem.r32(base + off), qwerty | IME_KEYS_QWERTY);
            assert_eq!(vm.mem.r32(base + off + 4), numeric | IME_KEYS_NUMERIC);
        }
    }

    /// The record layout the translation writes through, taken from the far end:
    /// the paint's backspace-icon block loads `w`, `x`, `y`, `h` off one record
    /// with `[r4]`, `[r4,#-4]`, `[r4,#-2]`, `[r4,#2]` where `r4` is
    /// `ime + 0x2dc + i*0x14`. So `x` is at record +0 and `w` at +4 — which is
    /// what decides both what gets moved and what counts as an unused slot.
    #[test]
    fn the_key_record_is_x_y_w_h_at_the_stride_we_walk() {
        let mut vm = loaded_guest();
        let paint = vm.symbol("imeLocal_QWERTY_Paint").expect("in .symtab");
        for (off, word, what) in [
            (0x608u32, 0xe1d4_00f0u32, "w at +4"),
            (0x60c, 0xe154_10f4, "x at +0"),
            (0x610, 0xe154_20f2, "y at +2"),
            (0x614, 0xe1d4_30f2, "h at +6"),
        ] {
            assert_eq!(vm.mem.r32(paint + off), word, "{what} drifted");
        }
        // `add r4, r4, #0x14` is the walk, and the QWERTY loop runs from
        // `ime + 0x2dc` to `ime + 0x548` — 31 records of 0x14, the count the
        // hit test picks for mode 0.
        assert_eq!(vm.mem.r32(paint + 0x4f4), 0xe284_4000 | IME_KEY_STRIDE, "stride drifted");
        // `add sl, r4, #0x540` — the loop's end, with 0x540 as rot 13 of 0x15.
        assert_eq!(vm.mem.r32(paint + 0x414), 0xe284_ad15, "the row block moved");
        assert_eq!((0x548 - 0x2dc) / IME_KEY_STRIDE, IME_KEYS_QWERTY);
        assert_eq!(0x168 / IME_KEY_STRIDE, IME_KEYS_NUMERIC);
        // The record base this module uses is four bytes below the paint's.
        assert_eq!(IME_KEYS + 4, 0x2dc);
    }

    /// The OK button the naming popup borrows is the shop's, and it really is
    /// the bottom-right corner rect this module reconstructs. Pinned from the
    /// shop's own `Check_Rect` call, and paired with the popup's key path, since
    /// the rewrite is only correct if both ends still hold.
    #[test]
    fn the_ok_button_behind_the_popup_is_the_corner_rect_we_rebuild() {
        let mut vm = loaded_guest();
        let (base, tk, check) = (
            vm.symbol("MainFrame_KeyPressed").expect("in .symtab"),
            vm.symbol("NpcUI_tableKeyPressed").expect("in .symtab"),
            vm.symbol("Check_Rect").expect("in .symtab"),
        );
        assert_eq!(name_ok_site(Some(base), &mut vm.mem), Some(base), "the popup branch moved");
        // The shop builds `(w - 0x37, h - 0x3f, 0x37, 0x3f)` out of `_lcdG`.
        let (bw, bh) = OK_BUTTON;
        for (off, word) in [
            (0x638u32, 0xe3a0_c000 | bw as u32), // mov ip, #0x37   (width)
            (0x658, 0xe243_3000 | bh as u32),    // sub r3, r3, #0x3f
            (0x65c, 0xe3a0_c000 | bh as u32),    // mov ip, #0x3f   (height)
            (0x660, 0xe242_2000 | bw as u32),    // sub r2, r2, #0x37
        ] {
            assert_eq!(vm.mem.r32(tk + off), word, "the OK button rect at +{off:#x} drifted");
        }
        assert_eq!(branch_target(tk + 0x668, &mut vm.mem, true), Some(check));
        // And the popup's key table still sends -5 to the activate block and the
        // two arrows to the invisible-selection toggle we deliberately avoid.
        let key_tab = base + 0x27c;
        assert_eq!(branch_target(key_tab, &mut vm.mem, false), Some(base + 0x384), "-5 moved");
        for arrow in [key_tab + 4, key_tab + 8] {
            assert_eq!(branch_target(arrow, &mut vm.mem, false), Some(base + 0x36c));
        }
        assert_eq!(vm.mem.r32(base + 0x378), 0xe222_2001, "the selection toggle is not an eor");
    }

    /// `←` on an empty name is the shipped keyboard's cancel, and the frame
    /// below cannot tell a cancel from a commit. This pins both halves of that,
    /// since the fix is only justified by the pair.
    #[test]
    fn an_empty_backspace_is_a_cancel_the_frame_below_cannot_see() {
        let mut vm = loaded_guest();
        let base = vm.symbol("imeLocal_onHandleEventKey").expect("in .symtab");
        // The site the redirect stands on, checked the way the installer does.
        assert_eq!(
            ime_backspace_site(Some(base), &mut vm.mem),
            Some(base + IME_BS_TEST),
            "the backspace block moved"
        );
        // The `beq` really does go to the cancel, and the cancel really is what
        // marks itself with -7 and hands the tail `imeLocal_flush`.
        let cancel = base + 0x59c;
        assert_eq!(base + IME_BS_TEST + 8 + (0x83 << 2), cancel);
        assert_eq!(vm.mem.r32(cancel + 8), 0xe3e0_5006, "mvn r5, #6 (the -7 marker) moved");
        assert_eq!(branch_target(cancel + 0x14, &mut vm.mem, true), vm.symbol("imeLocal_flush"));
        // The exit it is redirected to is the one the guest's own two failed
        // backspaces take, and it returns unless the marker is -7. Both are
        // conditional (`bne` after the revert, `ble` on the length), so the
        // displacement is decoded here rather than through `branch_target`.
        for (from, cond) in [(0x3a0u32, 0x1au32), (0x3ac, 0xda)] {
            let at = base + from;
            let word = vm.mem.r32(at);
            assert_eq!(word >> 24, cond, "the failed-backspace branch at +{from:#x} changed");
            let target = at.wrapping_add(8).wrapping_add((((word << 8) as i32 >> 8) as u32) << 2);
            assert_eq!(
                target,
                base + IME_BS_NOTHING,
                "a failed-backspace exit no longer lands on the no-op tail"
            );
        }
        assert_eq!(vm.mem.r32(base + IME_BS_NOTHING + 0xc), 0xe375_0007, "cmn r5, #7 moved");
        // And the callback cannot tell the two apart: `cmp r0, #1 / bls` takes
        // both the commit (0) and the cancel (1).
        let close = vm.symbol("onCloseIME_MainFrame").expect("in .symtab");
        assert_eq!(vm.mem.r32(close + 8), 0xe350_0001, "cmp r0, #1 moved");
        assert_eq!(vm.mem.r32(close + 0x10), 0x9a00_0000, "bls moved");
    }

    #[test]
    fn every_net_item_site_still_matches_its_instruction() {
        let mut vm = loaded_guest();
        let sites = net_item_sites(&vm.elf.symbols, &mut vm.mem);
        assert_eq!(sites.len(), NET_ITEM_SITES.len(), "a hook site drifted");
        // Each site is followed by the `cmp rX, #0` the flag feeds; the gate
        // makes no sense against anything else.
        for (at, patch) in sites {
            let Patch::NetItemFlag { rd, .. } = patch else { panic!("wrong patch") };
            assert_eq!(vm.mem.r32(at + 4), 0xe350_0000 | u32::from(rd) << 16, "cmp drifted");
        }
        // And the first `bl MONSTER_loadCsv` in `MONSTER_dataLoad` still
        // returns where the drop hook's LR gate expects.
        let (load, csv) =
            (vm.symbol("MONSTER_dataLoad").unwrap(), vm.symbol("MONSTER_loadCsv").unwrap());
        let bl = vm.mem.r32(load + MON_LOADCSV_RET - 4);
        assert_eq!(bl >> 24, 0xeb, "not a bl");
        let target = (load + MON_LOADCSV_RET - 4)
            .wrapping_add(8)
            .wrapping_add((((bl << 8) as i32) >> 6) as u32);
        assert_eq!(target, csv, "the bl no longer calls MONSTER_loadCsv");
    }

    /// The equip-restore hook is pinned to one `bl` inside `Item_loadStorage`,
    /// and it is the *middle* one of the three. This pins that down: the other
    /// two — the opening clear at `+0x8c` and the pet's at `+0x408` — must not
    /// be the address we chose, or the patch would fire on the wrong call.
    #[test]
    fn the_equip_restore_hook_still_sits_on_the_stamp_match_call() {
        let mut vm = loaded_guest();
        let (loader, setter) =
            (vm.symbol("Item_loadStorage").unwrap(), vm.symbol("ITEM_setEquip").unwrap());
        assert_eq!(
            equip_restore_ret(loader, setter, &mut vm.mem),
            Some(loader + EQUIP_RESTORE_CALL + 4),
            "the bl at Item_loadStorage+{EQUIP_RESTORE_CALL:#x} no longer calls ITEM_setEquip"
        );
        // Three calls in the function, and we want the one that is neither the
        // clear nor the pet's.
        let calls: Vec<u32> = (0..0x474 / 4)
            .map(|i| loader + i * 4)
            .filter(|&a| {
                let w = vm.mem.r32(a);
                w >> 24 == 0xeb
                    && a.wrapping_add(8).wrapping_add((((w << 8) as i32) >> 6) as u32) == setter
            })
            .collect();
        assert_eq!(calls, vec![loader + 0xa0, loader + EQUIP_RESTORE_CALL, loader + 0x408]);
        // The clear passes NULL — `mov r2, r0` at `+0x98` with r0 zeroed two
        // instructions earlier — which is the arm the hook declines on anyway.
        assert_eq!(vm.mem.r32(loader + 0x98), 0xe1a0_2000, "the clear stopped being mov r2, r0");
    }

    /// The claim the skip watchdog rests on: a running skip is advanced by the
    /// *renderer* and by nothing else, so if the renderer is not called the
    /// cutscene can never resume.
    ///
    /// Two halves. `EVENT_Tick` still gates on the three bytes at the offsets
    /// the watchdog reads, and the only code anywhere in the image that writes
    /// the progress byte is inside `Event_Draw_Skip_Effect`.
    #[test]
    fn a_running_skip_is_advanced_only_by_the_thing_that_draws_it() {
        let mut vm = loaded_guest();
        let tick = vm.symbol("EVENT_Tick").unwrap();
        // `ldrb r3, [r4, #0x334]` / `ldrb r3, [r4, #0x337]` / `ldrb r2, [r4, #0x336]`
        assert_eq!(vm.mem.r32(tick + 0x28), 0xe5d4_3000 | EV_SKIPPING, "the skip test moved");
        assert_eq!(vm.mem.r32(tick + 0x34), 0xe5d4_3000 | EV_SKIP_TOTAL, "the total moved");
        assert_eq!(vm.mem.r32(tick + 0x38), 0xe5d4_2000 | EV_SKIP_PROGRESS, "progress moved");

        // Every `strb rD, [rN, #0x336]` in the loaded image, under any
        // condition code — the second of the two is `strbgt`, which is how the
        // renderer clamps.
        let draw = vm.symbol("Event_Draw_Skip_Effect").unwrap();
        let writers: Vec<u32> = (crate::mem::IMAGE_BASE..vm.elf.image_end - 4)
            .step_by(4)
            .filter(|&a| {
                let w = vm.mem.r32(a);
                w & 0x0ff0_0000 == 0x05c0_0000 && w & 0xfff == EV_SKIP_PROGRESS
            })
            .collect();
        // Four writers, and only two of them are an *advance*: the other two
        // are the arm, which zeroes progress — `Event_skipFunction` and the
        // copy of it inlined into `Event_Key`, the one the SKIP opening jumps
        // to. Nothing else in the image can move a skip along.
        let arm = vm.symbol("Event_skipFunction").unwrap();
        let key = vm.symbol("Event_Key").unwrap();
        assert_eq!(
            writers,
            vec![arm + 0x2c, draw + 0x3c, draw + 0x40, key + 0xf4],
            "someone other than Event_Draw_Skip_Effect now moves the skip along"
        );
        // `mov r2, #0` / `mov r1, #0` feed the two arms, so they only ever
        // write the constant the watchdog starts counting from.
        assert_eq!(vm.mem.r32(arm + 0x8), 0xe3a0_2000, "the arm stopped zeroing progress");
        assert_eq!(vm.mem.r32(key + 0xd4), 0xe3a0_1000, "the inlined arm stopped zeroing it");
    }

    /// The watchdog is inert while the renderer is doing its job, and steps in
    /// — one frame's worth at a time, clamped exactly as the renderer clamps —
    /// once it plainly is not.
    #[test]
    fn the_skip_watchdog_only_moves_a_skip_that_has_stopped_moving() {
        let mut vm = loaded_guest();
        let ev = vm.symbol("_event").unwrap();
        let set = |vm: &mut Vm, on: u8, progress: u8| {
            vm.mem.w8(ev + EV_SKIPPING, on);
            vm.mem.w8(ev + EV_SKIP_PROGRESS, progress);
            vm.mem.w8(ev + EV_SKIP_TOTAL, 5);
        };
        let run = |vm: &mut Vm, n: u32| {
            for _ in 0..n {
                assert!(!vm.host_skip_watchdog().unwrap(), "the watchdog must always decline");
            }
            vm.mem.r8(ev + EV_SKIP_PROGRESS)
        };

        // No skip running: nothing happens however long we wait.
        set(&mut vm, 0, 0);
        assert_eq!(run(&mut vm, SKIP_STALL_TICKS * 3), 0);

        // A skip the renderer is advancing: the stall counter never builds up.
        set(&mut vm, 1, 0);
        for i in 0..SKIP_STALL_TICKS * 3 {
            vm.mem.w8(ev + EV_SKIP_PROGRESS, (i % 5) as u8);
            assert!(!vm.host_skip_watchdog().unwrap());
            assert_eq!(
                vm.mem.r8(ev + EV_SKIP_PROGRESS),
                (i % 5) as u8,
                "not the watchdog's to move"
            );
        }

        // A skip nothing is advancing: one step per threshold, and it stops at
        // the total rather than running past it.
        set(&mut vm, 1, 0);
        vm.skip_stall = 0;
        vm.skip_last = 0;
        assert_eq!(run(&mut vm, SKIP_STALL_TICKS - 1), 0, "too eager");
        assert_eq!(run(&mut vm, 1), 1, "should have stepped");
        assert_eq!(run(&mut vm, SKIP_STALL_TICKS * 4), 5);
        assert_eq!(run(&mut vm, SKIP_STALL_TICKS * 2), 5, "clamped at the total");
    }

    /// A spare in the bag must not take the slot from the item that is worn,
    /// and an empty slot must always be filled whatever the flag says.
    #[test]
    fn a_bag_spare_never_takes_a_slot_from_the_item_that_is_worn() {
        let mut vm = loaded_guest();
        let loader = vm.symbol("Item_loadStorage").unwrap();
        let lr = loader + EQUIP_RESTORE_CALL + 4;
        // Two item-shaped blocks in the heap, one worn and one spare.
        let worn = vm.heap.alloc(0x100).unwrap();
        let spare = vm.heap.alloc(0x100).unwrap();
        vm.mem.w8(worn + ITEM_EQUIP_FLAG, 1);
        vm.mem.w8(spare + ITEM_EQUIP_FLAG, 0xff);

        let call = |vm: &mut Vm, item: u32, from: u32| {
            vm.cpu.reg_set(Mode::User, reg::LR, from);
            vm.cpu.reg_set(Mode::User, 0, 0);
            vm.cpu.reg_set(Mode::User, 1, 0);
            vm.cpu.reg_set(Mode::User, 2, item);
            vm.host_equip_restore().unwrap()
        };

        // Slot 0 is empty: even the spare goes in, so nobody is left unarmed.
        assert!(!call(&mut vm, spare, lr));
        // Put the worn one in for real, through the guest.
        let set = vm.symbol("ITEM_setEquip").unwrap();
        vm.call(set, &[0, 0, worn]).unwrap();
        // Now the spare is turned away, and the worn one is still there.
        assert!(call(&mut vm, spare, lr));
        let get = vm.symbol("ITEM_getEquip").unwrap();
        assert_eq!(vm.call(get, &[0, 0]).unwrap(), worn);
        // Any other caller of ITEM_setEquip is none of our business.
        assert!(!call(&mut vm, spare, lr + 4));
    }

    #[test]
    fn a_network_flag_reads_back_as_minus_one_and_a_normal_one_declines() {
        let mut vm = loaded_guest();
        let item = vm.heap.alloc(0x100).unwrap();
        let site = vm.symbol("Check_Hand").unwrap() + 0x4c;
        let enter = |vm: &mut Vm, flag: u8| {
            vm.mem.write_bytes(item + 8, &[flag]);
            vm.cpu.reg_set(Mode::User, 5, item);
            vm.cpu.reg_set(Mode::User, 3, 0xdead);
            vm.cpu.reg_set(Mode::User, reg::PC, site);
        };
        // Network item (flag 0..4): the host answers -1 and steps over.
        enter(&mut vm, 2);
        assert!(vm.host_net_item_flag(3, 5).unwrap());
        assert_eq!(vm.cpu.reg_get(Mode::User, 3), u32::MAX);
        assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), site + 4);
        // Normal item (flag -1): declined, the guest's own load runs.
        enter(&mut vm, 0xff);
        assert!(!vm.host_net_item_flag(3, 5).unwrap());
        assert_eq!(vm.cpu.reg_get(Mode::User, 3), 0xdead);
        assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), site);
    }

    #[test]
    fn the_drop_hook_appends_behind_the_shipped_entries_and_only_there() {
        let mut vm = loaded_guest();
        let vfs = std::fs::read("../payload/data.vfs.mp3").expect("payload/ is committed");
        let index = crate::files::VfsIndex::build(&vfs, 0, vfs.len() as u64);
        vm.netitems = crate::netitem::NetItems::build(&vfs, &index);
        assert!(!vm.netitems.is_empty());

        let m = vm.heap.alloc(0x400).unwrap();
        let fill = |vm: &mut Vm, slots: &[(u32, u16, u16, i32)]| {
            for i in 0..12u32 {
                vm.mem.w16(m + MON_DROP_KIND + 2 * i, 0xffff);
                vm.mem.w16(m + MON_DROP_IDX + 2 * i, 0xffff);
                vm.mem.write_bytes(m + MON_DROP_RATE + 4 * i, &(-1i32).to_le_bytes());
            }
            for &(i, k, ix, r) in slots {
                vm.mem.w16(m + MON_DROP_KIND + 2 * i, k);
                vm.mem.w16(m + MON_DROP_IDX + 2 * i, ix);
                vm.mem.write_bytes(m + MON_DROP_RATE + 4 * i, &r.to_le_bytes());
            }
        };
        let enter = |vm: &mut Vm| {
            vm.cpu.reg_set(Mode::User, 0, m);
            vm.cpu.reg_set(
                Mode::User,
                reg::LR,
                vm.symbol("MONSTER_dataLoad").unwrap() + MON_LOADCSV_RET,
            );
        };

        // A monster that drops maces (kind 0) in pool slots 2 and 3: the new
        // entry lands in slot 4, is a network mace, and the shipped slots are
        // untouched.
        fill(&mut vm, &[(0, 61, 5, 0x1000), (2, 0, 10, 0x2000), (3, 0, 12, 0x300)]);
        enter(&mut vm);
        assert!(!vm.host_net_item_drop().unwrap(), "the hook must always decline");
        assert_eq!(vm.mem.r16(m + MON_DROP_KIND + 2 * 4), 0, "injected kind");
        let idx = vm.mem.r16(m + MON_DROP_IDX + 2 * 4);
        assert!((36..=72).contains(&idx), "a network mace, got {idx}");
        let rate = i32::from_le_bytes([
            vm.mem.r8(m + MON_DROP_RATE + 16),
            vm.mem.r8(m + MON_DROP_RATE + 17),
            vm.mem.r8(m + MON_DROP_RATE + 18),
            vm.mem.r8(m + MON_DROP_RATE + 19),
        ]);
        assert_eq!(rate, net_drop_rate());
        assert_eq!(vm.mem.r16(m + MON_DROP_KIND + 2 * 2), 0);
        assert_eq!(vm.mem.r16(m + MON_DROP_IDX + 2 * 2), 10);
        assert_eq!(vm.mem.r16(m + MON_DROP_IDX + 2 * 3), 12);
        // Slot 0 (dropIndexItem's own roll) is not part of the pool.
        assert_eq!(vm.mem.r16(m + MON_DROP_IDX), 5);

        // No equipment drops: nothing is added.
        fill(&mut vm, &[(2, 25, 3, 0x1000)]); // kind 25 is a consumable
        enter(&mut vm);
        vm.host_net_item_drop().unwrap();
        assert_eq!(vm.mem.r16(m + MON_DROP_KIND + 2 * 3), 0xffff);

        // A full pool: nothing to overwrite, nothing added.
        let full: Vec<(u32, u16, u16, i32)> = (2..=10).map(|i| (i, 1u16, 4u16, 0x100)).collect();
        fill(&mut vm, &full);
        enter(&mut vm);
        vm.host_net_item_drop().unwrap();
        assert_eq!(vm.mem.r16(m + MON_DROP_IDX + 2 * 10), 4, "slot 10 kept");
        assert_eq!(vm.mem.r16(m + MON_DROP_KIND + 2 * 11), 0xffff, "slot 11 never touched");

        // The wrong LR — the second loadCsv call, or anyone else — declines
        // before reading anything.
        fill(&mut vm, &[(2, 0, 10, 0x2000)]);
        vm.cpu.reg_set(Mode::User, 0, m);
        vm.cpu.reg_set(Mode::User, reg::LR, vm.symbol("MONSTER_dataLoad").unwrap() + 0xc8);
        vm.host_net_item_drop().unwrap();
        assert_eq!(vm.mem.r16(m + MON_DROP_KIND + 2 * 3), 0xffff);
    }

    #[test]
    fn the_item_panel_shows_the_old_requirement_and_the_new_one() {
        let mut vm = loaded_guest();
        let (h2, draw_info) =
            (vm.symbol("HF_DrawTextOutlineH2").unwrap(), vm.symbol("drawItemInfo").unwrap());
        let line = vm.heap.alloc(64).unwrap();
        // `getInCodeString(0x157)` formatted: "r요구레벨 &45", escapes and all.
        let text = b"r\xbf\xe4\xb1\xb8\xb7\xb9\xba\xa7 &45\0";
        vm.mem.write_bytes(line, text);

        // The renderer is entered the way `drawItemInfo` enters it.
        let enter = |vm: &mut Vm, s: u32| {
            let sp = vm.cpu.reg_get(Mode::User, reg::SP) - 0x40;
            vm.cpu.reg_set(Mode::User, reg::SP, sp);
            vm.cpu.reg_set(Mode::User, reg::PC, h2);
            vm.cpu.reg_set(Mode::User, reg::LR, draw_info + ITEM_INFO_LEVEL_RET);
            for (i, v) in [0u32, 20, 30, 0x64].iter().enumerate() {
                vm.cpu.reg_set(Mode::User, i as u8, *v);
            }
            for (off, v) in
                [(H2_STR, s), (H2_LEN, 0x64), (H2_C1, u32::MAX), (H2_C2, 0), (H2_FLAGS, 0)]
            {
                vm.mem.w32(sp + off, v);
            }
            sp
        };

        // Nothing running: the shipped line is drawn untouched.
        enter(&mut vm, line);
        assert!(!vm.host_item_info_level().unwrap());

        let now = vm.wall_now_ms();
        vm.potions.grant(crate::potions::Kind::Finisher, now);
        let sp = enter(&mut vm, line);
        assert!(vm.host_item_info_level().unwrap());
        // The buffer handed to the renderer is the original with " 40" spliced
        // in behind the digits — the guest's own bytes, untouched ahead of them.
        let drawn = vm.mem.cstr_bytes(vm.potion_buf);
        assert_eq!(drawn, b"r\xbf\xe4\xb1\xb8\xb7\xb9\xba\xa7 &45 40".to_vec());
        // The guest's own argument block is left exactly as it was.
        assert_eq!(vm.mem.r32(sp + H2_STR), line);

        // A requirement the delta cannot lower has nothing to show.
        vm.mem.write_bytes(line, b"r\xbf\xe4\xb1\xb8\xb7\xb9\xba\xa7 &0\0");
        enter(&mut vm, line);
        assert!(!vm.host_item_info_level().unwrap());
        // Neither has a line with no number in it at all.
        vm.mem.write_bytes(line, b"r\xbf\xe4\xb1\xb8\xb7\xb9\xba\xa7\0");
        enter(&mut vm, line);
        assert!(!vm.host_item_info_level().unwrap());
        // `item[7]` is signed and the starting gear carries -1, which the game
        // prints as 요구레벨-1. Reading that as "1" and ruling it through was a
        // real bug, caught on a screenshot.
        vm.mem.write_bytes(line, b"r\xbf\xe4\xb1\xb8\xb7\xb9\xba\xa7 &-1\0");
        enter(&mut vm, line);
        assert!(!vm.host_item_info_level().unwrap());
    }

    /// The rule's x is the draw loop's pen, not the measurer's width, and the
    /// two differ by a pixel per glyph. Both are pinned here — against the
    /// guest's own `HF_GetWidth` and `HF_IsEscape`, and against strides read off
    /// a screenshot — because getting this wrong puts the rule beside the digits
    /// instead of through them, which is exactly what happened.
    ///
    /// Runs on an unbooted guest: both width leaves are constants
    /// (`HF_hangleWidth` = 9, `HF_getRealWidthEng` = 5) and `HF_IsEscape` is a
    /// table lookup, so none of it needs font state.
    #[test]
    fn the_rule_lands_on_the_pen_not_on_the_measured_width() {
        let mut vm = loaded_guest();
        let get_width = vm.symbol("HF_GetWidth").unwrap();
        let s = vm.heap.alloc(64).unwrap();
        // "r요구레벨 &45": escape, four syllables, space, escape, two digits.
        let text = b"r\xbf\xe4\xb1\xb8\xb7\xb9\xba\xa7 &45\0";
        vm.mem.write_bytes(s, text);

        // What the measurer says: seeded at 1, escapes free, syllable 9, space 4.
        assert_eq!(vm.call(get_width, &[s, 11, 0]).unwrap() as i32, 1 + 4 * 9 + 4);
        // Where the pen actually is: one more per drawn glyph (four syllables
        // and the space), and the seed is one of them. Measured on screen as a
        // stride of 10 per syllable from a line starting at pen + 1.
        assert_eq!(vm.pen_offset(s, text, 11).unwrap(), 4 * 10 + 5);
        // The rule then spans one digit cell per digit, 5 + 1 wide.
        let (x0, x1) = (vm.pen_offset(s, text, 11).unwrap(), vm.pen_offset(s, text, 13).unwrap());
        assert_eq!(x1 - x0, 2 * 6);
        // An escape costs nothing, in either arithmetic.
        assert_eq!(vm.pen_offset(s, text, 1).unwrap(), 0);
    }

    #[test]
    fn an_experience_award_is_multiplied_at_the_door() {
        let mut vm = loaded_guest();
        let entry = vm.symbol("HERO_add_Exp").unwrap();
        vm.cpu.reg_set(Mode::User, reg::PC, entry);

        let award = |vm: &mut Vm, amount: u32| {
            vm.cpu.reg_set(Mode::User, 1, amount);
            // Always declines: the shipped body runs with the new argument.
            assert!(!vm.host_exp_mul().unwrap());
            vm.cpu.reg_get(Mode::User, 1)
        };
        assert_eq!(award(&mut vm, 100), 100, "no buff, no change");

        let now = vm.wall_now_ms();
        vm.potions.grant(Kind::Exp2, now);
        assert_eq!(award(&mut vm, 100), 200);

        vm.potions.grant(Kind::Exp4, now);
        assert_eq!(award(&mut vm, 100), 400, "the larger of the two wins");

        // A zero or negative award is left alone rather than scaled.
        assert_eq!(award(&mut vm, 0), 0);
        assert_eq!(award(&mut vm, u32::MAX), u32::MAX);

        // And it stops when the week does.
        vm.potions = PotionState::default();
        assert_eq!(award(&mut vm, 100), 100);
    }

    /// The exp gap hook is pinned to two instruction words and two decoded
    /// `bl` targets; this keeps all four honest against the shipped binary.
    #[test]
    fn the_exp_gap_gate_still_sits_on_its_compare_and_its_two_calls() {
        let mut vm = loaded_guest();
        let (drop, add) =
            (vm.symbol("MONSTER_dropExp").unwrap(), vm.symbol("HERO_add_Exp").unwrap());
        assert_eq!(
            exp_gap_site(drop, add, &mut vm.mem),
            Some(drop + EXP_GAP_CMP),
            "the gate at MONSTER_dropExp+{EXP_GAP_CMP:#x} drifted"
        );
    }

    #[test]
    fn a_kill_past_the_level_gap_pays_decayed_experience_instead_of_none() {
        let mut vm = loaded_guest();
        let drop = vm.symbol("MONSTER_dropExp").unwrap();
        let site = drop + EXP_GAP_CMP;
        vm.exp_gap_lrs = EXP_GAP_LRS.map(|lr| drop + lr);

        let gate = |vm: &mut Vm, gap: u32| {
            vm.cpu.reg_set(Mode::User, reg::PC, site);
            vm.cpu.reg_set(Mode::User, 2, gap);
            vm.host_exp_gap_gate().unwrap()
        };
        // Within the shipped threshold the gate declines: the guest's own
        // compare and branch run, and the award is the shipped full award.
        assert!(!gate(&mut vm, 5));
        assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), site, "a declined gate must not move PC");
        // Past it, the compare and the no-experience exit are skipped.
        assert!(gate(&mut vm, 7));
        assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), site + 8);

        // The award is then halved per level past five...
        let entry = vm.symbol("HERO_add_Exp").unwrap();
        let award = |vm: &mut Vm, lr: u32, amount: u32| {
            vm.cpu.reg_set(Mode::User, reg::PC, entry);
            vm.cpu.reg_set(Mode::User, reg::LR, lr);
            vm.cpu.reg_set(Mode::User, 1, amount);
            assert!(!vm.host_exp_mul().unwrap());
            vm.cpu.reg_get(Mode::User, 1)
        };
        // A level-19 monster at hero level 26: 19 x 8 = 152, two halvings.
        assert_eq!(award(&mut vm, drop + EXP_GAP_LRS[0], 152), 38);
        assert_eq!(award(&mut vm, drop + EXP_GAP_LRS[1], 152), 38, "the pet's call decays too");
        // ...but never to nothing, however far the gap grows.
        assert!(gate(&mut vm, 96));
        assert_eq!(award(&mut vm, drop + EXP_GAP_LRS[0], 152), 1);

        // A quest reward arriving while the remembered gap is stale returns
        // to a different address and is left whole.
        assert_eq!(award(&mut vm, 0x52eb0, 152), 152);

        // And the growth 비약 multiply what the decay leaves.
        assert!(gate(&mut vm, 6));
        let now = vm.wall_now_ms();
        vm.potions.grant(Kind::Exp2, now);
        assert_eq!(award(&mut vm, drop + EXP_GAP_LRS[0], 152), 152);
    }

    #[test]
    fn an_equip_gate_is_answered_five_levels_lower() {
        let mut vm = loaded_guest();
        let sites = item_level_sites(&vm.elf.symbols, &mut vm.mem);
        let now = vm.wall_now_ms();
        vm.potions.grant(Kind::Finisher, now);
        let item = vm.heap.alloc(0x100).unwrap();

        for (at, patch) in sites {
            let Patch::ItemLevel { rd, rn } = patch else { panic!("wrong patch") };
            let answer = |vm: &mut Vm, req: u8| {
                vm.mem.w8(item + 7, req);
                vm.cpu.reg_set(Mode::User, rn, item);
                vm.cpu.reg_set(Mode::User, reg::PC, at);
                assert!(vm.host_item_level(rd, rn).unwrap());
                // The instruction it stands in for has been retired.
                assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), at + 4);
                vm.cpu.reg_get(Mode::User, rd)
            };
            assert_eq!(answer(&mut vm, 45), 40, "{at:#x}");
            // Clamped at zero rather than wrapping into "level 251 required".
            assert_eq!(answer(&mut vm, 3), 0, "{at:#x}");
            // The game's "no requirement" byte has bit 7 set and must stay
            // negative — the load is unsigned, so it comes back as the byte.
            assert_eq!(answer(&mut vm, 0xfe), 0xfe, "{at:#x}");
        }

        // Nothing running, nothing changed: the interpreter runs the original.
        vm.potions = PotionState::default();
        let (at, patch) = item_level_sites(&vm.elf.symbols, &mut vm.mem)[0];
        let Patch::ItemLevel { rd, rn } = patch else { panic!("wrong patch") };
        vm.cpu.reg_set(Mode::User, reg::PC, at);
        assert!(!vm.host_item_level(rd, rn).unwrap());
        assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), at);
    }

    #[test]
    fn an_item_that_is_not_there_is_left_to_the_interpreter() {
        let mut vm = loaded_guest();
        let sites = item_level_sites(&vm.elf.symbols, &mut vm.mem);
        let now = vm.wall_now_ms();
        vm.potions.grant(Kind::Finisher, now);
        let (at, patch) = sites[0];
        let Patch::ItemLevel { rd, rn } = patch else { panic!("wrong patch") };
        // A null item: the guest would fault here, and so must this.
        vm.cpu.reg_set(Mode::User, rn, 0);
        vm.cpu.reg_set(Mode::User, reg::PC, at);
        assert!(!vm.host_item_level(rd, rn).unwrap());
        assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), at);
    }

    /// The touch-scheme byte belongs to the player, and the shipped
    /// configuration must never write over a value the settings screen made.
    /// This is the whole bug: the old default forced 0 after every load, so
    /// 터치방식 fell back to TYPE A on every cold boot and every trip back to
    /// the main menu.
    #[test]
    fn the_touch_scheme_byte_is_only_touched_when_the_game_cannot_reach_it() {
        // Shipping defaults: whatever the save says survives, untouched.
        assert_eq!(pad_target(0, -1, true), None);
        assert_eq!(pad_target(1, -1, true), None);
        // A value the settings screen cannot produce pins the fixed pad with
        // no way back — that one, and only that one, is normalised.
        assert_eq!(pad_target(2, -1, true), Some(0));
        assert_eq!(pad_target(0xff, -1, true), Some(0));
        // Idempotent, which is why this needs no "first boot" marker: the
        // loader runs several times per process.
        assert_eq!(pad_target(pad_target(2, -1, true).unwrap(), -1, true), None);
        // Opting out of the migration leaves even that alone.
        assert_eq!(pad_target(2, -1, false), None);
        // An explicit `CS_PAD_MODE` still forces, for repro and debugging.
        assert_eq!(pad_target(1, 0, true), Some(0));
        assert_eq!(pad_target(0, 2, true), Some(2));
    }

    /// The migration's premise, checked against the shipped binary rather than
    /// trusted: both arrow paths of the 터치방식 row clamp to `{0, 1}`, so a
    /// byte above 1 really is unreachable from inside the game.
    #[test]
    fn the_settings_row_still_clamps_the_touch_scheme_to_two_values() {
        let mut vm = loaded_guest();
        let key = vm.symbol("MainMenuOptionKey").expect("named in .symtab");
        // LEFT: `strb r2, [r3, #0x14]` then `strbne r2, [r3, #0x14]` — the
        // wrap that turns 0 - 1 back into 1.
        assert_eq!(vm.mem.r32(key + 0xb50), 0xe5c3_2014, "LEFT store drifted");
        assert_eq!(vm.mem.r32(key + 0xb58), 0x15c3_2014, "LEFT wrap drifted");
        // RIGHT: `strb r2, [r3, #0x14]`, and `strb r0, [r3, #0x14]` on the
        // branch that puts 2 back to 0.
        assert_eq!(vm.mem.r32(key + 0xd24), 0xe5c3_2014, "RIGHT store drifted");
        assert_eq!(vm.mem.r32(key + 0xd30), 0xe5c3_0014, "RIGHT wrap drifted");
        // And the byte the whole patch is about is where we think it is.
        let opt = vm.symbol("_menuOpt").expect("named in .symtab");
        assert_eq!(opt & 0xff_ffff, 0x17ab18);
        assert_eq!(MENU_OPT_PAD, 0x14);
    }

    /// The cube's item-compose popup shares the bottom-left D-pad with every
    /// other menu, and its four `Check_Rect` copies sit in the *tail* of
    /// `GameMenuFrame_KeyPressed`, past the `_stack` symbol the linker parked
    /// at `+0x80000`. While the handler window was computed as "up to the next
    /// symbol" that tail was outside it, so the taps were tested against the
    /// shipped boxes while the sprites were drawn scaled in the new corner —
    /// the arrows looked big and answered small.
    ///
    /// Three claims, all against the shipped binary: `_stack` really is inside
    /// The margins clear the one button the cluster can reach.
    ///
    /// Moving the D-pad off its corner moves its **hit** boxes too, and they
    /// are tested before everything else on their screen, so a margin that
    /// looks fine can silently steal a tap. Exactly one rectangle in the
    /// sixteen handlers is in reach: `Compose_RecipeKey+0x864`, the 72x16
    /// button at `(0x50, H - 0x10)` on the cube's compose screen. Its five
    /// words are read back here rather than trusted, and then the down arrow —
    /// the only one of the four that can touch it — is required to miss it at
    /// the values that ship.
    ///
    /// This is what keeps `CS_DPAD_DX` from being raised on its own: at 125 %
    /// the arrow's right edge is `dx + 75`, so any `dx > 5` overlaps unless
    /// `dy` has already lifted the arrow above `y = 224`.

    /// The 장비 tab's cell grid is what the look cells are placed against, so
    /// the numbers this file keeps are the ones the tab's own loop uses, and
    /// its two draw calls still go where the cells send theirs.

    /// 터치방식 B 의 월드 패드는 이 파일이 적어 둔 네 자리에서 그려지고 재어진다.
    ///
    /// 복귀 주소가 곧 방향이라는 것이 이 패치의 전제이므로, 그 주소가 정말
    /// 그 두 함수의 `bl` 다음인지를 디코드해서 본다 — 아니면 우리는 엉뚱한
    /// 그림을 지우고 엉뚱한 사각형을 옮기는 것이다.
    #[test]
    fn the_world_pad_is_drawn_and_measured_where_this_file_says() {
        let mut vm = loaded_guest();
        let draw = vm.symbol("Draw_TouchPad").expect("in .symtab");
        let rot = vm.symbol("DM_DrawImgRotateEx").expect("in .symtab");
        let keypad = vm.symbol("GameFrame_PressTouchKeypad").expect("in .symtab");
        let check = vm.symbol("Check_Rect").expect("in .symtab");
        for (off, dir) in WORLD_PAD_DRAW {
            assert_eq!(
                branch_target(draw + off - 4, &mut vm.mem, true),
                Some(rot),
                "{dir:?} does not return from a DM_DrawImgRotateEx"
            );
        }
        for (off, _, dir) in WORLD_PAD_HIT {
            assert_eq!(
                branch_target(keypad + off - 4, &mut vm.mem, true),
                Some(check),
                "{dir:?} does not return from a Check_Rect"
            );
        }
        // Four sites, four directions, on both halves — a repeat would make
        // two arrows share a plate.
        for list in [
            WORLD_PAD_DRAW.iter().map(|&(o, d)| (o, d)).collect::<Vec<_>>(),
            WORLD_PAD_HIT.iter().map(|&(o, _, d)| (o, d)).collect::<Vec<_>>(),
        ] {
            for (i, &(o, d)) in list.iter().enumerate() {
                for &(o2, d2) in &list[i + 1..] {
                    assert_ne!(o, o2, "two sites share an offset");
                    assert_ne!(d, d2, "two sites share a direction");
                }
            }
        }
        // The shipped boxes this patch replaces are the world pad's, not the
        // menu's: the two sets of sizes do not meet.
        for (_, (_, _, w, h), _) in WORLD_PAD_HIT {
            assert!((w, h) == (0x39, 0x26) || (w, h) == (0x26, 0x39), "{w}x{h}");
            assert!(!MENU_ARROW_RECTS.iter().any(|&(_, _, mw, mh)| (mw, mh) == (w, h)));
        }
    }

    /// 네 판이 한 십자 위에 있고, 클러스터가 정확히 `2*ARM + SHORT` 정사각이며,
    /// **위/아래 판이 좌/우 판의 90° 회전**이다.
    ///
    /// 마지막 것이 제보였다 — 넷이 같은 모양이 아니면 D-pad 로 안 읽힌다.
    /// 그 정사각은 「여백 + 크기」 계산의 전부이기도 하다: 메뉴 상자와의
    /// 거리도, 충돌 테스트도 이 값을 쓴다.
    #[test]
    fn the_cluster_is_four_plates_on_one_cross() {
        let (h, s, dx, dy) = (240, 100, 40, 26);
        // 회전이라 묶음은 정사각이다.
        let span = 2 * DPAD_ARM + dpad_thick();
        let mut bbox = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        let mut boxes = Vec::new();
        for dir in MENU_ARROW_DIRS {
            let (x, y, w, ph) = dpad_plate(dir, h, s, dx, dy);
            // 가리키는 방향을 가로지르는 쪽이 길다.
            let across = matches!(dir, Dir::Up | Dir::Down) == (w > ph);
            assert!(across, "{dir:?} is {w}x{ph}, not fat across the way it points");
            bbox = (bbox.0.min(x), bbox.1.min(y), bbox.2.max(x + w), bbox.3.max(y + ph));
            boxes.push((dir, x + w / 2, y + ph / 2, w, ph));
        }
        let at = |d: Dir| boxes.iter().find(|b| b.0 == d).copied().unwrap();
        let size = |d: Dir| {
            let b = at(d);
            (b.3, b.4)
        };
        // 네 중심이 한 십자 위에 있고 팔 길이가 하나다. 기준점을 여기서 다시
        // 계산하지 않는다 — 그러면 `dpad_plate` 의 반올림을 베끼게 되고, 그게
        // 이 테스트가 잡아야 할 바로 그것이다.
        let (cx, cy) = (at(Dir::Up).1, at(Dir::Left).2);
        for (dir, want) in [
            (Dir::Up, (cx, cy - DPAD_ARM)),
            (Dir::Down, (cx, cy + DPAD_ARM)),
            (Dir::Left, (cx - DPAD_ARM, cy)),
            (Dir::Right, (cx + DPAD_ARM, cy)),
        ] {
            let b = at(dir);
            assert_eq!((b.1, b.2), want, "{dir:?} is off the cross");
        }
        // 같은 모양의 회전: 마주 보는 짝끼리 크기가 같고, 눕힌 짝은 세운 짝의
        // `(w, h)` 가 뒤집힌 것이다.
        assert_eq!(size(Dir::Up), size(Dir::Down));
        assert_eq!(size(Dir::Left), size(Dir::Right));
        let (uw, uh) = size(Dir::Up);
        assert_eq!(size(Dir::Left), (uh, uw), "the up plate is not the left plate turned 90°");
        assert_eq!(
            (bbox.2 - bbox.0, bbox.3 - bbox.1),
            (span, span),
            "the cluster is the wrong size"
        );
        assert_eq!((bbox.0, bbox.3), (dx, h - dy), "the cluster is not on its margins");
    }

    /// 월드 고정 패드의 상자는 **메뉴 D-pad 와 한 자리 한 간격**이고, 판보다
    /// 사방 `slop` 만큼만 넓다.
    ///
    /// 제보가 이 테스트다 — 「인게임 화살표와 메뉴 화살표의 위치가 같아야
    /// 하는데 지금 서로 다르다, 간격도 너무 넓다」. 그래서 출고 사각형 넷을
    /// 게스트가 내미는 그대로 훅에 물려 보고, 돌아온 상자의 **중심이 메뉴
    /// 판의 중심과 같은지**를 본다. 넓어지는 것은 판정뿐이라는 것도 같이.
    #[test]
    fn the_world_pad_lands_on_the_same_cross_as_the_menu_one() {
        assert!(dpad_hex(), "this test needs the shipped CS_DPAD_HEX default");
        let mut vm = loaded_guest();
        // 이 테스트는 `CS_JOY_HERO5=0` 일 때의 육각 플레이트 경로를 고정한다 —
        // 기본값에서는 hero5 조이스틱이 이 네 사이트를 먼저 가져간다.
        vm.joy_base = None;
        vm.menu_arrow_keys = menu_arrow_key_ranges(&vm.elf);
        let lcd_g = vm.symbol("_lcdG").expect("in .symtab");
        let (h, w) = (240i32, 400i32);
        vm.mem.w32(lcd_g + LCD_H, h as u32);
        vm.mem.w32(lcd_g + LCD_W, w as u32);
        let keypad = vm.symbol("GameFrame_PressTouchKeypad").expect("in .symtab");
        let sp = vm.cpu.reg_get(Mode::User, reg::SP) - 0x20;
        vm.cpu.reg_set(Mode::User, reg::SP, sp);
        let s = dpad_scale();
        let (dx, dy) = (dpad_dx_for(w, s), dpad_dy());
        let t = dpad_slop() * s / 100;
        let mut boxes = Vec::new();
        for (off, (rx, rd, rw, rh), dir) in WORLD_PAD_HIT {
            vm.cpu.reg_set(Mode::User, reg::LR, keypad + off);
            vm.cpu.reg_set(Mode::User, 2, rx as u32);
            vm.cpu.reg_set(Mode::User, 3, (h - rd) as u32);
            vm.mem.w32(sp, rw as u32);
            vm.mem.w32(sp + 4, rh as u32);
            // `Check_Rect` 자신은 그대로 돌아야 한다 — 진동도 판정도 게스트 것이다.
            assert!(!vm.host_menu_arrow_hit().unwrap(), "{dir:?} was served");
            let got = (
                vm.cpu.reg_get(Mode::User, 2) as i32,
                vm.cpu.reg_get(Mode::User, 3) as i32,
                vm.mem.r32(sp) as i32,
                vm.mem.r32(sp + 4) as i32,
            );
            // 그려지는 판 — 그리기 훅이 부르는 그 호출이다.
            let (px, py, pw, ph) = dpad_plate(dir, h, s, dx, dy);
            assert_eq!(got, (px - t, py - t, pw + 2 * t, ph + 2 * t), "{dir:?}");
            // 중심이 같다는 것이 「같은 자리 같은 간격」의 전부다.
            assert_eq!(
                (2 * got.0 + got.2, 2 * got.1 + got.3),
                (2 * px + pw, 2 * py + ph),
                "{dir:?} is not concentric with its plate"
            );
            boxes.push((dir, got));
        }
        // 가운데는 아무 상자도 안 덮는다 — 안 그러면 엄지를 가운데 두는 것만으로
        // 걷는다.
        let span = 2 * (DPAD_ARM * s / 100) + dpad_thick() * s / 100;
        let (cx, cy) = (dx + span / 2, h - dy - (span - span / 2));
        for (dir, (bx, by, bw, bh)) in boxes {
            assert!(
                !(bx <= cx && cx <= bx + bw && by <= cy && cy <= by + bh),
                "the {dir:?} box covers the cluster centre"
            );
        }
    }

    /// hero5 조이스틱의 훅 자리 열여섯이 이 파일이 말하는 그것인지 — draw
    /// 사이트 12개는 `bl DM_DrawImgRotateEx`, 히트 사이트 넷은 `bl Check_Rect`,
    /// 그리고 근거 워드 일곱. **하나라도 바꾸면 설치가 거부되는지**까지.
    #[test]
    fn the_hero5_joystick_sites_are_where_this_file_says() {
        let mut vm = loaded_guest();
        let base = vm.symbol("Draw_TouchPad").expect("in .symtab");
        assert_eq!(vm.joy_sites_ok(), Some(base));
        // 워드 하나라도 다르면 통째로 미설치다 — draw 쪽도 MOVE 경로 쪽도.
        for (off, w) in JOY_WORDS {
            let at = base + off;
            vm.mem.w32(at, w ^ 1);
            assert_eq!(vm.joy_sites_ok(), None, "word at +{off:#x}");
            vm.mem.w32(at, w);
        }
        let ohe = vm.symbol("onHandleEvent").expect("in .symtab");
        for (off, w) in JOY_MOVE_WORDS {
            let at = ohe + off;
            vm.mem.w32(at, w ^ 1);
            assert_eq!(vm.joy_sites_ok(), None, "move word at +{off:#x}");
            vm.mem.w32(at, w);
        }
        // `bl` 을 하나 다른 데로 돌려도 미설치다 — draw 와 move rect 양쪽.
        for at in [base + JOY_FLOAT_FIRST - 4, ohe + WORLD_PAD_MOVE_HIT[0].0 - 4] {
            let had = vm.mem.r32(at);
            vm.mem.w32(at, had ^ 2);
            assert_eq!(vm.joy_sites_ok(), None, "a moved bl must refuse the install");
            vm.mem.w32(at, had);
        }
        assert_eq!(vm.joy_sites_ok(), Some(base));
        // 스킵 목록과 두 「첫」 사이트가 서로 겹치지 않는다 — 겹치면 한
        // 프레임에 스틱이 두 번 그려지거나 아예 안 그려진다.
        let mut all: Vec<u32> = JOY_SKIP_DRAWS.to_vec();
        all.push(JOY_FIXED_FIRST);
        all.push(JOY_FLOAT_FIRST);
        let n = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), n, "a draw site is listed twice");
    }

    /// hero5 스틱이 월드 패드의 draw 자리에서 그려진다 — 고정(TYPE B)은
    /// 앵커 (65, H−65)에, 자유(TYPE A)는 호출 인자가 실어 온 앵커에, 방향은
    /// `mainFrame+0x6c` 대로 노브가 밀리고, 스틱 상자 밖은 한 픽셀도 안
    /// 바뀐다.
    #[test]
    fn the_hero5_stick_draws_at_the_anchor_and_only_there() {
        let mut vm = loaded_guest();
        vm.joy_base = vm.joy_sites_ok();
        assert!(vm.joy_base.is_some());
        let base = vm.joy_base.unwrap();
        let lcd_g = vm.symbol("_lcdG").expect("in .symtab");
        let mf = vm.symbol("mainFrame").expect("in .symtab");
        let (w, h) = (400i32, 240i32);
        let px = vm.heap.alloc((w * h * 2) as u32).expect("guest heap");
        let surf = vm.heap.alloc(0x18).expect("guest heap");
        vm.mem.w32(surf, w as u32);
        vm.mem.w32(surf + 4, h as u32);
        vm.mem.w32(surf + 8, (w * 2) as u32);
        vm.mem.w32(surf + 0xc, 16);
        vm.mem.w32(surf + 0x10, px);
        vm.mem.w32(lcd_g + 4, surf);
        vm.mem.w32(lcd_g + LCD_W, w as u32);
        vm.mem.w32(lcd_g + LCD_H, h as u32);

        let fill = |vm: &mut Vm| {
            for i in 0..(w * h) {
                vm.mem.w16(px + 2 * i as u32, 0x1234);
            }
        };
        let changed = |vm: &mut Vm| -> Vec<(i32, i32)> {
            let mut v = Vec::new();
            for y in 0..h {
                for x in 0..w {
                    if vm.mem.r16(px + 2 * (y * w + x) as u32) != 0x1234 {
                        v.push((x, y));
                    }
                }
            }
            v
        };
        let mut draw_at = |vm: &mut Vm, off: u32, dir_key: i32, ax: i32, ay: i32| {
            fill(vm);
            vm.mem.w32(mf + JOY_MF_DIR, dir_key as u32);
            vm.cpu.reg_set(Mode::User, 1, ax as u32);
            vm.cpu.reg_set(Mode::User, 2, (ay - 0x28) as u32);
            vm.cpu.reg_set(Mode::User, reg::LR, base + off);
            assert!(vm.host_menu_arrow().unwrap(), "+{off:#x} was not served");
            assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), base + off, "PC did not move to LR");
            changed(vm)
        };

        // 고정: idle(방향 없음) — 앵커 (65,175) 의 73x73 안에서만 바뀐다.
        let idle = draw_at(&mut vm, JOY_FIXED_FIRST, -999, 0, 0);
        assert!(!idle.is_empty(), "the idle plate drew nothing");
        for &(x, y) in &idle {
            assert!((29..=101).contains(&x) && (139..=211).contains(&y), "idle drew at ({x},{y})");
        }
        // 고정: 방향이 잡히면 밝은 판 — idle 과 다른 그림이고, 노브가 그
        // 방향으로 민 만큼 상자가 커진다.
        let up = draw_at(&mut vm, JOY_FIXED_FIRST, -1, 0, 0);
        assert_ne!(idle, up, "held must differ from idle");
        let down = draw_at(&mut vm, JOY_FIXED_FIRST, -2, 0, 0);
        let (up_min_y, down_max_y) = (
            up.iter().map(|&(_, y)| y).min().unwrap(),
            down.iter().map(|&(_, y)| y).max().unwrap(),
        );
        assert!(up_min_y < 139 || down_max_y > 211, "the knob never deflected");
        assert_ne!(up, down, "up and down draw the same picture");

        // 자유: 앵커가 인자로 온다 — (200,120) 에 같은 판이 선다.
        let float = draw_at(&mut vm, JOY_FLOAT_FIRST, -999, 200, 120);
        assert!(!float.is_empty());
        for &(x, y) in &float {
            assert!(
                (200 - 37..=200 + 37).contains(&x) && (120 - 37..=120 + 37).contains(&y),
                "float drew at ({x},{y})"
            );
        }

        // 나머지 draw 사이트는 그냥 지나간다 — 픽셀 0, PC 는 LR.
        for off in JOY_SKIP_DRAWS {
            fill(&mut vm);
            vm.cpu.reg_set(Mode::User, reg::LR, base + off);
            assert!(vm.host_menu_arrow().unwrap());
            assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), base + off);
            assert!(changed(&mut vm).is_empty(), "+{off:#x} drew something");
        }
    }

    /// TYPE B 의 히트 넷이 hero5 의 판정으로 서브된다 — 우세축, 동률은 세로,
    /// 데드존, 도달 반경. 사각형 넷이 아니라 사분면이라는 것이 요점이다.
    #[test]
    fn the_world_pad_hits_are_served_with_hero5s_quantizer() {
        let mut vm = loaded_guest();
        vm.joy_base = vm.joy_sites_ok();
        assert!(vm.joy_base.is_some());
        let lcd_g = vm.symbol("_lcdG").expect("in .symtab");
        let (w, h) = (400i32, 240i32);
        vm.mem.w32(lcd_g + LCD_W, w as u32);
        vm.mem.w32(lcd_g + LCD_H, h as u32);
        let keypad = vm.symbol("GameFrame_PressTouchKeypad").expect("in .symtab");
        let sp = vm.cpu.reg_get(Mode::User, reg::SP) - 0x20;
        vm.cpu.reg_set(Mode::User, reg::SP, sp);
        let ohe = vm.symbol("onHandleEvent").expect("in .symtab");
        let (ax, ay) = (crate::joy::ANCHOR_X, h - crate::joy::ANCHOR_BOTTOM);
        let mf = vm.symbol("mainFrame").expect("in .symtab");
        // PRESS 사이트(`GameFrame_PressTouchKeypad`)는 언제나 hero5 그대로의
        // **잡기** 판정이다 — 직전 방향이 서 있어도 안 본다(새 손가락이다).
        let press_cases: [(i32, i32, i32, Option<Dir>); 9] = [
            (-999, ax, ay - 30, Some(Dir::Up)),
            (-999, ax + 30, ay, Some(Dir::Right)),
            (-999, ax - 30, ay + 5, Some(Dir::Left)),
            (-999, ax + 5, ay + 30, Some(Dir::Down)),
            (-999, ax + 20, ay + 20, Some(Dir::Down)), // 동률은 세로
            (-999, ax + 8, ay - 8, None),              // 데드존
            (-999, ax + joy_reach() + 1, ay, None),    // 잡기 반경 밖
            (-999, ax + 45, ay - 55, Some(Dir::Up)),   // hero5 q1 그대로
            (-4, ax + 8, ay - 8, None),                // 직전 방향은 press 를 안 살린다
        ];
        for (prev, tx, ty, want) in press_cases {
            vm.mem.w32(mf + JOY_MF_PREV, prev as u32);
            for (off, (rx, rd, rw, rh), dir) in WORLD_PAD_HIT {
                vm.mem.w32(mf + JOY_MF_PREV, prev as u32);
                vm.cpu.reg_set(Mode::User, reg::LR, keypad + off);
                vm.cpu.reg_set(Mode::User, 0, tx as u32);
                vm.cpu.reg_set(Mode::User, 1, ty as u32);
                vm.cpu.reg_set(Mode::User, 2, rx as u32);
                vm.cpu.reg_set(Mode::User, 3, (h - rd) as u32);
                vm.mem.w32(sp, rw as u32);
                vm.mem.w32(sp + 4, rh as u32);
                assert!(vm.host_menu_arrow_hit().unwrap(), "{dir:?} was not served");
                assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), keypad + off);
                let hit = vm.cpu.reg_get(Mode::User, 0) != 0;
                assert_eq!(hit, want == Some(dir), "press ({tx},{ty}) vs {dir:?}");
                // 잡힌 press 는 첫 이동의 히스테리시스 공백을 메우려고
                // `+0x70` 을 채운다; 빗나간 press 는 남의 걸음을 안 지운다.
                let after = vm.mem.r32(mf + JOY_MF_PREV) as i32;
                if hit {
                    assert_eq!(crate::joy::dir_of_wipi(after), Some(dir), "+0x70 not primed");
                } else {
                    assert_eq!(after, prev, "a missed press must leave +0x70 alone");
                }
            }
        }
        // MOVE 사이트(`onHandleEvent`)는 `+0x70` 의 직전 방향으로 히스테리시스 —
        // 쥔 뒤에는 데드존 흔들림·잡기 반경 밖에서도 안 놓인다. `+0x6c` 는
        // 그 경로가 rect 검사 전에 −999 로 지우는 값 그대로 둔다.
        vm.mem.w32(mf + JOY_MF_DIR, (-999i32) as u32);
        let move_cases: [(i32, i32, i32, Option<Dir>); 6] = [
            (-999, ax + 30, ay, Some(Dir::Right)), // 안 쥔 채의 이동은 잡기 판정
            (-999, ax + 8, ay - 8, None),
            (-4, ax + 8, ay - 8, Some(Dir::Right)), // 쥔 채 데드존 흔들림: 유지
            (-4, ax + joy_reach() + 40, ay, Some(Dir::Right)), // 쥔 채 판 밖: 유지
            (-4, ax + 2, ay + 1, None),             // 정중앙 되돌림: 놓인다
            (-4, ax + 20, ay + 22, Some(Dir::Right)), // 5/4 미만 우세: 축 유지
        ];
        for (prev, tx, ty, want) in move_cases {
            for (off, (rx, rd, rw, rh), dir) in WORLD_PAD_MOVE_HIT {
                vm.mem.w32(mf + JOY_MF_PREV, prev as u32);
                vm.cpu.reg_set(Mode::User, reg::LR, ohe + off);
                vm.cpu.reg_set(Mode::User, 0, tx as u32);
                vm.cpu.reg_set(Mode::User, 1, ty as u32);
                vm.cpu.reg_set(Mode::User, 2, rx as u32);
                vm.cpu.reg_set(Mode::User, 3, (h - rd) as u32);
                vm.mem.w32(sp, rw as u32);
                vm.mem.w32(sp + 4, rh as u32);
                assert!(vm.host_menu_arrow_hit().unwrap(), "move {dir:?} was not served");
                assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), ohe + off);
                let hit = vm.cpu.reg_get(Mode::User, 0) != 0;
                assert_eq!(hit, want == Some(dir), "prev {prev}, move ({tx},{ty}) vs {dir:?}");
                // MOVE 쪽은 `+0x70` 을 게스트 팔이 관리한다 — 서브는 안 쓴다.
                assert_eq!(vm.mem.r32(mf + JOY_MF_PREV) as i32, prev);
            }
        }
    }

    // ---- 그리기 리프 다섯 ---------------------------------------------------

    /// Run a leaf's own shipped body, with its host patch out of the table, and
    /// give back what it returned. `Vm::call` saves and restores the whole CPU,
    /// so `r0` afterwards is the caller's, not the callee's.
    fn guest_leaf(vm: &mut Vm, kind: Patch, base: u32, args: &[u32]) -> u32 {
        let keep = vm.patches.clone();
        vm.patches.retain(|&(_, p)| p != kind);
        let r = vm.call(base, args).expect("the guest's own leaf runs");
        vm.patches = keep;
        r
    }

    /// `MapClip` clamps the way the shipped body does, on every corner of the
    /// range that matters: negative, inside, exactly on the edge, and past it.
    #[test]
    fn the_guest_agrees_with_our_map_clip() {
        let mut vm = loaded_guest();
        let base = vm.symbol("MapClip").expect("in .symtab");
        assert!(map_clip_site(base, &mut vm.mem), "the shipped words are still there");
        for (off, w) in MAP_CLIP_WORDS {
            let had = vm.mem.r32(base + off);
            vm.mem.w32(base + off, w ^ 1);
            assert!(!map_clip_site(base, &mut vm.mem), "word at {off:#x}");
            vm.mem.w32(base + off, had);
        }

        let lcd = vm.symbol("_lcdG").expect("in .symtab");
        let slots: Vec<u32> = ["nMapClipX1", "nMapClipY1", "nMapClipX2", "nMapClipY2"]
            .iter()
            .map(|n| vm.symbol(n).expect("in .symtab"))
            .collect();
        for &(sw, sh) in &[(400i32, 240i32), (533, 240)] {
            vm.mem.w32(lcd + LCD_W, sw as u32);
            vm.mem.w32(lcd + LCD_H, sh as u32);
            for &x1 in &[-100i32, -1, 0, 7, sw, sw + 40] {
                for &y1 in &[-9i32, 0, 5, sh + 3] {
                    for &(x2, y2) in &[(-3i32, -3i32), (0, 0), (10, 10), (sw, sh), (sw + 1, sh + 1)]
                    {
                        let args = [x1 as u32, y1 as u32, x2 as u32, y2 as u32];
                        let ret = guest_leaf(&mut vm, Patch::MapClip, base, &args);
                        let want: Vec<u32> = slots.iter().map(|&a| vm.mem.r32(a)).collect();

                        for &a in &slots {
                            vm.mem.w32(a, 0xdead_beef);
                        }
                        for (i, &v) in args.iter().enumerate() {
                            vm.cpu.reg_set(Mode::User, i as u8, v);
                        }
                        vm.cpu.reg_set(Mode::User, reg::LR, 0x1234);
                        assert!(vm.host_map_clip().unwrap(), "declined {args:?}");
                        let got: Vec<u32> = slots.iter().map(|&a| vm.mem.r32(a)).collect();
                        assert_eq!(got, want, "{sw}x{sh} {args:?}");
                        assert_eq!(vm.cpu.reg_get(Mode::User, 0), ret, "and what it returns");
                        assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), 0x1234, "and it returns");
                    }
                }
            }
        }
    }

    /// `Map_getTileIndex` gives the same byte and leaves the same pointer copy.
    #[test]
    fn the_guest_agrees_with_our_map_tile_index() {
        let mut vm = loaded_guest();
        let base = vm.symbol("Map_getTileIndex").expect("in .symtab");
        assert!(map_tile_site(base, &mut vm.mem));
        for (off, w) in MAP_TILE_WORDS {
            let had = vm.mem.r32(base + off);
            vm.mem.w32(base + off, w ^ 1);
            assert!(!map_tile_site(base, &mut vm.mem), "word at {off:#x}");
            vm.mem.w32(base + off, had);
        }

        let map = vm.symbol("_map").expect("in .symtab");
        let tiles = vm.heap.alloc(0x100).expect("guest heap");
        for i in 0..0x100u32 {
            vm.mem.w8(tiles + i, (i * 7 + 3) as u8);
        }
        for layer in [0u32, 1, 5] {
            let ptr = map + ((layer + MAP_TILE_LAYER) << 2);
            let copy = map + ((layer + MAP_TILE_COPY) << 2);
            for off in [0u32, 1, 0x7f, 0xff] {
                vm.mem.w32(ptr, tiles);
                vm.mem.w32(copy, 0);
                let ret = guest_leaf(&mut vm, Patch::MapTileIndex, base, &[layer, off]);
                let want = (ret, vm.mem.r32(copy));

                vm.mem.w32(ptr, tiles);
                vm.mem.w32(copy, 0);
                vm.cpu.reg_set(Mode::User, 0, layer);
                vm.cpu.reg_set(Mode::User, 1, off);
                vm.cpu.reg_set(Mode::User, reg::LR, 0x1234);
                assert!(vm.host_map_tile_index().unwrap());
                let got = (vm.cpu.reg_get(Mode::User, 0), vm.mem.r32(copy));
                assert_eq!(got, want, "layer {layer} off {off:#x}");
                assert_eq!(got.1, tiles, "and the copy is the pointer");
                assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), 0x1234);
            }
        }
    }

    /// `DM_CalcAnchorGbm` moves the origin to the same place for every anchor,
    /// every quarter turn, and negative extents too.
    ///
    /// This one is checked hardest because the host already *calls* it from two
    /// blitters, so a wrong answer here moves every rotated sprite.
    #[test]
    fn the_guest_agrees_with_our_anchor() {
        let mut vm = loaded_guest();
        let base = vm.symbol("DM_CalcAnchorGbm").expect("in .symtab");
        assert!(anchor_site(base, &mut vm.mem));
        for (off, w) in ANCHOR_WORDS {
            let had = vm.mem.r32(base + off);
            vm.mem.w32(base + off, w ^ 1);
            assert!(!anchor_site(base, &mut vm.mem), "word at {off:#x}");
            vm.mem.w32(base + off, had);
        }

        let px = vm.heap.alloc(8).expect("guest heap");
        let py = px + 4;
        let mut moved = 0;
        // 9 and 0x12 are what the shipped blitters pass; the rest walk every
        // combination of the four bits, including both set on one axis.
        for anchor in [0u32, 1, 2, 4, 6, 8, 9, 0x10, 0x12, 0x14, 0x20, 0x22, 0x24, 0x30, 0x3f] {
            for angle in [0i32, 0x5a, 0xb4, 0x10e, 0x168, -1, 0x10c] {
                for &(w, h) in &[(0i32, 0i32), (1, 1), (32, 16), (7, 25), (-8, 12), (13, -5)] {
                    for &(x, y) in &[(0i32, 0i32), (100, -40)] {
                        vm.mem.w32(px, x as u32);
                        vm.mem.w32(py, y as u32);
                        // Seven arguments; the fifth, the rotation, is never
                        // read, so a poison value proves that.
                        let args = [px, py, w as u32, h as u32, 0x5555_5555, angle as u32, anchor];
                        guest_leaf(&mut vm, Patch::CalcAnchor, base, &args);
                        let want = (vm.mem.r32(px), vm.mem.r32(py));

                        vm.mem.w32(px, x as u32);
                        vm.mem.w32(py, y as u32);
                        for (i, &v) in args[..4].iter().enumerate() {
                            vm.cpu.reg_set(Mode::User, i as u8, v);
                        }
                        // The last three are stack arguments, where `Vm::call`
                        // puts them: `[sp]`, `[sp+4]`, `[sp+8]`.
                        let sp = vm.cpu.reg_get(Mode::User, reg::SP) - 0x20;
                        vm.cpu.reg_set(Mode::User, reg::SP, sp);
                        vm.mem.w32(sp, 0x5555_5555);
                        vm.mem.w32(sp + 4, angle as u32);
                        vm.mem.w32(sp + 8, anchor);
                        vm.cpu.reg_set(Mode::User, reg::LR, 0x1234);
                        assert!(vm.host_calc_anchor().unwrap());
                        let got = (vm.mem.r32(px), vm.mem.r32(py));
                        assert_eq!(
                            got, want,
                            "anchor {anchor:#x} angle {angle:#x} {w}x{h} at ({x},{y})"
                        );
                        assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), 0x1234);
                        if got != (x as u32, y as u32) {
                            moved += 1;
                        }
                    }
                }
            }
        }
        // An anchor that never moved anything would make all of the above pass
        // for free.
        assert!(moved > 500, "only {moved} cases moved the origin");
    }

    /// `DM_RestorePalette` rebuilds the same palette and leaves the same fields.
    #[test]
    fn the_guest_agrees_with_our_palette_restore() {
        let mut vm = loaded_guest();
        let base = vm.symbol("DM_RestorePalette").expect("in .symtab");
        assert!(restore_pal_site(base, &mut vm.mem));
        for (off, w) in RESTORE_PAL_WORDS {
            let had = vm.mem.r32(base + off);
            vm.mem.w32(base + off, w ^ 1);
            assert!(!restore_pal_site(base, &mut vm.mem), "word at {off:#x}");
            vm.mem.w32(base + off, had);
        }

        let lcd = vm.symbol("_lcdG").expect("in .symtab");
        let obj = vm.heap.alloc(0x40).expect("guest heap");
        let blob = vm.heap.alloc(0x400).expect("guest heap");
        let live = vm.heap.alloc(0x200).expect("guest heap");
        let mut r = 0x1234_5678u32;
        for i in 0..0x400u32 {
            r = r.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            vm.mem.w8(blob + i, (r >> 17) as u8);
        }

        // Every gate the shipped body has, then the walk itself: no object, the
        // flag already clear, no blob, no live palette, no entries, a negative
        // count, and a full 0x100.
        let cases: [(u32, u32, u32, i32, u8); 7] = [
            (0, blob, live, 16, 1),
            (obj, blob, live, 16, 0),
            (obj, 0, live, 16, 1),
            (obj, blob, 0, 16, 1),
            (obj, blob, live, 0, 1),
            (obj, blob, live, -4, 1),
            (obj, blob, live, 0x100, 1),
        ];
        for (which, blob_p, live_p, count, flag) in cases {
            let set = |vm: &mut Vm| {
                vm.mem.write_bytes(live, &vec![0xa5u8; 0x200]);
                vm.mem.w32(obj + PAL_BLOB, blob_p);
                vm.mem.w32(obj + PAL_LIVE, live_p);
                vm.mem.w32(obj + PAL_COUNT, count as u32);
                vm.mem.w32(obj + PAL_CURSOR, 0);
                vm.mem.w32(obj + PAL_BLOB_CURSOR, 0);
                vm.mem.w8(obj + PAL_DONE, flag);
            };
            for &bpp in &[1u8, 2] {
                vm.mem.w8(lcd + LCD_BPP16, bpp);
                set(&mut vm);
                guest_leaf(&mut vm, Patch::RestorePalette, base, &[which]);
                let want = (
                    vm.mem.slice(live, 0x200).unwrap().to_vec(),
                    vm.mem.r32(obj + PAL_CURSOR),
                    vm.mem.r32(obj + PAL_BLOB_CURSOR),
                    vm.mem.r8(obj + PAL_DONE),
                );

                set(&mut vm);
                vm.cpu.reg_set(Mode::User, 0, which);
                vm.cpu.reg_set(Mode::User, reg::LR, 0x1234);
                assert!(vm.host_restore_palette().unwrap());
                let got = (
                    vm.mem.slice(live, 0x200).unwrap().to_vec(),
                    vm.mem.r32(obj + PAL_CURSOR),
                    vm.mem.r32(obj + PAL_BLOB_CURSOR),
                    vm.mem.r8(obj + PAL_DONE),
                );
                assert_eq!(got.1, want.1, "cursor: {which:#x} {count} bpp {bpp}");
                assert_eq!(got.2, want.2, "blob cursor");
                assert_eq!(got.3, want.3, "flag");
                assert_eq!(got.0, want.0, "palette: {which:#x} count {count} bpp {bpp}");
                assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), 0x1234);
            }
        }
        // And the big case really did rebuild it, or the comparison above was
        // two copies of the untouched buffer.
        vm.mem.w8(lcd + LCD_BPP16, 1);
        vm.mem.write_bytes(live, &vec![0xa5u8; 0x200]);
        vm.mem.w32(obj + PAL_BLOB, blob);
        vm.mem.w32(obj + PAL_LIVE, live);
        vm.mem.w32(obj + PAL_COUNT, 0x100);
        vm.mem.w8(obj + PAL_DONE, 1);
        vm.cpu.reg_set(Mode::User, 0, obj);
        vm.cpu.reg_set(Mode::User, reg::LR, 0x1234);
        assert!(vm.host_restore_palette().unwrap());
        let out = vm.mem.slice(live, 0x200).unwrap().to_vec();
        assert_ne!(out, vec![0xa5u8; 0x200], "nothing happened");
        // Little-endian halfwords from offset 9.
        for i in 0..0x100u32 {
            let lo = vm.mem.r8(blob + PAL_BLOB_FIRST + 2 * i) as u16;
            let hi = vm.mem.r8(blob + PAL_BLOB_FIRST + 2 * i + 1) as u16;
            assert_eq!(vm.mem.r16(live + 2 * i), lo | hi << 8, "entry {i}");
        }
    }

    /// `DM_SetChagePalBlendEx` blends every entry the way the shipped body does,
    /// for every mode it serves, both weightings, and a target of nothing.
    ///
    /// The palette is compared whole, so one wrong lane in one entry fails.
    #[test]
    fn the_guest_agrees_with_our_palette_blend() {
        let mut vm = loaded_guest();
        let base = vm.symbol("DM_SetChagePalBlendEx").expect("in .symtab");
        assert!(pal_blend_site(base, &mut vm.mem), "the shipped words are still there");
        for (off, w) in PAL_BLEND_WORDS {
            let had = vm.mem.r32(base + off);
            vm.mem.w32(base + off, w ^ 1);
            assert!(!pal_blend_site(base, &mut vm.mem), "word at {off:#x}");
            vm.mem.w32(base + off, had);
        }

        let lcd = vm.symbol("_lcdG").expect("in .symtab");
        let skip_p = vm.symbol("SKIP_COL").expect("in .symtab");
        let tbl_p = vm.symbol("BlendingTable32Bit").expect("in .symtab");
        vm.mem.w8(lcd + LCD_BPP16, 1);
        // `DM_Initialize` is what fills the blend table, and a guest that has
        // only booted has it at zeros — every mode would then agree on zero and
        // prove nothing. Filled with noise instead, which both sides read the
        // same and which exercises more of the 5-bit range than the real ramps.
        let mut t = 0x2545_f491u32;
        for i in 0..16 * 32 * 32u32 {
            t = t.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            vm.mem.w8(tbl_p + i, (t >> 20) as u8 & 0x1f);
        }

        const N: u32 = 0x100;
        let obj = vm.heap.alloc(0x40).expect("guest heap");
        let pal = vm.heap.alloc(2 * N).expect("guest heap");
        let skip = vm.mem.r32(skip_p);
        let mut r = 0x1357_9bdfu32;
        let entries: Vec<u16> = (0..N)
            .map(|i| {
                r = r.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                // One entry in sixteen is the transparent colour, which the walk
                // has to step over rather than blend.
                if i % 16 == 3 {
                    skip as u16
                } else {
                    (r >> 13) as u16
                }
            })
            .collect();

        let mut changed = 0;
        for mode in [0u32, 1, 7, 0xf, PAL_MODE_GRAY] {
            for alpha in [0u32, 1, 0x40, 0x7f, 0xf8, 0xfe, 0xff] {
                for &(cr, cg, cb) in &[(0u32, 0u32, 0u32), (0xff, 0, 0), (0x40, 0x88, 0xd0)] {
                    let args = [obj, cr, cg, cb, mode, alpha];
                    let set = |vm: &mut Vm| {
                        for (i, &e) in entries.iter().enumerate() {
                            vm.mem.w16(pal + 2 * i as u32, e);
                        }
                        vm.mem.w32(obj + PAL_LIVE, pal);
                        vm.mem.w32(obj + PAL_COUNT, N);
                        vm.mem.w32(obj + PAL_CURSOR, 0);
                        vm.mem.w8(obj + PAL_DONE, 0);
                    };
                    set(&mut vm);
                    guest_leaf(&mut vm, Patch::PalBlend, base, &args);
                    let want = (
                        vm.mem.slice(pal, 2 * N).unwrap().to_vec(),
                        vm.mem.r32(obj + PAL_CURSOR),
                        vm.mem.r8(obj + PAL_DONE),
                    );

                    set(&mut vm);
                    for (i, &v) in args[..4].iter().enumerate() {
                        vm.cpu.reg_set(Mode::User, i as u8, v);
                    }
                    let sp = vm.cpu.reg_get(Mode::User, reg::SP) - 0x20;
                    vm.cpu.reg_set(Mode::User, reg::SP, sp);
                    for (i, &v) in args[4..].iter().enumerate() {
                        vm.mem.w32(sp + 4 * i as u32, v);
                    }
                    vm.cpu.reg_set(Mode::User, reg::LR, 0x1234);
                    assert!(vm.host_pal_blend().unwrap(), "declined mode {mode:#x}");
                    let got = (
                        vm.mem.slice(pal, 2 * N).unwrap().to_vec(),
                        vm.mem.r32(obj + PAL_CURSOR),
                        vm.mem.r8(obj + PAL_DONE),
                    );
                    assert_eq!(got.1, want.1, "cursor: mode {mode:#x} alpha {alpha:#x}");
                    assert_eq!(got.2, want.2, "flag");
                    assert_eq!(
                        got.0, want.0,
                        "palette: mode {mode:#x} alpha {alpha:#x} rgb ({cr:#x},{cg:#x},{cb:#x})"
                    );
                    let raw: Vec<u8> = entries.iter().flat_map(|e| e.to_le_bytes()).collect();
                    if got.0 != raw {
                        changed += 1;
                    }
                    // The transparent entries are never touched, whatever else is.
                    for i in (3..N).step_by(16) {
                        assert_eq!(vm.mem.r16(pal + 2 * i) as u32, skip, "entry {i} was blended");
                    }
                }
            }
        }
        // A blend that left every palette alone would satisfy all of the above.
        assert!(changed > 90, "only {changed} of 105 cases changed the palette");

        // The two modes that are declined rather than guessed, so the guest runs
        // its own body: the per-entry HSB call, and anything past the table.
        for mode in [PAL_MODE_HSB, 0x11, 0xff] {
            let sp = vm.cpu.reg_get(Mode::User, reg::SP);
            vm.mem.w32(sp, mode);
            vm.mem.w32(sp + 4, 0xff);
            vm.cpu.reg_set(Mode::User, 0, obj);
            assert!(!vm.host_pal_blend().unwrap(), "mode {mode:#x} must go to the guest");
        }

        // And the early exits, which the host answers rather than declines.
        for (flag, live, count, bpp) in
            [(1u8, pal, N, 1u8), (0, 0, N, 1), (0, pal, N, 2), (0, pal, 0, 1)]
        {
            vm.mem.w8(lcd + LCD_BPP16, bpp);
            vm.mem.w8(obj + PAL_DONE, flag);
            vm.mem.w32(obj + PAL_LIVE, live);
            vm.mem.w32(obj + PAL_COUNT, count);
            vm.mem.w32(obj + PAL_CURSOR, 0);
            for (i, &e) in entries.iter().enumerate() {
                vm.mem.w16(pal + 2 * i as u32, e);
            }
            let args = [obj, 0xff, 0, 0, 0, 0xff];
            guest_leaf(&mut vm, Patch::PalBlend, base, &args);
            let want = (
                vm.mem.slice(pal, 2 * N).unwrap().to_vec(),
                vm.mem.r32(obj + PAL_CURSOR),
                vm.mem.r8(obj + PAL_DONE),
            );

            vm.mem.w8(obj + PAL_DONE, flag);
            vm.mem.w32(obj + PAL_LIVE, live);
            vm.mem.w32(obj + PAL_COUNT, count);
            vm.mem.w32(obj + PAL_CURSOR, 0);
            for (i, &e) in entries.iter().enumerate() {
                vm.mem.w16(pal + 2 * i as u32, e);
            }
            vm.cpu.reg_set(Mode::User, 0, obj);
            for (i, &v) in [0xffu32, 0, 0].iter().enumerate() {
                vm.cpu.reg_set(Mode::User, i as u8 + 1, v);
            }
            let sp = vm.cpu.reg_get(Mode::User, reg::SP) - 0x20;
            vm.cpu.reg_set(Mode::User, reg::SP, sp);
            vm.mem.w32(sp, 0);
            vm.mem.w32(sp + 4, 0xff);
            vm.cpu.reg_set(Mode::User, reg::LR, 0x1234);
            assert!(vm.host_pal_blend().unwrap());
            let got = (
                vm.mem.slice(pal, 2 * N).unwrap().to_vec(),
                vm.mem.r32(obj + PAL_CURSOR),
                vm.mem.r8(obj + PAL_DONE),
            );
            assert_eq!(got.1, want.1, "cursor: flag {flag} live {live:#x} n {count} bpp {bpp}");
            assert_eq!(got.2, want.2, "flag: flag {flag} live {live:#x} n {count} bpp {bpp}");
            assert_eq!(got.0, want.0, "palette: flag {flag} live {live:#x} n {count} bpp {bpp}");
        }
    }

    /// `MC_grpPutPixel` writes the same pixel the shipped body does, or leaves
    /// the surface alone in exactly the same cases.
    ///
    /// The surface is compared whole, so a pixel written one place too far is a
    /// failure rather than a near miss.
    #[test]
    fn the_guest_agrees_with_our_put_pixel() {
        let mut vm = loaded_guest();
        let base = vm.symbol("MC_grpPutPixel").expect("in .symtab");
        assert!(put_pixel_site(base, &mut vm.mem), "the shipped words are still there");
        for (off, w) in PUT_PIXEL_WORDS {
            let had = vm.mem.r32(base + off);
            vm.mem.w32(base + off, w ^ 1);
            assert!(!put_pixel_site(base, &mut vm.mem), "word at {off:#x}");
            vm.mem.w32(base + off, had);
        }
        // And the lerp it tail-calls, which the site check follows by target.
        let op = vm.symbol("_Z25_midas_funcGrpPixelOpProciiii").expect("in .symtab");
        for (off, w) in PIXEL_OP_WORDS {
            let had = vm.mem.r32(op + off);
            vm.mem.w32(op + off, w ^ 1);
            assert!(!put_pixel_site(base, &mut vm.mem), "lerp word at {off:#x}");
            vm.mem.w32(op + off, had);
        }
        let had = vm.mem.r32(base + PUT_PIXEL_OP_CALL);
        vm.mem.w32(base + PUT_PIXEL_OP_CALL, had.wrapping_add(1));
        assert!(!put_pixel_site(base, &mut vm.mem), "and the call has to reach it");
        vm.mem.w32(base + PUT_PIXEL_OP_CALL, had);

        const W: i32 = 8;
        const H: i32 = 6;
        let bytes = (2 * W * H) as u32;
        let px = vm.heap.alloc(bytes).expect("guest heap");
        let gbm = vm.heap.alloc(0x20).expect("guest heap");
        let ctx = vm.heap.alloc(0x40).expect("guest heap");
        vm.mem.w32(gbm + GBM_W, W as u32);
        vm.mem.w32(gbm + GBM_H, H as u32);
        vm.mem.w32(gbm + GBM_PITCH, (2 * W) as u32);
        vm.mem.w32(gbm + GBM_PIXELS, px);

        // Off each edge, the two corners, the middle, and one past the far edge
        // on both axes.
        let spots = [(-1i32, 0i32), (0, -1), (0, 0), (3, 2), (W - 1, H - 1), (W, 0), (0, H)];
        // Something with structure in both bytes, so a pixel written one place
        // too far shows up rather than landing on a neighbour that matches.
        let seed: Vec<u8> =
            (0..bytes).map(|i| (i as u8).wrapping_mul(37).wrapping_add(11)).collect();
        let mut wrote = 0;
        for bpp in [0x10u32, 0, 8] {
            vm.mem.w32(gbm + GBM_BPP, bpp);
            // Clips that keep everything, cut a corner off, and reject
            // everything — including an inverted rectangle.
            for clip in [[0i32, 0, W, H], [2, 1, 6, 5], [0, 0, 0, 0], [5, 5, 1, 1]] {
                for (i, &v) in clip.iter().enumerate() {
                    vm.mem.w32(ctx + CTX_CLIP[i], v as u32);
                }
                for alpha in [0u32, 1, 0x40, 0xfe, 0xff, 0x100] {
                    vm.mem.w32(ctx + CTX_ALPHA, alpha);
                    for colour in [0u32, 0xffff, 0x8410, 0x1_f81f] {
                        vm.mem.w32(ctx + CTX_COLOUR, colour);
                        for &(x, y) in &spots {
                            let args = [gbm, x as u32, y as u32, ctx];
                            vm.mem.write_bytes(px, &seed);
                            guest_leaf(&mut vm, Patch::PutPixel, base, &args);
                            let want = vm.mem.slice(px, bytes).unwrap().to_vec();

                            vm.mem.write_bytes(px, &seed);
                            for (i, &v) in args.iter().enumerate() {
                                vm.cpu.reg_set(Mode::User, i as u8, v);
                            }
                            vm.cpu.reg_set(Mode::User, reg::LR, 0x1234);
                            assert!(
                                vm.host_put_pixel().unwrap(),
                                "declined bpp {bpp:#x} alpha {alpha:#x} at ({x},{y})"
                            );
                            let got = vm.mem.slice(px, bytes).unwrap().to_vec();
                            assert_eq!(
                                got, want,
                                "bpp {bpp:#x} clip {clip:?} alpha {alpha:#x} colour {colour:#x} at ({x},{y})"
                            );
                            assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), 0x1234);
                            if got != seed {
                                wrote += 1;
                            }
                        }
                    }
                }
            }
        }
        // A port that never wrote a pixel would pass everything above. Four of
        // the seven positions land inside one of the two clips that keep
        // anything, times the six alphas and four colours, on the one bpp that
        // draws — so ninety-six, and anything much under that is a port that
        // stopped drawing.
        assert!(wrote > 80, "only {wrote} cases wrote a pixel");

        // The two the shipped body would have this guessing: a 32bpp surface,
        // and a context carrying its own per-pixel hook.
        vm.mem.w32(gbm + GBM_BPP, 0x20);
        assert!(!vm.host_put_pixel().unwrap(), "32bpp goes to the guest");
        vm.mem.w32(gbm + GBM_BPP, 0x10);
        for (i, &v) in [gbm, 3u32, 2, ctx].iter().enumerate() {
            vm.cpu.reg_set(Mode::User, i as u8, v);
        }
        for i in 0..4 {
            vm.mem.w32(ctx + CTX_CLIP[i], if i < 2 { 0 } else { 8 });
        }
        vm.mem.w32(ctx + CTX_HOOK, 0x1000);
        assert!(!vm.host_put_pixel().unwrap(), "a per-pixel hook goes to the guest");
        vm.mem.w32(ctx + CTX_HOOK, 0);
        assert!(vm.host_put_pixel().unwrap());
    }

    /// `_midas_funcFntInvalidate` seeds the same four bases and the same scratch.
    #[test]
    fn the_guest_agrees_with_our_fnt_invalidate() {
        let mut vm = loaded_guest();
        let base = vm.symbol("_midas_funcFntInvalidate").expect("in .symtab");
        assert!(fnt_invalidate_site(base, &mut vm.mem));
        for (off, w) in FNT_INVALIDATE_WORDS {
            let had = vm.mem.r32(base + off);
            vm.mem.w32(base + off, w ^ 1);
            assert!(!fnt_invalidate_site(base, &mut vm.mem), "word at {off:#x}");
            vm.mem.w32(base + off, had);
        }

        let g = vm.font_globals.expect("the font globals resolve");
        let font = vm.heap.alloc(0x40).expect("guest heap");
        let blob = vm.heap.alloc(0x40).expect("guest heap");
        let read = |vm: &mut Vm| {
            (
                vm.mem.r32(g.r0),
                (0..4).map(|i| vm.mem.r32(g.internal_font + 4 * i)).collect::<Vec<u32>>(),
            )
        };
        for (cho, jung) in [(0u8, 0u8), (19, 21), (0xff, 0xff)] {
            vm.mem.w8(blob + 6, cho);
            vm.mem.w8(blob + 7, jung);
            for comp in [0u32, 2, 4] {
                for ascii in [0u32, 0x1234] {
                    for f in [0u32, font] {
                        vm.mem.w32(font + FNT_BLOB, blob);
                        vm.mem.w32(font + FNT_COMP_BYTES, comp);
                        vm.mem.w32(font + FNT_ASCII_OFF, ascii);
                        vm.mem.w32(g.r0, 0xdead);
                        for i in 0..4 {
                            vm.mem.w32(g.internal_font + 4 * i, 0xbeef);
                        }
                        let ret = guest_leaf(&mut vm, Patch::FntInvalidate, base, &[f]);
                        let want = (read(&mut vm), ret);

                        vm.mem.w32(g.r0, 0xdead);
                        for i in 0..4 {
                            vm.mem.w32(g.internal_font + 4 * i, 0xbeef);
                        }
                        vm.cpu.reg_set(Mode::User, 0, f);
                        vm.cpu.reg_set(Mode::User, reg::LR, 0x1234);
                        assert!(vm.host_fnt_invalidate().unwrap());
                        let got = (read(&mut vm), vm.cpu.reg_get(Mode::User, 0));
                        assert_eq!(
                            got, want,
                            "cho {cho} jung {jung} comp {comp} ascii {ascii:#x} font {f:#x}"
                        );
                        assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), 0x1234);
                    }
                }
            }
        }
        // A null font writes nothing, which is the one case where "the same" is
        // also "untouched" — and the loop above ends on a real font, so this
        // asks again rather than reading what that left.
        vm.mem.w32(g.r0, 0xdead);
        for i in 0..4 {
            vm.mem.w32(g.internal_font + 4 * i, 0xbeef);
        }
        vm.cpu.reg_set(Mode::User, 0, 0);
        vm.cpu.reg_set(Mode::User, reg::LR, 0x1234);
        assert!(vm.host_fnt_invalidate().unwrap());
        assert_eq!(vm.cpu.reg_get(Mode::User, 0), 0, "and it answers 0");
        assert_eq!(vm.mem.r32(g.r0), 0xdead, "a null font left the scratch alone");
        assert_eq!(vm.mem.r32(g.internal_font), 0xbeef, "and the bases");
    }
    #[test]
    fn the_equipment_grid_is_where_the_look_cells_stand_beside_it() {
        let mut vm = loaded_guest();
        let draw = vm.symbol("IGM_DrawEquip").expect("in .symtab");
        let round = vm.symbol("UI_RoundRect").expect("in .symtab");
        let icon = vm.symbol("drawEqip_ItemIcon").expect("in .symtab");
        assert!(equip_grid_site(draw, round, icon, &mut vm.mem));

        // Change any one of the words and nothing is installed — a build whose
        // tab has moved must draw no cells rather than five in the wrong place.
        for (off, w) in EQUIP_GRID_WORDS {
            let had = vm.mem.r32(draw + off);
            vm.mem.w32(draw + off, w ^ 1);
            assert!(!equip_grid_site(draw, round, icon, &mut vm.mem), "word at {off:#x}");
            vm.mem.w32(draw + off, had);
        }
        // And the same for the two calls, which a word alone cannot vouch for.
        for off in [EQUIP_ROUND_RECT, EQUIP_ITEM_ICON] {
            let had = vm.mem.r32(draw + off);
            vm.mem.w32(draw + off, had.wrapping_add(1));
            assert!(!equip_grid_site(draw, round, icon, &mut vm.mem), "call at {off:#x}");
            vm.mem.w32(draw + off, had);
        }
        assert!(equip_grid_site(draw, round, icon, &mut vm.mem));
    }

    /// Each look cell sits two columns outward from the gear cell it belongs
    /// to, and the five slots are the ones a look can reach.
    ///
    /// The columns are not written down twice: they are derived here from the
    /// same loop constants the drawing uses, so a change to one is a failure
    /// rather than a quiet drift.

    /// The four arrow arms are still the 4x2 walk the look cells replace, the
    /// tap is still in the registers they are tested against, and the OK arm
    /// still decides an empty slot the way it did.
    ///
    /// Nothing in the image is rewritten for this feature any more, so the
    /// assertion runs the other way from the usual one: the shipped words are
    /// still there, and the hooks sit on them.
    #[test]
    fn the_equipment_cursor_is_the_one_the_look_cells_extend() {
        let mut vm = loaded_guest();
        let key = vm.symbol("GameMenuFrame_KeyPressed").expect("in .symtab");
        assert!(equip_cursor_site(key, &mut vm.mem), "the shipped words are still there");

        // Every one of them. An arrow whose step is neither the guest's nor
        // ours would leave the cursor where it was, and a cursor that can reach
        // a value nothing answers for is worse than no look cells at all.
        let words: Vec<u32> = EQ_STEP_WORDS
            .iter()
            .chain(EQ_TOUCH_WORDS.iter())
            .chain(EQ_OK_EMPTY_WORDS.iter())
            .map(|&(o, _)| o)
            .collect();
        for off in words {
            let had = vm.mem.r32(key + off);
            vm.mem.w32(key + off, had ^ 1);
            assert!(!equip_cursor_site(key, &mut vm.mem), "word at {off:#x}");
            vm.mem.w32(key + off, had);
        }
        assert!(equip_cursor_site(key, &mut vm.mem));

        // Each arm's two anchors are among the words that were checked, so a
        // hook can neither sit on nor return to an instruction nothing vouched
        // for. And the hook always sits on a `ldrb` of the cursor.
        for (at, resume, along, between) in EQ_STEP_SITES {
            for off in [at, resume] {
                assert!(EQ_STEP_WORDS.iter().any(|&(o, _)| o == off), "{off:#x} is checked");
            }
            let w = EQ_STEP_WORDS.iter().find(|&&(o, _)| o == at).unwrap().1;
            assert_eq!(w & 0xfff0_0fff, 0xe5d0_0000, "ldrb rX,[rY] at {at:#x}");
            assert!(resume > at, "the arm goes on after its own step");
            assert_eq!(along.abs() + between.abs(), 1, "one axis, one cell");
        }
    }

    /// The arrows walk the cells that are drawn, and every cell is reachable.
    ///
    /// The eight gear cells keep the shipped +-4 between the rows — that is the
    /// part of this that is not a change — while along a row the order is the
    /// screen's: 무기외형 투구외형 무기 투구 갑옷 방패 갑옷외형 방패외형.
    #[test]
    fn the_arrows_walk_the_cells_the_way_they_are_drawn() {
        let cells = equip_cells();
        assert_eq!(cells.len(), EQUIP_SLOTS as usize + SKIN_CELLS.len(), "thirteen");

        let row = |r: i32| {
            let mut v: Vec<(i32, i32)> =
                cells.iter().filter(|&&(_, _, rr)| rr == r).map(|&(c, x, _)| (x, c)).collect();
            v.sort_unstable();
            v.into_iter().map(|(_, c)| c).collect::<Vec<i32>>()
        };
        assert_eq!(row(0), vec![8, 9, 0, 1, 2, 3, 11, 12], "무기외형 to 방패외형");
        assert_eq!(row(1), vec![10, 4, 5, 6, 7], "신발외형 to 장신구");

        // Left and right walk that order and wrap inside the row.
        for r in [0, 1] {
            let across = row(r);
            for (i, &cur) in across.iter().enumerate() {
                let next = across[(i + 1) % across.len()];
                let prev = across[(i + across.len() - 1) % across.len()];
                assert_eq!(equip_step(&cells, cur, 1, 0), Some(next), "right from {cur}");
                assert_eq!(equip_step(&cells, cur, -1, 0), Some(prev), "left from {cur}");
            }
        }
        // The two the player reported, by name.
        assert_eq!(equip_step(&cells, 9, 1, 0), Some(0), "투구외형 -> 무기 장착");
        assert_eq!(equip_step(&cells, 12, 1, 0), Some(8), "방패외형 -> 무기외형");

        // Up and down over the gear cells are the shipped +-4, and off the ends
        // they are the shipped nothing.
        for cell in 0..4 {
            assert_eq!(equip_step(&cells, cell, 0, 1), Some(cell + 4), "down {cell}");
            assert_eq!(equip_step(&cells, cell + 4, 0, -1), Some(cell), "up {cell}");
            assert_eq!(equip_step(&cells, cell, 0, -1), None, "up off the top");
            assert_eq!(equip_step(&cells, cell + 4, 0, 1), None, "down off the bottom");
        }

        // A look cell with no cell under it lands on the nearest one, so the
        // whole grid is reachable by the arrows alone — and comes back.
        assert_eq!(equip_step(&cells, 8, 0, 1), Some(10), "무기외형 down to 신발외형");
        assert_eq!(equip_step(&cells, 10, 0, -1), Some(9), "신발외형 up to 투구외형");
        assert_eq!(equip_step(&cells, 11, 0, 1), Some(7), "갑옷외형 down to 장신구");
        assert_eq!(equip_step(&cells, 12, 0, 1), Some(7), "방패외형 down to 장신구");
        for &(cur, ..) in cells.iter() {
            let reached: Vec<i32> = [(1, 0), (-1, 0), (0, 1), (0, -1)]
                .into_iter()
                .filter_map(|(a, b)| equip_step(&cells, cur, a, b))
                .collect();
            assert!(!reached.is_empty(), "{cur} is a dead end");
            assert!(reached.iter().all(|&c| c != cur), "{cur} steps onto itself");
        }

        // A value no cell owns is nobody's: the handler declines there and the
        // shipped arithmetic brings the cursor home.
        for cur in [-1, 13, 100] {
            assert!(!cells.iter().any(|&(c, _, _)| c == cur), "{cur} is not a cell");
            assert_eq!(equip_step(&cells, cur, 1, 0), None);
        }
    }

    /// A look cell is a cursor value the equipment path answers nothing for.
    ///
    /// That is the whole of its safety: `getEqipNum` gives no slot, so
    /// `MP_getEquip` gives no item, so every arm that would change what the
    /// player is wearing takes the branch it already has for an empty hand.
    #[test]
    fn a_look_cell_has_no_slot_and_nothing_worn() {
        let mut vm = loaded_guest();
        for cell in 0..SKIN_CURSOR as u32 {
            vm.cpu.reg_set(Mode::User, 0, cell);
            assert!(!vm.host_equip_slot_num().unwrap(), "gear cell {cell} is the guest's");
        }
        for i in 0..SKIN_CELLS.len() as u32 {
            vm.cpu.reg_set(Mode::User, 0, SKIN_CURSOR as u32 + i);
            vm.cpu.reg_set(Mode::User, reg::LR, 0x1234_5678);
            assert!(vm.host_equip_slot_num().unwrap());
            assert_eq!(vm.cpu.reg_get(Mode::User, 0) as i32, -1, "no slot");
            assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), 0x1234_5678, "and it returns");
        }

        for slot in 0..EQUIP_SLOTS as u32 {
            vm.cpu.reg_set(Mode::User, 1, slot);
            assert!(!vm.host_equip_worn().unwrap(), "slot {slot} is the guest's");
        }
        for slot in [-1i32, EQUIP_SLOTS, 100] {
            vm.cpu.reg_set(Mode::User, 1, slot as u32);
            vm.cpu.reg_set(Mode::User, 0, 0xdead_beef);
            vm.cpu.reg_set(Mode::User, reg::LR, 0x8765_4321);
            assert!(vm.host_equip_worn().unwrap(), "slot {slot} is not one");
            assert_eq!(vm.cpu.reg_get(Mode::User, 0), 0, "nothing worn");
            assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), 0x8765_4321);
        }

        // The shipped `MP_getEquip` is `hero + slot*4` and reads whatever is
        // there, which is why the guard exists rather than a comment.
        let f = vm.symbol("MP_getEquip").expect("in .symtab");
        assert_eq!(vm.mem.r32(f + 0x0c), 0xe080_0104, "add r0,r0,r4,lsl #2");
        assert_eq!(vm.mem.r32(f + 0x10), 0xe590_065c, "ldr r0,[r0,#0x65c]");
    }

    #[test]
    fn the_look_cells_sit_two_columns_outside_their_gear_cells() {
        // The gear columns, rebuilt the way the loop builds them: `r5` steps
        // by `EQ_COL` every pass and `r8` is 0 on the first two, `0x1a` after.
        let gear: Vec<i32> =
            (0..4).map(|i| i * EQ_COL + if i < 2 { 0 } else { 0x1a }).collect();
        assert_eq!(gear, EQ_COLS, "the four shipped columns");

        // Cell -> slot, and slot -> the gear cell that holds it.
        let cell_of = |slot: u32| EQ_SLOTS.iter().position(|&s| s == slot).expect("a cell");
        for &(slot, dx, row) in SKIN_CELLS.iter() {
            let cell = cell_of(slot);
            assert_eq!(cell / 4, row as usize, "slot {slot} is on row {row}");
            let gear_dx = EQ_COLS[cell % 4];
            let outward = if gear_dx <= EQ_COLS[1] { -2 * EQ_COL } else { 2 * EQ_COL };
            assert_eq!(dx, gear_dx + outward, "slot {slot} is two columns out");
        }

        // 무기·투구·신발 on the left, 갑옷·방패 on the right, and nothing else:
        // the look only reaches the five slots that have a sprite layer.
        let slots: Vec<u32> = SKIN_CELLS.iter().map(|&(s, _, _)| s).collect();
        assert_eq!(slots, vec![0, 2, 4, 3, 1]);
        let left: Vec<u32> =
            SKIN_CELLS.iter().filter(|&&(_, dx, _)| dx < 0).map(|&(s, _, _)| s).collect();
        assert_eq!(left, vec![0, 2, 4], "the left ㄱ is 무기 투구 신발");

        // On a 400x240 screen every cell is on it, and no two overlap —
        // including the gear's own eight.
        let mut rects: Vec<(i32, i32)> = SKIN_CELLS.iter().map(|&(_, dx, r)| (dx, r)).collect();
        for (i, &gx) in EQ_COLS.iter().enumerate() {
            let _ = i;
            rects.push((gx, 0));
            rects.push((gx, 1));
        }
        for &(dx, row) in &rects {
            let x = 200 + EQ_X0 + dx;
            let y = 120 + EQ_Y0 + row * EQ_ROW;
            assert!(x >= 0 && x + EQ_W <= 400, "x {x}");
            assert!(y >= 0 && y + EQ_H <= 240, "y {y}");
        }
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                let apart = (a.0 - b.0).abs() >= EQ_W || (a.1 - b.1).abs() >= 1;
                assert!(apart, "{a:?} and {b:?} overlap");
            }
        }
    }

    /// The look recovered from a template is the one a live item of that kind
    /// and index carries, and the record's key survives a save.

    /// A look belongs to one save slot.
    ///
    /// One table serves the whole install and its key is the item's creation
    /// stamp, which the game does not keep unique — so without the slot in
    /// the key, a look applied on one character could appear on another's
    /// gear. Measured with the record's slot byte as the only difference.
    #[test]
    fn a_look_record_belongs_to_one_save_slot() {
        let mut vm = loaded_guest();
        let tab = vm.symbol("skin_tab").expect("the cave is installed");
        let slot_p = vm.symbol("_selectSlotNumber").expect("in .symtab");
        // The cave fills the table off the save container on first use; this
        // one is ours, so tell it that has happened.
        vm.mem.w8(vm.symbol("skin_loaded").expect("the cave is installed"), 1);

        // As far as the lookup is concerned an item is a stamp.
        let item = vm.heap.alloc(0x100).expect("scratch");
        vm.mem.w32(item + ITEM_STAMP, 0x4123_4567);
        vm.mem.w32(item + ITEM_STAMP + 4, 0x0000_018b);
        // One record, owned by save slot 1 (`_selectSlotNumber + 1`).
        vm.mem.w32(tab, 0x4123_4567);
        vm.mem.w32(tab + 4, 0x0000_018b);
        vm.mem.w8(tab + SKIN_REC_SPRITE, 7);
        vm.mem.w8(tab + SKIN_REC_SLOT, 2);
        vm.mem.w16(tab + SKIN_REC_PAL, 99);
        vm.mem.w32(tab + SKIN_REC_MAGIC, SKIN_MAGIC);

        vm.mem.w8(slot_p, 1);
        assert_eq!(vm.skin_look(item).unwrap(), Some((7, 99)), "its own slot sees it");
        vm.mem.w8(slot_p, 2);
        assert_eq!(vm.skin_look(item).unwrap(), None, "another save does not");
        // A record written before the slot was part of the key is any slot's,
        // because dropping those would take away looks a player has applied.
        vm.mem.w8(tab + SKIN_REC_SLOT, 0);
        assert_eq!(vm.skin_look(item).unwrap(), Some((7, 99)));

        // And the key survives what the loader does to the high word.
        vm.mem.w8(tab + SKIN_REC_SLOT, 3);
        vm.mem.w32(item + ITEM_STAMP, 0xc123_4567);
        vm.mem.w32(tab, 0xc123_4567);
        vm.mem.w32(item + ITEM_STAMP + 4, u32::MAX); // as it comes back off a save
        assert_eq!(vm.skin_look(item).unwrap(), Some((7, 99)), "0x18b and -1 are one key");
        // The magic is still what says a record is in use.
        vm.mem.w32(tab + SKIN_REC_MAGIC, 0);
        assert_eq!(vm.skin_look(item).unwrap(), None);
    }

    #[test]
    fn a_look_is_recovered_from_the_game_s_own_item_templates() {
        let vfs = std::fs::read("../payload/data.vfs.mp3").expect("payload/ is committed");
        let index = crate::files::VfsIndex::build(&vfs, 0, vfs.len() as u64);
        // Kind 12 index 0 is the knight's starting weapon. Its template's
        // sprite and palette are the two fields the look record keeps, so
        // looking that pair up in its own group must name kind 12 again — and
        // not by luck: the other two kinds in the group are searched first.
        // Index 3 of kind 11 is one of its own: no other kind in the group
        // carries that look, so the lookup has to cross a kind to find it.
        let other = crate::netitem::template_look(&vfs, &index, 11, 3).expect("kind 11 index 3");
        assert_eq!(
            crate::netitem::kind_with_look(&vfs, &index, &skin_donor_kinds(12), other.0, other.1),
            Some((11, 3)),
            "a look from a sibling kind is found there, index and all"
        );
        // Index 0 of all three is the same class-neutral piece, so that look
        // names no kind on its own. Searching the worn kind first makes it
        // answer with the piece's own kind rather than the group's lowest.
        let shared =
            crate::netitem::template_look(&vfs, &index, 12, 0).expect("the archive has kind 12");
        assert_eq!(crate::netitem::template_look(&vfs, &index, 10, 0), Some(shared));
        assert_eq!(
            crate::netitem::kind_with_look(&vfs, &index, &skin_donor_kinds(12), shared.0, shared.1),
            Some((12, 0))
        );
        let look = shared;
        // A look no template carries is not guessed at.
        assert_eq!(
            crate::netitem::kind_with_look(&vfs, &index, &skin_donor_kinds(12), 0xff, 0x7fff),
            None
        );
        // And the lookup really reaches across a group: kind 0 index 4's look
        // is the one a kind-0 weapon took in the probe run this was written
        // from, and it is found in kind 0 and nowhere in the armour group.
        let other = crate::netitem::template_look(&vfs, &index, 0, 4).expect("kind 0 index 4");
        assert_eq!(crate::netitem::kind_with_look(&vfs, &index, &[0], other.0, other.1), Some((0, 4)));
        assert_eq!(
            crate::netitem::kind_with_look(&vfs, &index, &skin_donor_kinds(12), other.0, other.1),
            None,
            "a weapon's look is not in the armour group"
        );
        // The groups are the transmog's own rule, read back: armour in threes
        // by body part, weapons and shields only their own kind.
        assert_eq!(skin_donor_kinds(12), vec![12, 10, 11]);
        assert_eq!(skin_donor_kinds(7), vec![7, 8, 9]);
        assert_eq!(skin_donor_kinds(3), vec![3]);
        assert!(skin_donor_kinds(0x13).is_empty(), "0x13 up is not equipment");

        // `UM_ByteToInt64` ORs the low word's sign into the high word, so a
        // key is only stable once it has been through that. Idempotent, which
        // is what lets a record written either side of a reload be found.
        assert_eq!(skin_stamp_norm(0xcfe6_3cd3, 0x0000_018b), u32::MAX);
        assert_eq!(skin_stamp_norm(0xcfe6_3cd3, u32::MAX), u32::MAX);
        assert_eq!(skin_stamp_norm(0x4fe6_3cd3, 0x0000_018b), 0x0000_018b);
    }

    /// That normalisation is the guest's, not ours.
    ///
    /// `Vm::call` restores the register file, so the second half of a 64-bit
    /// return cannot be read back through it. The claim is pinned where it is
    /// made instead: the instruction that ORs the low word's sign into the
    /// high one, and the `strd` that hands both back.
    #[test]
    fn the_guest_loses_the_high_word_the_way_the_look_key_assumes() {
        let mut vm = loaded_guest();
        let f = vm.symbol("UM_ByteToInt64").expect("in .symtab");
        // 0x000eefa0  orr sb, sb, r3, asr #31   — sb is the high word being
        // assembled, r3 the low one. This is the whole defect.
        assert_eq!(vm.mem.r32(f + 0x7c), 0xe189_9fc3, "the sign is ORed in");
        // 0x000eefa4/a8  mov r1, sb / mov r0, r8 — and that is what comes back.
        assert_eq!(vm.mem.r32(f + 0x80), 0xe1a0_1009);
        assert_eq!(vm.mem.r32(f + 0x84), 0xe1a0_0008);

        // What that arithmetic does to a stamp, which is what the key is.
        assert_eq!(skin_stamp_norm(0xcfe6_3cd3, 0x0000_018b), u32::MAX);
        assert_eq!(skin_stamp_norm(0x4fe6_3cd3, 0x0000_018b), 0x0000_018b);
    }

    #[test]
    fn the_margins_clear_the_one_button_the_cluster_can_reach() {
        let mut vm = loaded_guest();
        let base = vm.symbol("Compose_RecipeKey").expect("in .symtab");
        for (off, want) in COMPOSE_BTN_WORDS {
            assert_eq!(vm.mem.r32(base + off), want, "the compose button moved at +{off:#x}");
        }
        let check = vm.symbol("Check_Rect").expect("in .symtab");
        assert_eq!(
            branch_target(base + COMPOSE_BTN, &mut vm.mem, true),
            Some(check),
            "+{COMPOSE_BTN:#x} is not the button's Check_Rect"
        );

        // Both rectangles are anchored to the bottom edge, so the screen
        // height cancels and 240 stands for every screen this runs on.
        let h = 240i32;
        let (s, dy) = (dpad_scale(), dpad_dy());
        let (bx, by, bw, bh) = COMPOSE_BTN_RECT;
        let btn = (bx, h - by, bx + bw, h - by + bh);
        // Every plate, where it actually lands, on every screen width the
        // clamp can hand out a different margin for — 4:3, 16:10, the probe's
        // 5:3, 16:9, and the two the device uses.
        for w in [320, 384, 400, 427, 520, 533] {
        let dx = dpad_dx_for(w, s);
        // Every plate, where it actually lands. The down one is the only one
        // that can reach, but asserting all four is what keeps that true.
        for dir in MENU_ARROW_DIRS {
            let (x0, y0, pw, ph) = if dpad_hex() {
                dpad_plate(dir, h, s, dx, dy)
            } else {
                let i = MENU_ARROW_DIRS.iter().position(|&d| d == dir).unwrap();
                let (rx, ry, rw, rh) = MENU_ARROW_RECTS[i];
                (dpad_x(rx, s, dx), dpad_y(h - ry, h, s, dy), rw * s / 100, rh * s / 100)
            };
            let plate = (x0, y0, x0 + pw, y0 + ph);
            let hit = plate.0 < btn.2 && btn.0 < plate.2 && plate.1 < btn.3 && btn.1 < plate.3;
            assert!(
                !hit,
                "the {dir:?} plate {plate:?} would eat the compose button {btn:?}                  at scale {s}%, dx {dx}, dy {dy} on a {w}px screen"
            );
        }
        // And the cluster clears the 3x3 icon menu's left column, which answers
        // before it and is the reason `dx` is clamped at all. At 125 % the
        // cluster is 102 px wide where the shipped one was 91, so on a 4:3
        // screen no margin clears it — there the clamp just bottoms out on the
        // shipped margin and the overlap is the scale's, not the margin's.
        let (rx, _, rw, _) = dpad_plate(Dir::Right, h, s, dx, dy);
        assert_eq!(
            rx + rw < w / 2 - MENU_GRID_LEFT,
            w >= 384,
            "on a {w}px screen the right plate ends at {} and the menu answers from {}",
            rx + rw,
            w / 2 - MENU_GRID_LEFT
        );
        }
        // And the shipped cluster does not reach it either, which is what makes
        // the line above a statement about our margins and not about the game.
        let (rx, ry, rw, rh) = MENU_ARROW_RECTS[1];
        let x0 = dpad_x(rx, 100, DPAD_LEFT);
        assert!(x0 + rw <= COMPOSE_BTN_RECT.0, "the shipped arrow already overlapped");
    }

    /// the function, the four addresses really are `bl Check_Rect` set up with
    /// the D-pad rects, and the window `Vm::boot` builds really covers them.
    /// The chest fix stands on four words of `Hero_canAttack`, and on the
    /// meaning of two monster fields. Pin all of it: if the prologue moves,
    /// `r5` is not the monster and the hook would read a random address.
    #[test]
    fn the_chest_hook_sits_on_the_hp_refusal_and_nothing_else() {
        let mut vm = loaded_guest();
        let can = vm.symbol("Hero_canAttack").expect("named in .symtab");

        // Installs where the shipped compare is, and only there.
        let site = chest_break_site(can, &mut vm.mem).expect("site intact");
        assert_eq!(site, can + CHEST_HP_CMP);

        // Change any one of the four and it refuses to install — a moved
        // prologue means `r5` is no longer the monster.
        for &(off, word) in &CHEST_SITE {
            let at = can.wrapping_add(off);
            vm.mem.w32(at, word ^ 1);
            assert!(
                chest_break_site(can, &mut vm.mem).is_none(),
                "a changed word at +{off:#x} still installed"
            );
            vm.mem.w32(at, word);
        }
        assert!(chest_break_site(can, &mut vm.mem).is_some());

        // The two fields the hook reads are the ones it thinks they are:
        // `MONSTER_getType` is `m[0xc6]` and objects are type 5,
        // `CHAR_getVisible` is `m[0x20]`.
        let ty = vm.symbol("MONSTER_getType").expect("named in .symtab");
        assert_eq!(vm.mem.r32(ty + 4), 0x15d0_00c6, "type is not m[0xc6]"); // ldrbne
        let obj = vm.symbol("MONSTER_isObject").expect("named in .symtab");
        assert_eq!(vm.mem.r32(obj + 0xc), 0xe350_0405, "objects are not type 5");
        let vis = vm.symbol("OBJ_getVisible").expect("named in .symtab");
        assert_eq!(vm.mem.r32(vis), 0xe5d0_0020, "visible is not m[0x20]");

        // And the refusal really is about HP, not about being dead:
        // `MONSTER_isDie` reads the state byte, so a chest at state 0 passes
        // it untouched once the HP compare lets it through.
        let die = vm.symbol("MONSTER_isDie").expect("named in .symtab");
        assert_eq!(
            branch_target(die + 8, &mut vm.mem, true),
            vm.symbol("CHAR_getMainState"),
            "MONSTER_isDie no longer asks for the state"
        );
    }

    #[test]
    fn the_cube_compose_popup_arrows_are_inside_their_handler_window() {
        let mut vm = loaded_guest();
        let key = vm.symbol("GameMenuFrame_KeyPressed").expect("named in .symtab");
        let size = *vm.elf.sizes.get("GameMenuFrame_KeyPressed").expect("sized in .symtab");
        assert_eq!(size, 0xa730, "the handler was resized");

        // The trap itself: a symbol that is not a function, inside one.
        let stack = vm.symbol("_stack").expect("the linker defines it");
        assert!(key < stack && stack < key + size, "_stack stopped splitting the handler");

        // `mov r2, #rx` / `sub r3, r3, #ry` / `mov ip, #w` — the quadruple,
        // and `Check_Rect` is what each of them calls.
        let check_rect = vm.symbol("Check_Rect").expect("named in .symtab");
        for (off, rx, ry, w) in [
            (0xa630u32, 0xe3a0_201au32, 0xe243_3053u32, 0xe3a0_c027u32), // up
            (0xa668, 0xe3a0_201a, 0xe243_3017, 0xe3a0_c027),             // down
            (0xa69c, 0, 0xe243_303c, 0xe3a0_c01a),                       // left, rx from flags
            (0xa6d4, 0xe3a0_2041, 0xe243_303c, 0xe3a0_c01a),             // right
        ] {
            let at = key + off;
            let bl = vm.mem.r32(at);
            assert_eq!(bl >> 24, 0xeb, "{off:#x} is not a bl");
            let target = at.wrapping_add(8).wrapping_add((((bl << 8) as i32) >> 6) as u32);
            assert_eq!(target, check_rect, "{off:#x} no longer calls Check_Rect");
            // The setup is nine or ten instructions and the two shapes order
            // them differently, so look for the words rather than pin offsets.
            let mut has = |word: u32| (at - 0x28..at).step_by(4).any(|a| vm.mem.r32(a) == word);
            assert!(has(ry), "{off:#x} ry drifted");
            assert!(has(w), "{off:#x} width drifted");
            // The left box takes rx from the flags of the compare above it
            // (`subs r2, r0, #0` then `beq`), so it has no `mov r2, #0`.
            assert!(rx == 0 || has(rx), "{off:#x} rx drifted");
            // What the bug was: the window has to reach the *return* address,
            // which is the word after the `bl`.
            assert!(
                menu_arrow_key_ranges(&vm.elf).iter().any(|&(lo, hi)| (lo..hi).contains(&(at + 4))),
                "{off:#x} is outside every menu handler window"
            );
        }
        assert_eq!(MENU_ARROW_KEYS.len(), 16);
    }

    /// And the hook really answers for them: each of the four boxes, presented
    /// the way the guest presents it, comes back moved and scaled.
    #[test]
    fn the_cube_compose_popup_boxes_move_with_their_sprites() {
        assert!(dpad_changed(), "this test needs the shipped CS_DPAD_* defaults");
        let mut vm = loaded_guest();
        // `Vm::boot` does this; `loaded_guest` stops short of booting.
        vm.menu_arrow_keys = menu_arrow_key_ranges(&vm.elf);
        let lcd_g = vm.symbol("_lcdG").expect("named in .symtab");
        let h = 240i32;
        vm.mem.w32(lcd_g + LCD_H, h as u32);
        let sp = vm.cpu.reg_get(Mode::User, reg::SP) - 0x20;
        vm.cpu.reg_set(Mode::User, reg::SP, sp);
        let key = vm.symbol("GameMenuFrame_KeyPressed").unwrap();

        // (return address, rx, ry, w, rh) — the quadruple as the tail of
        // `GameMenuFrame_KeyPressed` builds it.
        let boxes = [
            (key + 0xa630 + 4, 0x1a, h - 0x53, 0x27, 0x17),
            (key + 0xa668 + 4, 0x1a, h - 0x17, 0x27, 0x17),
            (key + 0xa69c + 4, 0x00, h - 0x3c, 0x1a, 0x25),
            (key + 0xa6d4 + 4, 0x41, h - 0x3c, 0x1a, 0x25),
        ];
        let lcd_g = vm.symbol("_lcdG").expect("in .symtab");
        let screen_w = vm.mem.r32(lcd_g.wrapping_add(LCD_W)) as i32;
        let s = dpad_scale();
        let (dx, dy) = (dpad_dx_for(screen_w, s), dpad_dy());
        let present = |vm: &mut Vm, lr: u32, rx: i32, ry: i32, w: i32, rh: i32| {
            vm.cpu.reg_set(Mode::User, reg::LR, lr);
            vm.cpu.reg_set(Mode::User, 2, rx as u32);
            vm.cpu.reg_set(Mode::User, 3, ry as u32);
            vm.mem.w32(sp, w as u32);
            vm.mem.w32(sp + 4, rh as u32);
            // Always declines: `Check_Rect` itself still has to run.
            assert!(!vm.host_menu_arrow_hit().unwrap());
            (
                vm.cpu.reg_get(Mode::User, 2) as i32,
                vm.cpu.reg_get(Mode::User, 3) as i32,
                vm.mem.r32(sp) as i32,
                vm.mem.r32(sp + 4) as i32,
            )
        };

        for (i, (lr, rx, ry, w, rh)) in boxes.into_iter().enumerate() {
            let got = present(&mut vm, lr, rx, ry, w, rh);
            // The hexagon cluster replaces the rect outright; the older
            // sprite-moving path scales the shipped one about its corner.
            let want = if dpad_hex() {
                dpad_plate(MENU_ARROW_DIRS[i], h, s, dx, dy)
            } else {
                (dpad_x(rx, s, dx), dpad_y(ry, h, s, dy), w * s / 100, rh * s / 100)
            };
            assert_eq!(got, want, "the box returning to {lr:#x} was left where the guest put it");
            assert_ne!(got, (rx, ry, w, rh), "nothing moved");
        }

        // The caller gate still bites: the same rect from somewhere that draws
        // no D-pad is left exactly as the guest built it.
        let elsewhere = vm.symbol("Check_Rect").unwrap();
        let (rx, ry, w, rh) = (0x1a, h - 0x53, 0x27, 0x17);
        assert_eq!(present(&mut vm, elsewhere, rx, ry, w, rh), (rx, ry, w, rh));
    }

    /// The duplicate-오토루팅 hook stands on six instruction words and two
    /// decoded branches. If any of them drift the patch must refuse to install
    /// rather than call `ITEMSTORAGE_removeCurItem` at some unrelated address.
    #[test]
    fn the_second_autoloot_is_still_refused_where_this_patch_hooks() {
        let mut vm = loaded_guest();
        let key = vm.symbol("GameMenuFrame_KeyPressed").expect("named in .symtab");
        let remove_cur = vm.symbol("ITEMSTORAGE_removeCurItem").expect("named in .symtab");

        assert_eq!(autoloot_dup_site(key, &mut vm.mem, remove_cur), Some(key + AUTOLOOT_APPLIED));

        // Both arms name the same field, and it is the one the drop paths read
        // (`MONSTER_dropItem+0x184`) and the save writes
        // (`GameFrame_saveGame+0x2a0`): `hero[0x898]`. Spelled out here because
        // the words above would still match if the immediate drifted with them.
        let flag = 0x898u32;
        let mut field = |off: u32| vm.mem.r32(key + off) & 0xfff;
        assert_eq!(field(AUTOLOOT_APPLIED - 0xc), flag, "the test is not hero[0x898]");
        assert_eq!(field(AUTOLOOT_APPLY + 0x10), flag, "the set is not hero[0x898]");

        // Each word in turn: break it, and the site must disappear.
        for (off, word) in AUTOLOOT_WORDS {
            let at = key.wrapping_add(off);
            vm.mem.w32(at, word ^ 0x10);
            assert_eq!(autoloot_dup_site(key, &mut vm.mem, remove_cur), None, "{off:#x} ignored");
            vm.mem.w32(at, word);
        }
        // And the two branches, which the words cannot check: the `beq` has to
        // land on the arm that consumes the item, and the `bl` next to it has
        // to be the call this patch is about to make itself.
        let beq = key + AUTOLOOT_APPLIED - 4;
        let saved = vm.mem.r32(beq);
        vm.mem.w32(beq, saved.wrapping_add(1)); // one word further on
        assert_eq!(autoloot_dup_site(key, &mut vm.mem, remove_cur), None, "beq target ignored");
        vm.mem.w32(beq, saved);
        assert_eq!(
            autoloot_dup_site(key, &mut vm.mem, remove_cur + 4),
            None,
            "the bl is not checked against ITEMSTORAGE_removeCurItem"
        );
        assert_eq!(autoloot_dup_site(key, &mut vm.mem, remove_cur), Some(key + AUTOLOOT_APPLIED));
    }

    /// Why the copy has to be taken away here and cannot simply be sold or
    /// dropped — both halves read out of the guest, because both are the
    /// reason this patch exists at all.
    #[test]
    fn the_autoloot_item_can_be_neither_sold_nor_dropped() {
        let mut vm = loaded_guest();

        // 판매불가: `drawItemInfo+0x1ac` prints the tag when the template's
        // price halfword is the sentinel. `mov r3, #0x10000` / `add r3, r3, fp`
        // with `fp == -1` is `0xffff`; the compare is against `item[0x10]`.
        let info = vm.symbol("drawItemInfo").expect("named in .symtab");
        assert_eq!(vm.mem.r32(info + 0x1a8), 0xe1d4_21b0, "not ldrh r2, [r4, #0x10]");
        assert_eq!(vm.mem.r32(info + 0x1ac), 0xe3a0_3801, "not mov r3, #0x10000");
        assert_eq!(vm.mem.r32(info + 0x1b0), 0xe083_300b, "not add r3, r3, fp");

        // 버리기: the 사용/버리기 two-cell menu is `_GameMenuState` 0x15, and
        // the use dispatcher only sends kinds 0x18 and 0x1d..0x27 there. Kind
        // 0x13 goes to 0x19 instead — its own 예/아니오 confirm, no drop cell.
        let key = vm.symbol("GameMenuFrame_KeyPressed").expect("named in .symtab");
        assert_eq!(vm.mem.r32(key + 0x5924), 0xe353_0013, "not cmp r3, #0x13");
        let beq = vm.mem.r32(key + 0x5928);
        assert_eq!(beq >> 24, 0x0a, "kind 0x13 no longer branches");
        let to = (key + 0x5928 + 8).wrapping_add((((beq << 8) as i32 >> 8) as u32) << 2);
        assert_eq!(to, key + 0xa488, "kind 0x13 goes somewhere new");
        assert_eq!(vm.mem.r32(key + 0xa498), 0xe3a0_2019, "not mov r2, #0x19");
        assert_eq!(vm.mem.r32(key + 0x9d34), 0xe3a0_1015, "state 0x15 moved"); // mov r1, #0x15
    }

    /// The gamepad's D-pad is sent to the guest as keypad digits
    /// (`cs_android::wipi_key`), and this pins which digit is which direction
    /// to the guest's own answer: `HERO_Key_Convert_Dir`'s jump table sends
    /// `'2'` to the very arm WIPI UP (-1) goes to, `'8'` to DOWN's, `'4'` to
    /// LEFT's and `'6'` to RIGHT's.
    ///
    /// This is the test the first pad build needed: it shipped `Up→'8'`, on
    /// the strength of a menu where `'2'` and DOWN looked identical — a
    /// two-state toggle, where every direction looks identical — and the
    /// player report was a walk with its Y axis inverted.
    #[test]
    fn the_pad_walk_digits_mean_what_the_stick_arrows_mean() {
        let mut vm = loaded_guest();
        let f = vm.symbol("HERO_Key_Convert_Dir").expect("named in .symtab");
        // The dispatch this reads through: `cmp r4, #0x3c` then
        // `addls pc, pc, r4, lsl #2`, with r4 = key + 4 and the `b` table
        // starting at +0x20.
        assert_eq!(vm.mem.r32(f + 0x14), 0xe354_003c, "the bound compare moved");
        assert_eq!(vm.mem.r32(f + 0x18), 0x908f_f104, "the table dispatch moved");
        let mut arm = |key: i32| {
            let at = f.wrapping_add(0x20).wrapping_add(4 * (key + 4) as u32);
            let w = vm.mem.r32(at);
            assert_eq!(w >> 24, 0xea, "table entry for key {key} is not a plain `b`: {w:#010x}");
            let off = ((w << 8) as i32) >> 8;
            at.wrapping_add(8).wrapping_add((off * 4) as u32)
        };
        // The four walk digits land on the arrows' own arms...
        assert_eq!(arm(b'2' as i32), arm(crate::KEY_UP), "'2' is the keypad's UP");
        assert_eq!(arm(b'8' as i32), arm(crate::KEY_DOWN), "'8' is the keypad's DOWN");
        assert_eq!(arm(b'4' as i32), arm(crate::KEY_LEFT), "'4' is the keypad's LEFT");
        assert_eq!(arm(b'6' as i32), arm(crate::KEY_RIGHT), "'6' is the keypad's RIGHT");
        // ...and the four arms are genuinely four directions, or the equalities
        // above would hold vacuously on a table that collapsed.
        let dirs = [arm(-1), arm(-2), arm(-3), arm(-4)];
        for i in 0..4 {
            for j in i + 1..4 {
                assert_ne!(dirs[i], dirs[j], "two directions share an arm");
            }
        }
    }

    /// 배지가 앉는 자리는 게스트 자신의 히트 사각형이다. 그 좌표를 꺼내 오는 세
    /// 표 중 **키 표가 앞의 둘을 증명한다** — 정확히 게임이 스킬 슬롯을 부르는
    /// 여섯 숫자여야 하고, 그래야 우리가 옳은 `.rodata` 를 잡은 것이다. 그리고
    /// 그 여섯 개가 전부 패드 자리 하나로 되짚어져야 배지가 빈 칸을 안 만든다.
    #[test]
    fn the_slot_button_tables_are_the_ones_the_hit_test_reads() {
        let mut vm = loaded_guest();

        // 표의 정체 증명 — 키 표가 맞아야 `pad_slot_tables` 가 답을 낸다.
        let tbl = vm.pad_slot_tables().expect("the slot tables moved");

        // 좌표 표: 여섯 칸이 서로 다른 자리이고, 400x240 에서 화면 안이다.
        let (w, h) = (400i32, 240i32);
        let mut seen = Vec::new();
        for i in 0..6usize {
            let dx = vm.mem.r32(tbl + PAD_DX_OFF + 4 * i as u32) as i32;
            let dy = vm.mem.r32(tbl + PAD_DY_OFF + 4 * i as u32) as i32;
            assert!(dx < 0 && dy < 0, "slot {i} is not measured back from the corner");
            let (rx, ry) = (w + dx - PAD_SLOT_BACK, h + dy - PAD_SLOT_BACK);
            assert!(rx >= 0 && ry >= 0, "slot {i} starts off-screen at 400x240");
            assert!(rx + PAD_SLOT_SIZE <= w && ry + PAD_SLOT_SIZE <= h, "slot {i} runs off");
            assert!(!seen.contains(&(rx, ry)), "two slots share a rect");
            seen.push((rx, ry));
        }

        // 여섯 슬롯 전부가 패드 자리로 되짚어지고, 서로 다른 자리다 — 같은 자리가
        // 둘이면 배지 둘이 같은 그림이 되어 아무것도 알려주지 못한다.
        let mut pos = Vec::new();
        for k in PAD_SLOT_KEYS {
            let p = pos_for_key(k).unwrap_or_else(|| panic!("slot key {k} has no position"));
            assert!(!pos.contains(&p), "two slots claim the same pad button: {p:?}");
            pos.push(p);
        }
        // 그리고 그 자리들은 어깨 넷 + 스틱 둘이다.
        assert_eq!(pos.iter().filter(|p| matches!(p, PadPos::Shoulder { .. })).count(), 4);
        assert_eq!(pos.iter().filter(|p| matches!(p, PadPos::Stick { .. })).count(), 2);
    }

    /// 히트테스트 루프가 정말 그 여섯 칸을 32x32 로, 앵커에서 6px 물러나 재는지 —
    /// 배지의 자리가 곧 눌리는 자리라는 주장의 근거.
    #[test]
    fn the_hit_test_loop_still_measures_the_slots_the_way_the_badges_assume() {
        let mut vm = loaded_guest();
        let f = vm.symbol("GameFrame_PressTouchKeypad").expect("named in .symtab");
        // `mov r8, #0x20` — 폭·높이. 루프가 매 바퀴 `[sp]`/`[sp+4]` 로 넘긴다.
        assert_eq!(vm.mem.r32(f + 0x5a8), 0xe3a0_8020, "the slot rect is no longer 0x20 square");
        // `sub r2, r2, #6` / `sub r3, r3, #6` — 앵커에서 물러나는 그 6.
        assert_eq!(vm.mem.r32(f + 0x610), 0xe242_2006, "x is no longer pulled back by 6");
        assert_eq!(vm.mem.r32(f + 0x614), 0xe243_3006, "y is no longer pulled back by 6");
        // `cmp r5, #6` — 여섯 바퀴.
        assert_eq!(vm.mem.r32(f + 0x62c), 0xe355_0006, "the loop no longer runs six slots");
    }

    /// The legend must be drawable with the 비약 turned off — the two features
    /// share the world-screen hook but not a knob — and the hook must decline
    /// outright when neither wants to draw, because that decline is what keeps
    /// a run with no pad byte-identical to a build without the feature.
    #[test]
    fn the_pad_legend_takes_the_world_screen_hook_and_declines_without_a_pad() {
        // Under the compiled defaults (`CS_PAD_HUD` unset = on), the world
        // screen's draw is claimed whatever `CS_POTION_HUD` says.
        assert!(
            hud_patches().contains(&("GameFrame_drawGameUI", Patch::Hud)),
            "no pad legend can ever draw: the world-screen hook is not requested"
        );

        let mut vm = loaded_guest();
        let ui = vm.symbol("GameFrame_drawGameUI").unwrap();
        let render = vm.symbol("GameFrame_Render").unwrap();
        vm.cpu.reg_set(Mode::User, reg::PC, ui);
        vm.cpu.reg_set(Mode::User, reg::LR, render + GAME_UI_RET);
        // No pad, and the buff lines are scoped to the menu by default: the
        // hook must step aside without running a single guest instruction —
        // `loaded_guest` is unbooted, so any attempt to actually draw here
        // would fault, which doubles as the proof none was made.
        assert!(!vm.host_hud().unwrap());
        // A pad, but the wrong caller (the options screen draws the same UI as
        // a preview): still declined.
        vm.gamepad_hud = true;
        vm.cpu.reg_set(Mode::User, reg::LR, render);
        assert!(!vm.host_hud().unwrap());
    }

    /// The probe cannot reach a boss kill, so the only caller of
    /// `DM_DrawImageRotateEx` never runs in a replay — a dump comparison cannot
    /// see this function at all. So the shipped ARM code is the oracle, the same
    /// way it is for the radial blur: run the guest's own body with our patch
    /// lifted out of the table, then run the host over the same input, and
    /// compare the destination byte for byte.
    #[test]
    fn the_guest_agrees_with_our_image_rotate() {
        let mut vm = loaded_guest();
        let func = vm.symbol("DM_DrawImageRotateEx").expect("in .symtab");
        let lcd_g = vm.symbol("_lcdG").expect("in .symtab");
        let table = vm.symbol("BlendingTable32Bit").expect("in .symtab");

        // `DM_ProcPixelBlending` takes its 16bpp path off this byte; without it
        // the guest leaves through the `DM_ByteToIntPixel` function pointers,
        // which are null in an unbooted image.
        vm.mem.w8(lcd_g + 0x8a, 1);

        // The level table is `.bss` until the game fills it, and an all-zero
        // table would make every blend answer zero — the comparison would pass
        // without exercising the lookup at all. Five-bit entries, which is what
        // the shipped table holds and what the 565 recomposition assumes.
        let mut seed = 0x1234_5678u32;
        let mut next = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            seed >> 16
        };
        let lut: Vec<u8> = (0..0x4000).map(|_| (next() & 0x1f) as u8).collect();
        vm.mem.write_bytes(table, &lut);

        for &(sw, sh, dw_px, dh_px) in &[(21i32, 17i32, 40i32, 31i32), (16, 16, 24, 24)] {
            // Slack rows on the source: the walk is not clipped on that side —
            // the step is rounded *up* by one — so the last row can read a pixel
            // or two past the image. The guest does the same, from the same
            // address, so the dumps still agree; the slack only keeps both of
            // them inside mapped memory.
            let src_bytes = (sw * (sh + 2)) as u32 * 2;
            let dst_bytes = (dw_px * dh_px) as u32 * 2;
            let src_px = vm.heap.alloc(src_bytes).expect("guest heap");
            let dst_px = vm.heap.alloc(dst_bytes).expect("guest heap");
            let src_fb = vm.heap.alloc(0x18).expect("guest heap");
            let dst_fb = vm.heap.alloc(0x18).expect("guest heap");
            for &(fb, px, w, h) in &[(src_fb, src_px, sw, sh + 2), (dst_fb, dst_px, dw_px, dh_px)] {
                vm.mem.w32(fb, w as u32);
                vm.mem.w32(fb + 4, h as u32);
                // `DM_GET_FRAME_BUFFER_INFO` divides this by the width to get
                // the bpp byte, so it has to be a real stride.
                vm.mem.w32(fb + 8, (w * 2) as u32);
                vm.mem.w32(fb + 0xc, 0);
                vm.mem.w32(fb + 0x10, px);
            }
            // Structure in all three 565 lanes, so a mistake in any one shows.
            let src_in: Vec<u8> = (0..src_bytes).map(|_| next() as u8).collect();
            let dst_in: Vec<u8> = (0..dst_bytes).map(|_| next() as u8).collect();

            // (x, y, sx, sy, rot, angle, scaleX, scaleY, level, alpha, anchor)
            let cases: &[(i32, i32, i32, i32, u32, i32, i32, i32, u32, i32, u32)] = &[
                // What the one caller actually passes: centre anchor, no
                // rotation, equal scales, a partial alpha.
                (dw_px / 2, dh_px / 2, 0, 0, 0, 0, 100, 100, 0, 0x80, 0x12),
                (dw_px / 2, dh_px / 2, 0, 0, 0, 0, 160, 160, 0, 0x40, 0x12),
                (dw_px / 2, dh_px / 2, 0, 0, 0, 0, 220, 220, 0, 0xc0, 0x12),
                // The plain copy arm: full alpha and no level.
                (2, 3, 0, 0, 0, 0, 100, 100, 0, 0xff, 0),
                // Every quadrant of the angle table, including its seams.
                (dw_px / 2, dh_px / 2, 0, 0, 0, 5, 100, 100, 0, 0x90, 0x12),
                (dw_px / 2, dh_px / 2, 0, 0, 0, 45, 100, 100, 0, 0x90, 0x12),
                (dw_px / 2, dh_px / 2, 0, 0, 0, 90, 100, 100, 0, 0x90, 0x12),
                (dw_px / 2, dh_px / 2, 0, 0, 0, 137, 130, 130, 0, 0x90, 0x12),
                (dw_px / 2, dh_px / 2, 0, 0, 0, 180, 100, 100, 0, 0x90, 0x12),
                (dw_px / 2, dh_px / 2, 0, 0, 0, 233, 100, 100, 0, 0x90, 0x12),
                (dw_px / 2, dh_px / 2, 0, 0, 0, 270, 100, 100, 0, 0x90, 0x12),
                (dw_px / 2, dh_px / 2, 0, 0, 0, 315, 100, 100, 0, 0x90, 0x12),
                (dw_px / 2, dh_px / 2, 0, 0, 0, 360, 100, 100, 0, 0x90, 0x12),
                // Above 360, which is the only case the reduction fires for.
                (dw_px / 2, dh_px / 2, 0, 0, 0, 415, 100, 100, 0, 0x90, 0x12),
                // The three mirror codes, and the reflection bases they use.
                (1, 1, 0, 0, 4, 0, 100, 100, 0, 0x90, 0),
                (1, 1, 0, 0, 8, 0, 100, 100, 0, 0x90, 0),
                (1, 1, 0, 0, 0xc, 0, 100, 100, 0, 0x90, 0),
                (1, 1, 3, 2, 0xc, 0, 100, 100, 0, 0x90, 0),
                // Level table lookups other than block zero.
                (dw_px / 2, dh_px / 2, 0, 0, 0, 0, 100, 100, 1, 0xff, 0x12),
                (dw_px / 2, dh_px / 2, 0, 0, 0, 0, 100, 100, 0xf, 0x77, 0x12),
                // Alpha at both ends of the weighting arm.
                (dw_px / 2, dh_px / 2, 0, 0, 0, 0, 100, 100, 0, 1, 0x12),
                (dw_px / 2, dh_px / 2, 0, 0, 0, 0, 100, 100, 0, 0xfe, 0x12),
                // Anchors: none, right, bottom, and both halves.
                (5, 4, 0, 0, 0, 0, 100, 100, 0, 0x90, 0),
                (dw_px - 2, 4, 0, 0, 0, 0, 100, 100, 0, 0x90, 0x20),
                (5, dh_px - 2, 0, 0, 0, 0, 100, 100, 0, 0x90, 4),
                (dw_px / 2, dh_px / 2, 0, 0, 0, 0, 100, 100, 0, 0x90, 0x14),
                // Scaled down, and scaled unevenly.
                (dw_px / 2, dh_px / 2, 0, 0, 0, 0, 50, 50, 0, 0x90, 0x12),
                (dw_px / 2, dh_px / 2, 0, 0, 0, 0, 300, 90, 0, 0x90, 0x12),
                // Hanging off each edge, so the clip is doing real work.
                (-6, -4, 0, 0, 0, 0, 100, 100, 0, 0x90, 0),
                (dw_px - 3, dh_px - 2, 0, 0, 0, 0, 100, 100, 0, 0x90, 0),
                // Entirely outside: nothing may be written.
                (-200, -200, 0, 0, 0, 0, 100, 100, 0, 0x90, 0),
            ];

            let mut ever_drew = false;
            for &(x, y, sx, sy, rot, angle, scx, scy, level, alpha, anchor) in cases {
                let args: [u32; 15] = [
                    dst_fb,
                    x as u32,
                    y as u32,
                    sw as u32,
                    sh as u32,
                    src_fb,
                    sx as u32,
                    sy as u32,
                    rot,
                    angle as u32,
                    scx as u32,
                    scy as u32,
                    level,
                    alpha as u32,
                    anchor,
                ];
                let case = format!(
                    "{sw}x{sh} -> {dw_px}x{dh_px} at ({x},{y}) rot {rot} angle {angle} \
                     scale {scx}/{scy} level {level} alpha {alpha:#x} anchor {anchor:#x}"
                );

                vm.mem.write_bytes(src_px, &src_in);
                vm.mem.write_bytes(dst_px, &dst_in);
                let keep = vm.patches.clone();
                vm.patches.retain(|&(_, p)| !matches!(p, Patch::ImageRotateEx));
                vm.call(func, &args).expect("the guest's own blitter runs");
                vm.patches = keep;
                let want = vm.mem.slice(dst_px, dst_bytes).unwrap().to_vec();

                vm.mem.write_bytes(src_px, &src_in);
                vm.mem.write_bytes(dst_px, &dst_in);
                for (i, a) in args.iter().take(4).enumerate() {
                    vm.cpu.reg_set(Mode::User, i as u8, *a);
                }
                // Laid out from `[sp]` up, exactly as `Vm::call` just did for
                // the guest: this hook runs before the callee's own `push`, so
                // writing them where the *body* reads them would only make the
                // test agree with a wrong offset.
                let words = args.len() - 4;
                let sp = (vm.cpu.reg_get(Mode::User, reg::SP) & !7) - ((words * 4 + 7) & !7) as u32;
                vm.cpu.reg_set(Mode::User, reg::SP, sp);
                for (i, a) in args[4..].iter().enumerate() {
                    vm.mem.w32(sp + (i * 4) as u32, *a);
                }
                vm.cpu.reg_set(Mode::User, reg::LR, 0);
                assert!(vm.host_image_rotate().unwrap(), "{case}: declined");
                let got = vm.mem.slice(dst_px, dst_bytes).unwrap().to_vec();

                assert_eq!(got, want, "{case}: destination differs");
                ever_drew |= got != dst_in;
            }
            // A blitter that wrote nothing at all would satisfy every assertion
            // above, because the guest would have written nothing either.
            assert!(ever_drew, "{sw}x{sh}: no case drew anything");
        }
    }

    /// The angle arrives in degrees and is looked up in a 19-entry quarter-turn
    /// table, so the table's shape *is* the contract: nineteen `u16` entries,
    /// five degrees apart, Q10, running from 0 to exactly 1024. Get any of that
    /// wrong and the port picks the wrong pair for every angle but zero.
    #[test]
    fn the_angle_table_is_the_shape_the_image_rotate_assumes() {
        let mut vm = loaded_guest();
        let sin_t = vm.symbol("sin1024").expect("in .symtab");
        let sizes = vm.elf.sizes.get("sin1024").copied();
        assert_eq!(sizes, Some(0x26), "sin1024 is 19 halfwords");
        let t: Vec<i32> = (0..19).map(|k| vm.mem.r16(sin_t + 2 * k as u32) as i32).collect();
        assert_eq!(t[0], 0, "sin 0");
        assert_eq!(t[18], 1024, "sin 90 is the Q10 one");
        for k in 1..19 {
            assert!(t[k] > t[k - 1], "entry {k} must climb");
            // Five degrees per entry, rounded to Q10.
            let want = ((k as f64 * 5.0).to_radians().sin() * 1024.0).round() as i32;
            assert!((t[k] - want).abs() <= 1, "entry {k}: {} vs {want}", t[k]);
        }
    }

    // ---- 공용창고 -------------------------------------------------------

    /// The immediate in `str{b,h,d}` / `ldr{b,h,sb,sh,d}` off register `rn`,
    /// or `None` for anything else. Word forms are deliberately not decoded:
    /// no item field is four bytes wide.
    fn imm_access(w: u32, rn: u32, store: bool) -> Option<u32> {
        if w >> 28 != 0xe || (w >> 16) & 0xf != rn {
            return None;
        }
        let (imm12, imm8) = (w & 0xfff, ((w >> 4) & 0xf0) | (w & 0xf));
        match (w & 0x0ff0_0000, w & 0xf0, store) {
            (0x05c0_0000, _, true) => Some(imm12),                  // strb
            (0x05d0_0000, _, false) => Some(imm12),                 // ldrb
            (0x01c0_0000, 0xb0 | 0xf0, true) => Some(imm8),         // strh, strd
            (0x01d0_0000, 0xb0 | 0xd0 | 0xf0, false) => Some(imm8), // ldrh, ldrsb, ldrsh
            (0x01c0_0000, 0xd0, false) => Some(imm8),               // ldrd — L clear, op 1101
            _ => None,
        }
    }

    /// Every field of the live item one of the two save functions touches,
    /// sorted and deduplicated. The loader keeps the item in `sl` (r10) and
    /// the saver in `r8`, and in neither function does that register hold
    /// anything else while the record is being copied, so the set of
    /// immediates *is* the record's field list.
    fn item_fields_of(vm: &mut Vm, name: &str, rn: u32, store: bool) -> Vec<u32> {
        let base = vm.symbol(name).expect("in .symtab");
        let size = vm.elf.sizes.get(name).copied().expect("sized");
        let mut out: Vec<u32> = (0..size)
            .step_by(4)
            .filter_map(|off| imm_access(vm.mem.r32(base + off), rn, store))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// A chest record is the save's own item record, field for field.
    ///
    /// Not two lists we wrote compared with each other: the expected set is
    /// **decoded out of the shipped functions**, so a field we do not know
    /// about fails here rather than silently making deposited items come back
    /// subtly different. A chest that eats an enchant level on the way through
    /// is worse than one that refuses to open.
    #[test]
    fn the_chest_record_is_the_saves_own_item_record() {
        let mut vm = loaded_guest();

        // What `Item_loadStorage` restores after `ITEM_CreateItem`: the table
        // minus the three constructor arguments.
        let mut want: Vec<u32> = ITEM_FIELDS
            .iter()
            .filter(|(at, _, _)| *at > crate::vault::rec::COUNT)
            .map(|(_, off, _)| *off)
            .collect();
        want.sort_unstable();
        assert_eq!(
            item_fields_of(&mut vm, "Item_loadStorage", 10, true),
            want,
            "Item_loadStorage restores a different field set than ITEM_FIELDS lists"
        );

        // `Item_saveStorage` reads those and the three arguments too (twice
        // over for two of them — the gate that skips empty entries).
        let mut want: Vec<u32> = ITEM_FIELDS.iter().map(|(_, off, _)| *off).collect();
        want.sort_unstable();
        want.dedup();
        assert_eq!(
            item_fields_of(&mut vm, "Item_saveStorage", 8, false),
            want,
            "Item_saveStorage writes a different field set than ITEM_FIELDS lists"
        );

        for &(at, off, width) in ITEM_FIELDS {
            assert!(
                matches!(width, 1 | 2 | 8) && at + width <= crate::vault::REC,
                "field {off:#x} does not fit the record"
            );
        }
    }

    /// The record is 36 bytes, seen from both sides: `Item_saveStorage` steps
    /// its cursor by `sl + 0x20` where `sl` is the record start plus four, and
    /// `Item_loadStorage` steps by `0x24`.
    #[test]
    fn a_record_is_thirty_six_bytes_on_both_sides() {
        let mut vm = loaded_guest();
        let save = vm.symbol("Item_saveStorage").expect("in .symtab");
        let load = vm.symbol("Item_loadStorage").expect("in .symtab");
        assert_eq!(vm.mem.r32(save + 0x1bc), 0xe28a_0020, "the saver's stride moved");
        assert_eq!(vm.mem.r32(load + 0x220), 0xe286_6024, "the loader's stride moved");
        assert_eq!(crate::vault::REC, 0x24);
        // ...and the stamp is the last eight bytes of it.
        assert_eq!(crate::vault::rec::STAMP + 8, crate::vault::REC);
    }

    /// The bag is a tab count at `_storage[2]` and one twelve-byte
    /// `LinkedList` per tab from `+4`, each with its own count at `+8`.
    /// [`Vm::bag_list`] and [`Vm::bag_count`] do that arithmetic inline rather
    /// than calling `ITEMSTORAGE_getList` per access, so it is pinned here.
    #[test]
    fn the_bag_is_shaped_the_way_the_chest_walks_it() {
        let mut vm = loaded_guest();
        let get_list = vm.symbol("ITEMSTORAGE_getList").expect("in .symtab");
        assert_eq!(vm.mem.r32(get_list + 0x10), 0xe3a0_300c, "the stride is not twelve");
        assert_eq!(vm.mem.r32(get_list + 0x14), 0xe282_2004, "the lists do not start at +4");
        assert_eq!(vm.mem.r32(get_list + 0x18), 0xe020_2093, "not `mla r0, r3, r0, r2`");
        // `ldrsb r3, [r5, #8]` — the per-tab count `ITEM_getItemFrom` bounds on.
        let from = vm.symbol("ITEM_getItemFrom").expect("in .symtab");
        assert_eq!(vm.mem.r32(from + 0x28), 0xe1d5_30d8, "the list count moved off +8");
        // `ldrb r2, [r2, #2]` — the tab count.
        let save = vm.symbol("Item_saveStorage").expect("in .symtab");
        assert_eq!(vm.mem.r32(save + 0x24), 0xe5d2_2002, "the tab count moved off _storage[2]");
        // `cmp r0, #0x13000000` — twenty entries per tab and no more.
        let add = vm.symbol("ITEMSTORAGE_addItem").expect("in .symtab");
        assert_eq!(vm.mem.r32(add + 0x1c), 0xe350_0413, "the per-tab cap moved");
        assert_eq!(BAG_TAB_SLOTS, 0x14);
    }

    /// Step 2 is the letter screen's own save and the resume is the letter
    /// screen's own slot load. Both were copied rather than invented, so both
    /// are pinned to what they were copied from.
    #[test]
    fn the_transfer_borrows_the_letter_screens_plumbing() {
        let mut vm = loaded_guest();

        // `MainMenuFrame_afterSendLetterWithItem` tail: flag 1, then
        // `GameFrame_save_hero_data`, then a tail branch to `GameFrame_saveGame`.
        let f = vm.symbol("MainMenuFrame_afterSendLetterWithItem").expect("in .symtab");
        assert_eq!(vm.mem.r32(f + 0xb0), 0xe3a0_0001, "the save flag is no longer 1");
        let hero = vm.symbol("GameFrame_save_hero_data").expect("in .symtab");
        let game = vm.symbol("GameFrame_saveGame").expect("in .symtab");
        assert_eq!(branch_target(f + 0xcc, &mut vm.mem, true), Some(hero));
        assert_eq!(branch_target(f + 0xd4, &mut vm.mem, false), Some(game));

        // `MainMenuFrame_KeyPressed+0x2760`: the slot loader, in order.
        let k = vm.symbol("MainMenuFrame_KeyPressed").expect("in .symtab");
        for (off, name) in [
            (0x2788u32, "Load_TalentData"),
            (0x2790, "MP_getHero"),
            (0x27a4, "Init_Shortcut_Slot"),
            (0x27ac, "HERO_Load_SkillData"),
            (0x27b0, "Init_Shortcut_Slot"),
            (0x27b8, "HERO_InitHeroStatus"),
            (0x27cc, "GameFrame_load_hero_data"),
            (0x27f0, "GameFrame_loadGame"),
            (0x2824, "GameFrame_loadPetData"),
        ] {
            let want = vm.symbol(name).expect("in .symtab");
            assert_eq!(
                branch_target(k + off, &mut vm.mem, true),
                Some(want),
                "+{off:#x} is not a call to {name}"
            );
        }
        // `strb r3, [r0, #0xc6]` — the class stamped on hero 0 before the load.
        assert_eq!(vm.mem.r32(k + 0x27a0), 0xe5c0_30c6, "the class byte moved off 0xc6");
        assert_eq!(HERO_CLASS, 0xc6);
        // `add r3, r3, r3, lsl #1` — `_saveSlot` is three bytes per slot.
        assert_eq!(vm.mem.r32(k + 0x2798), 0xe083_3083, "the slot table stride is not three");
        assert_eq!(vm.elf.sizes.get("_saveSlot").copied(), Some(9), "three slots of three");
    }

    /// `ITEM_CreateItem` stamps from the clock and dedupes against exactly one
    /// previous value. That is the whole reason [`Vm::next_stamp`] exists, and
    /// the reason it writes its answer back into `_menuOpt`.
    #[test]
    fn the_games_own_stamp_is_unique_only_within_a_run() {
        let mut vm = loaded_guest();
        let f = vm.symbol("ITEM_CreateItem").expect("in .symtab");
        let clock = vm.symbol("MC_knlCurrentTime").expect("in .symtab");
        assert_eq!(
            branch_target(f + 0x44, &mut vm.mem, true),
            Some(clock),
            "the stamp is not the clock any more"
        );
        // strd r0, r1, [r4, #0xd0]
        assert_eq!(vm.mem.r32(f + 0x4c), 0xe1c4_0df0, "the stamp moved off item[0xd0]");
        assert_eq!(ITEM_STAMP, 0xd0);
        // ldr ip, [r2] / cmp ip, r0 — the low word of one previous value.
        assert_eq!(vm.mem.r32(f + 0x54), 0xe592_c000);
        assert_eq!(vm.mem.r32(f + 0x58), 0xe15c_0000);
    }

    /// The chest's four hooks, and that **any** drift refuses them.
    ///
    /// Each one stands in for a whole shipped function or block through
    /// `Vm::call`, so a build where one of them has moved must install nothing
    /// rather than call into the middle of something else. The branches matter
    /// as much as the words: they are what say `_mLetterState == 4` is the bag
    /// grid on both the key and the draw side, and that the block at
    /// `+0x2768` is still the slot loader.
    #[test]
    fn the_chest_screen_is_where_the_letter_frame_keeps_its_two_widgets() {
        let mut vm = loaded_guest();
        let args = vault_test_args(&mut vm);
        assert!(vault_sites_of(&mut vm, args).is_some(), "the shipped guest");

        // Every word the install reads, and every branch it decodes.
        let (key, letter, bag_key, bag_draw, slot_draw) = (args.0, args.1, args.2, args.3, args.4);
        let touched = [
            key + VAULT_KEY_EXIT,
            key + VAULT_KEY_EXIT_POP,
            key + VAULT_SLOT_OK,
            key + VAULT_SLOT_OK_HEAD,
            key + VAULT_SLOT_OK_NEXT,
            key + VAULT_SLOT_KEY,
            key + VAULT_GRID_STATE,
            key + VAULT_SLOT_TOUCH,
            key + VAULT_SLOT_TALENT,
            key + VAULT_SLOT_HERO,
            key + VAULT_SLOT_GAME,
            key + VAULT_GRID_KEY_TAIL,
            letter + VAULT_GRID_DRAW_CALL,
            bag_key,
            bag_key + VAULT_BAG_KEY_ARG,
            bag_key + VAULT_BAG_KEY_CALL,
            bag_key + VAULT_BAG_KEY_BACK,
            bag_key + VAULT_BAG_KEY_PICK,
            bag_draw,
            bag_draw + VAULT_BAG_DRAW_ID,
            bag_draw + VAULT_BAG_DRAW_STRING,
            bag_draw + VAULT_BAG_DRAW_GRID,
            slot_draw,
        ];
        for at in touched {
            let was = vm.mem.r32(at);
            vm.mem.w32(at, was ^ 0x10);
            assert!(vault_sites_of(&mut vm, args).is_none(), "{at:#x} is not checked");
            vm.mem.w32(at, was);
        }
        assert!(vault_sites_of(&mut vm, args).is_some(), "restored");

        // And the four addresses it hands back are the ones the handlers
        // assume — the slot block's shared entry, not its head.
        let sites = vault_sites_of(&mut vm, args).unwrap();
        assert_eq!(sites.len(), 4);
        assert!(sites.contains(&(key + VAULT_SLOT_OK, Patch::VaultSlotOk)));
        assert!(sites.contains(&(bag_key, Patch::VaultBagKey)));
        assert!(sites.contains(&(bag_draw, Patch::VaultBagDraw)));
        assert!(sites.contains(&(slot_draw, Patch::VaultSlotDraw)));
    }

    /// The storage object is self-contained and exactly the size
    /// [`Vm::with_chest`] copies, which is what makes swapping its bytes a
    /// complete change of which list every shipped bag function means.
    #[test]
    fn the_storage_object_is_the_shape_the_chest_view_copies() {
        let mut vm = loaded_guest();
        // Shipped: five twelve-byte lists from +4, the cursor right after, and
        // `st_size` agreeing that nothing else is in there.
        assert_eq!(BAG_CURSOR, 4 + 12 * 5);
        assert_eq!(vm.elf.sizes.get("_storage").copied(), Some(BAG_CURSOR + 4));
        // The cave's copy: ten lists, and its own `BAGSLOT_OFF`.
        assert_eq!(BAG_CURSOR_CAVE, 4 + 12 * 10);
        assert!(vm.cave_installed.contains(&"bag10"), "loaded_guest installs the cave");
        assert_eq!(vm.bag_cursor_off(), BAG_CURSOR_CAVE);
        assert_eq!(vm.bag_storage_size(), Some(BAG_CURSOR_CAVE as usize + 4));
        // ...and it is the object the guest's GOT slot points at, with room
        // for all of it inside the cave's own `.bss`.
        let storage10 = vm.symbol("storage10").expect("a cave symbol");
        assert_eq!(vm.bag_storage(), Some(storage10));
        let m = crate::cave::manifest().expect("the manifest parses");
        assert!(m.bss_end >= storage10 + BAG_CURSOR_CAVE + 4);
    }

    /// A swap shows the chest and gives the bag back **byte for byte**.
    ///
    /// The whole safety of the trick is that the window is one call long and
    /// nothing outside it can see the chest — in particular `Item_saveStorage`,
    /// which reads the same object during step 2 of every transfer. So what
    /// has to hold is that the object is exactly as it was afterwards, that
    /// what was linked inside the window went into the view, and that the
    /// guest's own walk sees each list only while its image is in place.
    #[test]
    fn the_chest_view_swaps_in_and_the_bag_comes_back_unchanged() {
        let mut vm = loaded_guest();
        let at = vm.bag_storage().expect("a storage object");
        let n = vm.bag_storage_size().expect("a size");
        let init = vm.symbol("ItemBag_init").expect("in .symtab");
        for i in 0..2u32 {
            vm.call(init, &[at, i]).unwrap();
        }
        vm.mem.w8(at + 2, 2); // two tabs

        // One entry in the bag, so "unchanged" has something to be about.
        let in_bag = fake_bag_item(&mut vm, 0x1111);
        assert_eq!(vm.bag_count(0), 1);
        assert_eq!(vm.bag_item(0, 0).unwrap(), in_bag);
        let bag: Vec<u8> = (0..n as u32).map(|i| vm.mem.r8(at + i)).collect();

        vm.vault_view = vec![0u8; n];
        let in_chest = vm
            .with_chest(|vm| {
                let at = vm.bag_storage().unwrap();
                vm.call(init, &[at, 0])?;
                vm.mem.w8(at + 2, 1);
                assert_eq!(vm.bag_count(0), 0, "the chest starts empty inside the window");
                let it = fake_bag_item(vm, 0x2222);
                assert_eq!(vm.bag_count(0), 1);
                Ok(it)
            })
            .unwrap();

        let after: Vec<u8> = (0..n as u32).map(|i| vm.mem.r8(at + i)).collect();
        assert_eq!(bag, after, "the bag did not come back byte for byte");
        assert_eq!(vm.bag_count(0), 1, "and it still has its own one entry");
        assert_eq!(vm.bag_item(0, 0).unwrap(), in_bag, "and it is still the bag's item");
        // The view kept what was linked into it, and it is not the bag's image.
        assert_ne!(vm.vault_view, bag);
        assert_eq!(vm.vault_view[2], 1, "one tab");
        vm.with_chest(|vm| {
            assert_eq!(vm.bag_count(0), 1, "the chest came back with its entry");
            assert_eq!(vm.bag_item(0, 0).unwrap(), in_chest);
            Ok(())
        })
        .unwrap();
    }

    /// The game's stamps are not unique inside one bag, which is why a deposit
    /// re-mints before it arms.
    ///
    /// Two entries a mint apart in the same millisecond carry the same number —
    /// the four starting pieces are two such pairs — kind 0 with kind 12 and
    /// kind 5 with kind 15, measured at `-806994733`/`-806994732` in the run
    /// that lost the hammer and at `1700000054483`/`1700000054484` in another,
    /// so the numbers move and the pairing does not. A resume asks exactly
    /// "is this stamp in
    /// the bag?", so without the re-mint a committed step 2 answers *yes*
    /// through the twin, the verdict becomes "step 2 never landed", and the
    /// rollback drops an item that is already gone. That destroyed a hammer in
    /// the probe.
    #[test]
    fn a_stamp_shared_with_another_bag_entry_is_visible_as_shared() {
        let mut vm = loaded_guest();
        let at = vm.bag_storage().expect("a storage object");
        let init = vm.symbol("ItemBag_init").expect("in .symtab");
        vm.call(init, &[at, 0]).unwrap();
        vm.mem.w8(at + 2, 1);

        fake_bag_item(&mut vm, 0x1234_5678_9abc);
        assert_eq!(vm.bag_stamp_twins(0x1234_5678_9abc).unwrap(), 1);
        let twin = fake_bag_item(&mut vm, 0x1234_5678_9abc);
        assert_eq!(vm.bag_stamp_twins(0x1234_5678_9abc).unwrap(), 2, "a twin is a twin");
        // `bag_find` cannot tell them apart, which is the failure itself.
        assert!(vm.bag_find(0x1234_5678_9abc).unwrap().is_some());

        // A fresh stamp is above everything in reach, so re-minting one of them
        // ends the collision — and the other is then found on its own.
        let fresh = vm.next_stamp().unwrap();
        assert!(fresh > 0x1234_5678_9abc);
        vm.mem.w32(twin + ITEM_STAMP, fresh as u32);
        vm.mem.w32(twin + ITEM_STAMP + 4, (fresh >> 32) as u32);
        assert_eq!(vm.bag_stamp_twins(0x1234_5678_9abc).unwrap(), 1);
        assert_eq!(vm.bag_stamp_twins(fresh).unwrap(), 1);
        assert_eq!(vm.bag_find(fresh).unwrap().map(|(_, _, i)| i), Some(twin));
    }

    /// An item in the bag without the game's file layer.
    ///
    /// `ITEM_CreateItem` loads a template through `openGbmMergerFile`, which
    /// needs an initialised MIDAS file layer and so cannot run in a VM that has
    /// only been booted. What the walk actually needs is smaller: the object
    /// embedded at `item + 0x9c` whose `+0x18` points back at the item
    /// (`ITEM_getItemFrom` ends in `OBJ_getPtr`), linked by the guest's own
    /// `ITEMSTORAGE_addItem`.
    fn fake_bag_item(vm: &mut Vm, stamp: i64) -> u32 {
        let add = vm.symbol("ITEMSTORAGE_addItem").expect("in .symtab");
        let item = vm.heap.alloc(0x200).expect("guest heap");
        for i in (0..0x200u32).step_by(4) {
            vm.mem.w32(item + i, 0);
        }
        vm.mem.w32(item + 0x9c + 0x18, item); // OBJ_getPtr's payload
        vm.mem.w32(item + ITEM_STAMP, stamp as u32);
        vm.mem.w32(item + ITEM_STAMP + 4, (stamp >> 32) as u32);
        vm.mem.w8(item + ITEM_EQUIPPED, 0xff); // not worn
        vm.call(add, &[item, 0]).expect("the guest links it");
        item
    }

    /// The two ids the screen takes over are the strings it thinks they are,
    /// and what it puts there is well-formed EUC-KR with no `%`.
    ///
    /// `setPopUp` and `MC_knlSprintk` both treat their argument as a **format**,
    /// and the font will draw any two bytes as some syllable, so a wrong byte
    /// here is plausible-looking nonsense rather than a visible failure.
    #[test]
    fn the_chest_screens_words_are_the_ones_it_stands_in_for() {
        let mut vm = loaded_guest();
        let f = vm.symbol("getInCodeString").expect("in .symtab");
        let ask = |vm: &mut Vm, id: u32| {
            let at = vm.call(f, &[id]).unwrap();
            vm.mem.cstr_bytes(at)
        };
        // The tile labels are one run of ids, `0x1c2 + tile`. Its neighbours
        // are untouched, and that is what makes 0x1c3 tile 1's — 0x1c3 itself
        // is already answered by our own hook, which is the other half of the
        // pair worth pinning.
        // **EUC-KR, not `String`.** A guest string compared against a Rust
        // literal would be comparing two different encodings of the same word
        // and never match; this is the trap `CLAUDE.md` records.
        assert_eq!(ask(&mut vm, 0x1c2), b"\xbd\xc3\xc0\xdb", "tile 0 is 시작");
        assert_eq!(ask(&mut vm, 0x1c4), b"\xc6\xed\xc1\xf6", "tile 2 is 편지");
        assert_eq!(ask(&mut vm, VAULT_LABEL_ID), VAULT_LABEL, "tile 1 is ours");
        // The grid's heading is only taken over while the chest owns the frame,
        // so a fresh VM still gets the letter's own wording.
        assert_eq!(vm.vault_screen, crate::vault::Screen::Off);
        assert_eq!(
            ask(&mut vm, VAULT_TITLE_ID),
            b"\xba\xb8\xb3\xbe\x20\xbe\xc6\xc0\xcc\xc5\xdb\x20\xbc\xb1\xc5\xc3",
            "the grid's heading is the letter's 보낼 아이템 선택"
        );

        for (name, s) in [
            ("label", VAULT_LABEL),
            ("bag", VAULT_BAG),
            ("chest", VAULT_CHEST),
            ("full", VAULT_FULL),
            ("worn", VAULT_WORN),
            ("no room", VAULT_NO_ROOM),
            ("broken", VAULT_BROKEN),
            ("empty slot", VAULT_EMPTY_SLOT),
        ] {
            let mut i = 0;
            while i < s.len() {
                let c = s[i];
                if c < 0x80 {
                    assert!(c == b' ' || c == b';', "{name} byte {i} ({c:#04x})");
                    i += 1;
                } else {
                    assert!((0x81..=0xfe).contains(&c), "{name} byte {i} is not a lead");
                    let t = *s.get(i + 1).expect("lead byte without a trail");
                    assert!((0x41..=0xfe).contains(&t), "{name} byte {} is not a trail", i + 1);
                    i += 2;
                }
            }
            assert!(!s.contains(&b'%'), "{name} is passed as a format string");
            assert!(s.len() < HUD_SCRATCH, "{name} does not fit the host's line buffer");
        }
        assert_eq!(VAULT_LABEL_ID, 0x1c2 + VAULT_TILE, "the label id is the tile's");
    }

    /// The panel words sit in the band the shipped heading had, and the two
    /// halves do not overlap — the second is the target that switches panels,
    /// and the first is the one that switches back.
    #[test]
    fn the_two_panel_words_divide_the_headings_band() {
        let mut vm = loaded_guest();
        // `draw_CommonItembag` places its heading at (w/2 - 0x32, h/2 - 0x5c)
        // with a length cap of 0x64: `sub r8, r8, #0x32` / `sub r7, r7, #0x5c`
        // / `mov ip, #0x64`.
        let d = vm.symbol("draw_CommonItembag").expect("in .symtab");
        assert_eq!(vm.mem.r32(d + 0x64), 0xe248_8032, "the heading's x moved");
        assert_eq!(vm.mem.r32(d + 0x68), 0xe247_705c, "the heading's y moved");
        assert_eq!(vm.mem.r32(d + 0x80), 0xe3a0_c064, "the heading's width moved");
        assert_eq!((VAULT_TITLE_DX, VAULT_TITLE_DY, VAULT_TITLE_W), (-0x32, -0x5c, 0x64));
        assert_eq!(crate::vault::Panel::Bag.other(), crate::vault::Panel::Chest);
        assert_eq!(crate::vault::Panel::Chest.other(), crate::vault::Panel::Bag);
    }

    /// Everything `vault_sites` needs, resolved once.
    fn vault_test_args(vm: &mut Vm) -> (u32, u32, u32, u32, u32, u32, u32, u32, u32, u32, u32) {
        let s = |n: &str| vm.symbol(n).unwrap_or_else(|| panic!("{n} in .symtab"));
        (
            s("MainMenuFrame_KeyPressed"),
            s("MainMenuFrame_drawLetter"),
            s("MainMenuFrame_ItemBagKeyPressed"),
            s("draw_CommonItembag"),
            s("MainMenuFrame_drawMenuSlot"),
            s("ItemBagKey"),
            s("drawItemBag"),
            s("getInCodeString"),
            s("Load_TalentData"),
            s("GameFrame_load_hero_data"),
            s("GameFrame_loadGame"),
        )
    }

    fn vault_sites_of(
        vm: &mut Vm,
        a: (u32, u32, u32, u32, u32, u32, u32, u32, u32, u32, u32),
    ) -> Option<Vec<(u32, Patch)>> {
        vault_sites(a.0, a.1, a.2, a.3, a.4, a.5, a.6, a.7, a.8, a.9, a.10, &mut vm.mem)
    }

    /// A press on the retired 편지 tile stores 7, and 7 has to mean what a
    /// press on shipped tile 7 meant: the grey-tile alert, with a way out.
    /// Both dispatches are decoded rather than assumed.
    #[test]
    fn the_dead_letter_tile_goes_where_the_shipped_grey_tiles_went() {
        let mut vm = loaded_guest();
        let render = vm.symbol("MainMenuFrame_Render").expect("in .symtab");
        let key = vm.symbol("MainMenuFrame_KeyPressed").expect("in .symtab");
        let alert = vm.symbol("drawAndroidAlertMsg").expect("in .symtab");

        // `addls pc, pc, r3, lsl #2`: state N's arm is the `b` at +8 + 4N.
        assert_eq!(vm.mem.r32(render + MENU_RENDER_DISPATCH), 0x908f_f103);
        let arm = |vm: &mut Vm, state: u32| {
            branch_target(render + MENU_RENDER_DISPATCH + 8 + 4 * state, &mut vm.mem, false)
                .expect("a jump-table entry")
        };
        let dead = arm(&mut vm, UNUSED_MENU_STATE);
        assert_eq!(dead, arm(&mut vm, 1), "states 1 and 7 shipped as one grey-tile screen");
        assert_ne!(dead, arm(&mut vm, LETTER_TILE), "the letter frame is a different arm");
        assert_eq!(
            branch_target(dead, &mut vm.mem, true),
            Some(alert),
            "that screen is 사용되지 않는 메뉴입니다"
        );
        // The key dispatch (`addls pc, pc, r2, lsl #2`) has an arm for it too,
        // not the epilogue: the alert can be dismissed.
        assert_eq!(vm.mem.r32(key + MENU_KEY_DISPATCH), 0x908f_f102);
        let karm = branch_target(key + MENU_KEY_DISPATCH + 8 + 4 * UNUSED_MENU_STATE, &mut vm.mem, false)
            .expect("a jump-table entry");
        assert_ne!(karm, key + VAULT_KEY_EXIT, "state 7's keys go straight to the epilogue");

        // And it is 편지's tile, whose number the commit would have stored.
        let f = vm.symbol("getInCodeString").expect("in .symtab");
        let at = vm.call(f, &[0x1c2 + LETTER_TILE]).unwrap();
        assert_eq!(vm.mem.cstr_bytes(at), b"\xc6\xed\xc1\xf6", "tile 2 is 편지");
        assert_eq!(UNUSED_MENU_STATE, MY_ID_TILE, "the number a press on tile 7 stored");
        // Both sites still install with the two features off and the letter
        // retired, since the retirement needs them.
        assert!(my_id_key_site(key, &mut vm.mem).is_some());
        let draw = vm.symbol("MainMenuFrame_drawMainMenu").expect("in .symtab");
        assert!(my_id_draw_site(draw, &mut vm.mem).is_some());

        // The fee branch: `cmp ip, #2` / `beq +0x21a8`, and that target is the
        // block that raises the question and leaves (`strb r1, [r2]` with 1,
        // then `b +0x248`), not the commit.
        let round = vm.symbol("UI_RoundRect").expect("in .symtab");
        assert_eq!(letter_sites(key, draw, round, &mut vm.mem), Some(key + LETTER_BRANCH));
        assert_eq!(vm.mem.r32(key + LETTER_BRANCH_CMP), LETTER_BRANCH_CMP_WORD);
        assert_eq!(vm.mem.r32(key + LETTER_BRANCH_TARGET), 0xe3a0_1001, "mov r1, #1");
        assert_eq!(
            branch_target(key + LETTER_BRANCH_TARGET + 0x14, &mut vm.mem, false),
            Some(key + VAULT_KEY_EXIT),
            "the fee block leaves through the epilogue without committing"
        );
        // The retirement is all or nothing: every word it reads is checked,
        // and the branch it skips is `beq`, not something with a side effect.
        let touched: Vec<u32> = [key + LETTER_BRANCH, key + LETTER_BRANCH_CMP, key + MY_ID_COMMIT]
            .into_iter()
            .chain(MENU_GREY_WORDS.iter().map(|&(off, _)| draw + off))
            .chain([draw + MENU_GREY_CONT, draw + MENU_GREY_CALL, draw + MY_ID_GREY])
            .collect();
        for at in touched {
            let was = vm.mem.r32(at);
            vm.mem.w32(at, was ^ 0x10);
            assert!(letter_sites(key, draw, round, &mut vm.mem).is_none(), "{at:#x} is not checked");
            vm.mem.w32(at, was);
        }
        assert!(letter_sites(key, draw, round, &mut vm.mem).is_some(), "restored");
        assert_eq!(vm.mem.r32(key + LETTER_BRANCH) >> 24, 0x0a, "beq");
        // And the box the retired tile draws is the shipped grey box: the
        // shipped path's `x` is `w/2 - 0x15` with no column term, which is why
        // it cannot be reused for column 2.
        assert_eq!((MENU_GREY_DX, MENU_GREY_DY, MENU_GREY_W, MENU_GREY_H), (0x15, 0x40, 0x2b, 0x28));
        assert_eq!(MENU_TILE_STRIDE, 0x2e);
    }

    /// The gold word and its ceiling, read off the guest rather than assumed:
    /// the line the grid prints, the letter's debit, and the clamp a credited
    /// letter runs into.
    #[test]
    fn the_gold_field_and_cap_are_the_guests_own() {
        let mut vm = loaded_guest();
        let s = |vm: &mut Vm, n: &str| vm.symbol(n).unwrap_or_else(|| panic!("{n} in .symtab"));
        let bag = s(&mut vm, "drawItemBag");
        assert_eq!(vm.mem.r32(bag + 0xd8), 0xe595_389c, "ldr r3, [r5, #0x89c] — the 소지금 line");
        // ...whose label is id 0x18e, `mov r0, #0x18c` / `add r0, r0, #2`.
        assert_eq!(vm.mem.r32(bag + 0xc0), 0xe3a0_0f63);
        assert_eq!(vm.mem.r32(bag + 0xc4), 0xe280_0002);
        let f = s(&mut vm, "getInCodeString");
        let at = vm.call(f, &[0x18e]).unwrap();
        assert_eq!(vm.mem.cstr_bytes(at), b"\xbc\xd2\xc1\xf6\xb1\xdd", "소지금");
        let send = s(&mut vm, "MainMenuFrame_afterSendLetterWithItem");
        assert_eq!(vm.mem.r32(send + 0x2c), 0xe590_289c, "ldr r2, [r0, #0x89c]");
        assert_eq!(vm.mem.r32(send + 0x34), 0xe580_389c, "str r3, [r0, #0x89c]");
        assert_eq!(HERO_GOLD, 0x89c);

        // The cap: `ldr sb, [pc, #0x280]` at +0x6b0, `cmp r3, sb` at +0x6e8,
        // after a letter's gold is added.
        let recv = s(&mut vm, "ProcRecvPacket_Single");
        let at = recv + 0x6b0;
        let w = vm.mem.r32(at);
        assert_eq!(w, 0xe59f_9280);
        assert_eq!(vm.mem.r32(at + 8 + (w & 0xfff)), crate::vault::GOLD_CAP);
        assert_eq!(vm.mem.r32(at + 0x38), 0xe153_0009, "cmp r3, sb");
        assert_eq!(GOLD_DIGITS as usize, crate::vault::GOLD_CAP.to_string().len());

        for (typed, want) in [
            (&b"500"[..], 500),
            (b"", 0),
            (b"0", 0),
            (b"12a3", 12),
            (b"999999999", crate::vault::GOLD_CAP),
            (b"1000000000", crate::vault::GOLD_CAP + 1),
            (b"99999999999999", crate::vault::GOLD_CAP + 1),
        ] {
            assert_eq!(gold_amount(typed), want, "{typed:?}");
        }
    }

    /// The two gold words sit where nothing of the shipped grid does — its
    /// back button and its cells are read off `ItemBagKey` — and the strings
    /// are whole EUC-KR with no `%` for `setPopUp`'s format.
    #[test]
    fn the_gold_words_and_targets_clear_the_grids_own() {
        let mut vm = loaded_guest();
        let keys = vm.symbol("ItemBagKey").expect("in .symtab");
        // `Check_Rect(x, y, w - 0x30, 0, 0x30, 0x36)` — the back button.
        assert_eq!(vm.mem.r32(keys + 0x520), 0xe3a0_c030, "mov ip, #0x30");
        assert_eq!(vm.mem.r32(keys + 0x53c), 0xe3a0_c036, "mov ip, #0x36");
        assert_eq!(vm.mem.r32(keys + 0x540), 0xe242_2030, "sub r2, r2, #0x30");
        assert_eq!(branch_target(keys + 0x548, &mut vm.mem, true), vm.symbol("Check_Rect"));
        // The first row of cells: `(w/2 - 0x37, h/2 - 0x4b)`.
        assert_eq!(vm.mem.r32(keys + 0x3bc), 0xe242_2037, "sub r2, r2, #0x37");
        assert_eq!(vm.mem.r32(keys + 0x3c0), 0xe243_304b, "sub r3, r3, #0x4b");
        let (w, h) = (crate::WIPI_W as i32, crate::WIPI_H as i32);
        for (x, width) in [(GOLD_PUT_HIT_X, GOLD_PUT_HIT_W), (GOLD_TAKE_HIT_X, GOLD_TAKE_HIT_W)] {
            assert!(x + width < w - 0x30, "into the back button on a {w}-wide screen");
            assert!(GOLD_HIT_Y + GOLD_HIT_H < h / 2 - 0x4b, "into the first row of cells");
            assert!(x >= 0 && GOLD_HIT_Y >= 0);
        }
        // `Check_Rect` is inclusive at both ends, so the two must not even touch.
        assert!(GOLD_PUT_HIT_X + GOLD_PUT_HIT_W < GOLD_TAKE_HIT_X, "the two words' boxes meet");
        assert!(GOLD_PUT_X >= GOLD_PUT_HIT_X && GOLD_TAKE_X >= GOLD_TAKE_HIT_X, "text outside its box");
        assert_eq!(GOLD_LINE_Y, 5, "the row 소지금 is on");

        for (name, s) in [
            ("put", GOLD_PUT),
            ("take", GOLD_TAKE),
            ("ask put", GOLD_ASK_PUT),
            ("ask take", GOLD_ASK_TAKE),
            ("none", GOLD_NONE),
            ("short hero", GOLD_SHORT_HERO),
            ("short chest", GOLD_SHORT_CHEST),
            ("cap put", GOLD_CAP_PUT),
            ("cap take", GOLD_CAP_TAKE),
            ("busy", GOLD_BUSY),
            ("chest line", &GOLD_CHEST_LINE[1..GOLD_CHEST_LINE.len() - 1]),
        ] {
            let mut i = 0;
            while i < s.len() {
                let c = s[i];
                if c < 0x80 {
                    assert!(c == b' ', "{name} byte {i} ({c:#04x})");
                    i += 1;
                } else {
                    assert!((0x81..=0xfe).contains(&c), "{name} byte {i} is not a lead");
                    let t = *s.get(i + 1).expect("lead byte without a trail");
                    assert!((0x41..=0xfe).contains(&t), "{name} byte {} is not a trail", i + 1);
                    i += 2;
                }
            }
            assert!(!s.contains(&b'%'), "{name} is passed as a format string");
            assert!(s.len() + (GOLD_DIGITS as usize) < HUD_SCRATCH, "{name} does not fit");
        }
        // The chest line's two escapes are the shipped 소지금's own: `o` opens
        // the label colour and `&` the number's.
        assert_eq!(GOLD_CHEST_LINE[0], b'o');
        assert_eq!(GOLD_CHEST_LINE[GOLD_CHEST_LINE.len() - 1], b'&');
    }
}
