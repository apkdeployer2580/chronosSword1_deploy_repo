/* 자동기본공격 — auto basic attack (feature "autoatk"), the CS2 mod's
 * FUN_013581e8 on Chronos Sword 1's hero.
 *
 * Every third world tick (GameFrame_Process's `bl Map_objectPreProcess`, so
 * about seven times a second), when nothing else has the hero — no NPC
 * screen or world map, no cutscene, alive, standing with no attack in flight
 * or already in an attack (state 4, where a press queues the next hit of the
 * combo) — pick the nearest monster that Hero_canAttack, is not a map
 * object and is in attack range, keep the current target if it still
 * qualifies, and press the attack key for the hero.
 *
 * The press is the guest's own HERO_Key_Press('5'): everything after it —
 * facing the target, the motion, the combo queue, sounds — is shipped code.
 * The one thing it would do differently is choose the target itself in
 * HERO_Check_Autotarget (or drop it, when 자동타겟 is off), so while our press
 * is in flight that function answers 1 and leaves hero[0x288] alone. */
#include "cave.h"

static u8 aa_tick;
static u8 aa_forcing;

int autoatk_forcing(void)
{
    return aa_forcing;
}

/* Does monster `i` qualify as the hero's target right now? Attack range is
 * judged by the guest against hero[0x288], so the candidate is written there
 * for the check; the caller restores or keeps it. */
static int target_ok(u8 *hero, int who, int i)
{
    u8 *m;
    if (i < 0 || i >= MONSTER_LIST_MAX || PointerList_isEnd(_monsterList, i))
        return 0;
    m = _monsterList[i];
    if (m == NULL || !Hero_canAttack(m) || MONSTER_isObject(m) || !MONSTER_Check_type_canAttack(m))
        return 0;
    hero[HERO_TARGET] = (u8)i;
    return HERO_Check_Attack_Range(who) != 0;
}

void autoatk_world_tick(void)
{
    u8 *hero;
    int who, i, cur, best = -1;
    s32 bestd = 0x7fffffff;
    Map_objectPreProcess();
    if (!cave_cfg.autoatk)
        return;
    opt_load();
    if (!opt.autoatk)
        return;
    if (++aa_tick < 3)
        return;
    aa_tick = 0;
    if (GameFrame_stopProcess() || Event_isEvent())
        return;
    who = CUR_HERO;
    hero = MP_getHero(who);
    if (hero == NULL || *(s32 *)(hero + HERO_HP) < 1)
        return;
    if (hero[HERO_STATE] == 0) {
        if (*(s32 *)(hero + HERO_ATK_WAIT) != 0)
            return;
    } else if (hero[HERO_STATE] != 4) {
        return;
    }
    cur = (s8)hero[HERO_TARGET];
    if (cur >= 0 && target_ok(hero, who, cur)) {
        best = cur;
    } else {
        for (i = 0; i < MONSTER_LIST_MAX; i++) {
            u8 *m;
            s32 dx, dy, d;
            if (PointerList_isEnd(_monsterList, i))
                break;
            if (!target_ok(hero, who, i))
                continue;
            m = _monsterList[i];
            dx = *(s32 *)(m + MON_X) - *(s32 *)(hero + HERO_X);
            dy = *(s32 *)(m + MON_Y) - *(s32 *)(hero + HERO_Y);
            if (dx < 0)
                dx = -dx;
            if (dy < 0)
                dy = -dy;
            d = dx + dy;
            if (d < bestd) {
                bestd = d;
                best = i;
            }
        }
    }
    if (best < 0) {
        hero[HERO_TARGET] = (u8)cur; /* the probes above wrote it */
        return;
    }
    hero[HERO_TARGET] = (u8)best;
    aa_forcing = 1;
    HERO_Key_Press('5', who);
    aa_forcing = 0;
}
