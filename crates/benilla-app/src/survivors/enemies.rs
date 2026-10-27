//! The horde: spawning on a ring outside the view, the chase, contact damage, death, and the
//! experience gems the dead leave.

use std::collections::HashMap;

use benilla_world::rig_anim::GlobalSeqDrive;
use benilla_world::world_point::WorldPoint;
use bevy::prelude::*;

use super::abilities::Hit;
use super::coop::Mate;
use super::data::{self, EnemyDef};
use super::hero::{self, Hero, HERO_RADIUS};
use super::units::{self, flat, yaw_toward, Fx, Marks, UnitSpec, COLOR_HURT};
use super::{Phase, Rng, Run, Selection};
use crate::creature_anim::{move_flags, MovementState};
use crate::net::{FieldChanged, Guid, ObjectStore};

#[derive(Component)]
pub(super) struct Enemy {
    pub def: &'static EnemyDef,
    pub hp: f32,
    pub max_hp: f32,
    pub speed: f32,
    pub damage: f32,
    pub radius: f32,
    pub xp: u32,
    pub boss: bool,
    pub attack_cd: f32,
    pub slow: f32,
    pub slow_t: f32,
    pub root_t: f32,
    pub stun_t: f32,
    pub fear_t: f32,
    pub weaken: f32,
    pub weaken_t: f32,
    pub dots: Vec<Dot>,
    pub dead: bool,
}

pub(super) struct Dot {
    pub dps: f32,
    pub left: f32,
    pub tick: f32,
    /// The hero whose spell laid it.
    pub src: Option<Entity>,
}

/// A corpse playing its death before it fades.
#[derive(Component)]
pub(super) struct Dying(f32);

#[derive(Resource, Default)]
pub(super) struct Spawner {
    timer: f32,
}

/// An enemy died here, worth this much.
#[derive(Message, Clone, Copy)]
pub(super) struct Loot {
    pub pos: Vec3,
    pub xp: u32,
    pub boss: bool,
}

#[derive(Clone, Copy)]
pub(super) enum GemKind {
    Xp(u32),
    Heal(f32),
}

#[derive(Component)]
pub(super) struct Gem {
    kind: GemKind,
    pulled: bool,
    speed: f32,
    /// The hero whose magnet caught it.
    to: Option<Entity>,
}

/// The `GameObjectDisplayInfo` row and scale a pickup wears: a wisp per experience tier, the
/// bigger tiers rarer, and a turkey leg for health.
fn look_for(kind: GemKind) -> (u32, f32) {
    match kind {
        // `TurkeyLeg.mdx`.
        GemKind::Heal(_) => (114, 2.5),
        // `BFD_WispSmallPurple.mdx`.
        GemKind::Xp(xp) if xp >= 20 => (1307, 1.0),
        // `BFD_WispSmallGreen.mdx`.
        GemKind::Xp(xp) if xp >= 5 => (1267, 1.0),
        // `BFD_WispSmall.mdx`.
        GemKind::Xp(_) => (426, 1.0),
    }
}

/// Body radius: the big beasts need more room.
fn radius_for(def: &EnemyDef) -> f32 {
    (if def.big { 1.3 } else { 0.6 }) * def.scale
}

fn spawn_enemy(
    commands: &mut Commands,
    run: &mut Run,
    def: &'static EnemyDef,
    pos: Vec3,
    hero: Vec3,
    boss: bool,
) {
    let scale = data::hp_scale(run.elapsed);
    let serial = run.guid();
    let e = units::spawn_unit(
        commands,
        &UnitSpec {
            serial,
            display: def.display,
            scale: def.scale,
            level: def.level,
            faction: units::FACTION_MONSTER,
            bytes0: 1 << 8,
            weapons: &[],
            sheath: 0,
        },
        pos,
        yaw_toward(hero - pos),
    );
    let hp = def.hp * scale;
    commands.entity(e).insert((
        Enemy {
            def,
            hp,
            max_hp: hp,
            speed: def.speed,
            damage: def.damage * data::damage_scale(run.elapsed),
            radius: radius_for(def),
            xp: def.xp,
            boss,
            attack_cd: 0.5,
            slow: 1.0,
            slow_t: 0.0,
            root_t: 0.0,
            stun_t: 0.0,
            fear_t: 0.0,
            weaken: 1.0,
            weaken_t: 0.0,
            dots: Vec::new(),
            dead: false,
        },
        Marks::debuffs(),
    ));
}

fn ground(world: &WorldPoint, mut p: Vec3, fallback: f32) -> Vec3 {
    p.y = world.terrain_height_under(p).unwrap_or(fallback);
    p
}

/// Keep the horde topped up on a ring outside the view, with the bosses and the encircling waves
/// on the clock.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_enemies(
    mut commands: Commands,
    time: Res<Time>,
    mut run: ResMut<Run>,
    mut rng: ResMut<Rng>,
    mut spawner: ResMut<Spawner>,
    selection: Res<Selection>,
    hero: Query<(&Transform, Option<&Mate>), With<Hero>>,
    enemies: Query<&Enemy>,
    world: WorldPoint,
) {
    // The horde gathers around whoever still stands; in co-op, around one of them at random.
    let standing: Vec<Vec3> = hero
        .iter()
        .filter(|(_, m)| match m {
            Some(m) => m.run.dead_t.is_none(),
            None => run.dead_t.is_none(),
        })
        .map(|(t, _)| t.translation)
        .collect();
    if standing.is_empty() {
        return;
    }
    let pick = |rng: &mut Rng| standing[rng.index(standing.len())];
    let elapsed = run.elapsed;
    let roster = data::roster(selection.map);
    let pool = data::wave_pool(elapsed, roster.enemies.len());

    let due_bosses = ((elapsed / data::BOSS_EVERY_SECS) as u32).min(roster.bosses.len() as u32);
    while run.bosses < due_bosses {
        let def = &roster.bosses[run.bosses as usize];
        let hpos = pick(&mut rng);
        let a = rng.range(0.0, std::f32::consts::TAU);
        let pos = ground(
            &world,
            hpos + Vec3::new(a.cos(), 0.0, a.sin()) * 30.0,
            hpos.y,
        );
        spawn_enemy(&mut commands, &mut run, def, pos, hpos, true);
        run.bosses += 1;
        run.banner = Some((format!("{} approaches!", def.name), 4.0));
    }

    // Half way between bosses, a closing ring of the moment's weakest, around each who stands.
    let due_rings = ((elapsed + data::BOSS_EVERY_SECS / 2.0) / data::BOSS_EVERY_SECS) as u32;
    if run.rings < due_rings {
        run.rings = due_rings;
        let def = &roster.enemies[pool[0]];
        let n = (28 + (data::progress(elapsed) * 60.0) as usize) / standing.len().max(1)
            + (standing.len() - 1) * 6;
        for &hpos in &standing {
            for i in 0..n {
                let a = i as f32 / n as f32 * std::f32::consts::TAU;
                let pos = ground(
                    &world,
                    hpos + Vec3::new(a.cos(), 0.0, a.sin()) * 26.0,
                    hpos.y,
                );
                spawn_enemy(&mut commands, &mut run, def, pos, hpos, false);
            }
        }
        run.banner = Some(("They're surrounding you!".into(), 3.0));
    }

    spawner.timer -= time.delta_secs();
    if spawner.timer > 0.0 {
        return;
    }
    spawner.timer = 0.12;
    let alive = enemies.iter().filter(|e| !e.dead).count();
    // Two heroes draw a bigger horde.
    let target = data::target_alive(elapsed) * (2 + standing.len()) / 3;
    if alive >= target {
        return;
    }
    let n = (((target - alive) as f32) / 6.0).ceil().min(4.0) as usize;
    for _ in 0..n {
        let def = &roster.enemies[pool[rng.index(pool.len())]];
        let hpos = pick(&mut rng);
        let a = rng.range(0.0, std::f32::consts::TAU);
        let r = rng.range(34.0, 42.0);
        let pos = ground(&world, hpos + Vec3::new(a.cos(), 0.0, a.sin()) * r, hpos.y);
        spawn_enemy(&mut commands, &mut run, def, pos, hpos, false);
    }
}

/// The chase: straight at the hero, pushed apart from each other, stopped by crowd control, and
/// swinging when in reach.
#[allow(clippy::type_complexity)]
pub(super) fn move_enemies(
    time: Res<Time>,
    mut run: ResMut<Run>,
    world: WorldPoint,
    mut hero: Query<(Entity, &Transform, Option<&mut Mate>), (With<Hero>, Without<Enemy>)>,
    mut enemies: Query<(Entity, &mut Transform, &mut Enemy, &mut MovementState), Without<Hero>>,
    mut fx: Fx,
    mut hits: MessageWriter<Hit>,
) {
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }
    // Every hero: where, whether fallen, whether faded from the horde's sight.
    let heroes: Vec<(Entity, Vec3, bool, bool)> = hero
        .iter()
        .map(|(e, tf, m)| {
            let r: &Run = match &m {
                Some(m) => &m.run,
                None => &run,
            };
            (e, tf.translation, r.dead_t.is_some(), r.mobile.vanish_t > 0.0)
        })
        .collect();
    if heroes.is_empty() {
        return;
    }

    // Separation over a coarse grid: only neighbours in the same or adjacent cells push.
    const CELL: f32 = 3.0;
    let bodies: Vec<(Entity, Vec3, f32)> = enemies
        .iter()
        .filter(|(_, _, e, _)| !e.dead)
        .map(|(ent, tf, e, _)| (ent, tf.translation, e.radius))
        .collect();
    let key = |p: Vec3| ((p.x / CELL).floor() as i32, (p.z / CELL).floor() as i32);
    let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    for (i, b) in bodies.iter().enumerate() {
        grid.entry(key(b.1)).or_default().push(i);
    }
    let mut push: HashMap<Entity, Vec3> = HashMap::with_capacity(bodies.len());
    for (i, &(ent, pos, r)) in bodies.iter().enumerate() {
        let (cx, cz) = key(pos);
        let mut acc = Vec3::ZERO;
        for dx in -1..=1 {
            for dz in -1..=1 {
                let Some(cell) = grid.get(&(cx + dx, cz + dz)) else {
                    continue;
                };
                for &j in cell {
                    if j == i {
                        continue;
                    }
                    let (_, other, or) = bodies[j];
                    let d = flat(pos - other);
                    let len = d.length();
                    let min = r + or;
                    if len < min && len > 1e-4 {
                        acc += d / len * (min - len);
                    } else if len <= 1e-4 {
                        acc += Vec3::new((i as f32).sin(), 0.0, (i as f32).cos()) * 0.1;
                    }
                }
            }
        }
        push.insert(ent, acc);
    }

    for (ent, mut tf, mut e, mut mv) in &mut enemies {
        let e = &mut *e;
        if e.dead {
            if mv.speed != 0.0 || mv.flags != 0 {
                mv.speed = 0.0;
                mv.flags = 0;
            }
            continue;
        }
        e.attack_cd -= dt;
        for t in [
            &mut e.slow_t,
            &mut e.root_t,
            &mut e.stun_t,
            &mut e.fear_t,
            &mut e.weaken_t,
        ] {
            *t = (*t - dt).max(0.0);
        }
        if e.slow_t <= 0.0 {
            e.slow = 1.0;
        }
        if e.weaken_t <= 0.0 {
            e.weaken = 1.0;
        }
        let pos = tf.translation;
        // The nearest standing hero it can see (a boss sees through Fade); with none it can see,
        // the nearest standing one, which it has lost; with every hero down, the nearest of all.
        let nearest = |want: &dyn Fn(&(Entity, Vec3, bool, bool)) -> bool| {
            heroes
                .iter()
                .filter(|h| want(h))
                .min_by(|a, b| flat(a.1 - pos).length().total_cmp(&flat(b.1 - pos).length()))
                .copied()
        };
        let seen = nearest(&|h| !h.2 && (!h.3 || e.boss));
        let (hero_e, hpos, hero_down, _) = seen
            .or_else(|| nearest(&|h| !h.2))
            .or_else(|| nearest(&|_| true))
            .expect("heroes is not empty");
        let to_hero = flat(hpos - pos);
        let dist = to_hero.length();

        // Strays far behind are brought back in front, as the reference genre does.
        if dist > 75.0 {
            let back = hpos + to_hero.normalize_or_zero() * 38.0;
            tf.translation = ground(&world, back, hpos.y);
            continue;
        }

        // Under Fade the horde has lost the hero: it mills where it stands.
        let lost = seen.is_none() && !hero_down;
        let held = e.stun_t > 0.0 || e.root_t > 0.0 || lost;
        let contact = dist <= HERO_RADIUS + e.radius + 0.35;
        let mut step = Vec3::ZERO;
        let mut speed = 0.0;
        if !held && !hero_down {
            let dir = if e.fear_t > 0.0 {
                -to_hero.normalize_or_zero()
            } else {
                to_hero.normalize_or_zero()
            };
            if !contact || e.fear_t > 0.0 {
                speed = e.speed * e.slow;
                step = dir * speed;
            }
            if dir != Vec3::ZERO {
                let target = Quat::from_rotation_y(yaw_toward(dir));
                tf.rotation = tf.rotation.slerp(target, (dt * 10.0).min(1.0));
            }
        }
        let shove = push.get(&ent).copied().unwrap_or_default();
        let mut next = pos + step * dt + shove * (dt * 8.0).min(0.5);
        next = ground(&world, next, pos.y);
        tf.translation = next;
        if speed > 0.0 {
            mv.speed = speed;
            mv.flags = move_flags::FORWARD;
        } else if mv.speed != 0.0 || mv.flags != 0 {
            mv.speed = 0.0;
            mv.flags = 0;
        }

        if contact
            && !lost
            && e.stun_t <= 0.0
            && e.fear_t <= 0.0
            && e.attack_cd <= 0.0
            && !hero_down
        {
            e.attack_cd = if e.boss { 1.0 } else { 1.3 };
            let raw = e.damage * e.weaken;
            let Ok((_, _, mate)) = hero.get_mut(hero_e) else {
                continue;
            };
            let target: &mut Run = match mate {
                Some(m) => &mut m.into_inner().run,
                None => &mut run,
            };
            let taken = hero::hurt_hero(target, raw);
            fx.swing(ent, hero_e, taken);
            if taken > 0.0 {
                fx.number(hero_e, taken, false, COLOR_HURT);
            }
            if target.thorns_t > 0.0 {
                hits.write(Hit {
                    src: Some(hero_e),
                    ..Hit::direct(ent, target.thorns * target.stats().might, false)
                });
            }
        }
    }
}

/// Mark timers run down; the state kits reap with them.
pub(super) fn tick_marks(
    time: Res<Time>,
    mut units: Query<(Entity, &Guid, &mut ObjectStore, &mut Marks), With<Enemy>>,
    mut fields: MessageWriter<FieldChanged>,
) {
    let dt = time.delta_secs();
    for (entity, guid, mut store, mut marks) in &mut units {
        marks.tick(dt);
        marks.flush(entity, guid.0, &mut store, &mut fields);
    }
}

/// Corpses fade once their death has played.
pub(super) fn reap_dead(
    mut commands: Commands,
    time: Res<Time>,
    mut dying: Query<(Entity, &mut Dying)>,
) {
    for (e, mut d) in &mut dying {
        d.0 += time.delta_secs();
        if d.0 > 2.5 {
            commands
                .entity(e)
                .remove::<Dying>()
                .insert(benilla_world::model_fade::DespawnFade::default());
        }
    }
}

pub(super) fn mark_dying(commands: &mut Commands, e: Entity) {
    commands.entity(e).insert(Dying(0.0));
}

/// Most gems on the ground at once; past it, a kill's worth joins an existing gem.
const MAX_GEMS: usize = 350;

pub(super) fn drop_loot(
    mut commands: Commands,
    mut drops: MessageReader<Loot>,
    mut rng: ResMut<Rng>,
    mut run: ResMut<Run>,
    mut gems: Query<&mut Gem>,
) {
    let mut count = gems.iter().count();
    for d in drops.read() {
        if d.boss || rng.f32() < 0.015 {
            let heal = GemKind::Heal(if d.boss { 1000.0 } else { 30.0 });
            spawn_gem(&mut commands, run.guid(), d.pos + Vec3::X * 0.8, heal);
        }
        if count >= MAX_GEMS && !d.boss {
            if let Some(mut g) = gems.iter_mut().find(|g| matches!(g.kind, GemKind::Xp(_))) {
                if let GemKind::Xp(xp) = &mut g.kind {
                    *xp += d.xp;
                }
                continue;
            }
        }
        spawn_gem(&mut commands, run.guid(), d.pos, GemKind::Xp(d.xp));
        count += 1;
    }
}

fn spawn_gem(commands: &mut Commands, serial: u64, pos: Vec3, kind: GemKind) {
    let (display, scale) = look_for(kind);
    let e = units::spawn_object(commands, serial, display, scale, pos + Vec3::Y * 0.5);
    commands.entity(e).insert(Gem {
        kind,
        pulled: false,
        speed: 6.0,
        to: None,
    });
}

/// Gems in a magnet's reach fly to that hero; experience goes to the team and levels bank as it
/// lands, a health pickup heals whoever caught it.
#[allow(clippy::type_complexity)]
pub(super) fn collect_gems(
    mut commands: Commands,
    time: Res<Time>,
    mut run: ResMut<Run>,
    mut next: ResMut<NextState<Phase>>,
    mut hero: Query<(Entity, &Transform, Option<&mut Mate>), (With<Hero>, Without<Gem>)>,
    mut gems: Query<(Entity, &mut Transform, &mut Gem)>,
) {
    let dt = time.delta_secs();
    // Standing heroes: entity, chest height, magnet reach, growth.
    let catchers: Vec<(Entity, Vec3, f32, f32)> = hero
        .iter()
        .filter_map(|(e, tf, m)| {
            let r: &Run = match &m {
                Some(m) => &m.run,
                None => &run,
            };
            r.dead_t.is_none().then(|| {
                let st = r.stats();
                (e, tf.translation + Vec3::Y * 1.0, st.magnet, st.growth)
            })
        })
        .collect();
    if catchers.is_empty() {
        return;
    }
    let mut landed: Vec<(Entity, GemKind)> = Vec::new();
    for (e, mut tf, mut g) in &mut gems {
        let at = tf.translation;
        if !g.pulled {
            let caught = catchers
                .iter()
                .filter(|c| flat(c.1 - at).length() <= c.2)
                .min_by(|a, b| flat(a.1 - at).length().total_cmp(&flat(b.1 - at).length()));
            if let Some(c) = caught {
                g.pulled = true;
                g.to = Some(c.0);
            }
        }
        if !g.pulled {
            continue;
        }
        // The catcher fell (or left): the nearest one standing takes it over.
        let target = match catchers.iter().find(|c| Some(c.0) == g.to) {
            Some(c) => *c,
            None => {
                let c = *catchers
                    .iter()
                    .min_by(|a, b| (a.1 - at).length().total_cmp(&(b.1 - at).length()))
                    .expect("catchers is not empty");
                g.to = Some(c.0);
                c
            }
        };
        let d = target.1 - at;
        let dist = d.length();
        if dist < 0.9 {
            landed.push((target.0, g.kind));
            commands.entity(e).despawn();
            continue;
        }
        g.speed += 30.0 * dt;
        tf.translation += d / dist * (g.speed * dt).min(dist);
    }
    for (by, kind) in landed {
        let growth = catchers
            .iter()
            .find(|c| c.0 == by)
            .map_or(1.0, |c| c.3);
        match kind {
            GemKind::Xp(xp) => run.xp += xp as f32 * data::XP_RATE * growth,
            GemKind::Heal(hp) => {
                if let Ok((_, _, mate)) = hero.get_mut(by) {
                    let r: &mut Run = match mate {
                        Some(m) => &mut m.into_inner().run,
                        None => &mut run,
                    };
                    r.hp = (r.hp + hp).min(r.stats().max_hp);
                }
            }
        }
    }
    let mut gained = 0;
    while run.xp >= data::xp_to_next(run.level) {
        run.xp -= data::xp_to_next(run.level);
        run.level += 1;
        run.pending_levels += 1;
        gained += 1;
        if run.level == super::mobility::LEARN_LEVEL {
            if let Some(m) = super::mobility::for_class(run.class.id) {
                run.banner = Some((format!("You have learned {}! Press F.", m.name), 4.0));
            }
        }
    }
    // Every teammate picks a card for each level the team gains.
    let mut mates_pending = false;
    for (_, _, mate) in &mut hero {
        if let Some(mut m) = mate {
            m.run.pending_levels += gained;
            m.run.level = run.level;
            m.run.xp = run.xp;
            mates_pending |= m.run.pending_levels > 0;
        }
    }
    if run.pending_levels > 0 || mates_pending {
        next.set(Phase::LevelUp);
    }
}

/// Pickups hang still: a wisp's own loop (its circling motes and their trails) stops on its first
/// frame, clip and global sequences alike, as soon as the model attaches.
#[allow(clippy::type_complexity)]
pub(super) fn still_gems(
    mut players: Query<
        &mut AnimationPlayer,
        (Or<(With<Gem>, With<super::coop::Still>)>, Added<AnimationPlayer>),
    >,
    mut drives: Query<
        &mut GlobalSeqDrive,
        (Or<(With<Gem>, With<super::coop::Still>)>, Added<GlobalSeqDrive>),
    >,
) {
    for mut player in &mut players {
        for (_, active) in player.playing_animations_mut() {
            active.set_speed(0.0);
        }
    }
    for mut drive in &mut drives {
        drive.set_paused(true);
    }
}
