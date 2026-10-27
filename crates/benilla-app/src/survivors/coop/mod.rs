//! Two-player co-op over the internet or a LAN: one player hosts (`--host`), the other joins
//! (`--join <ip>`). The host's game is the real one: it runs the horde, the spells, the gems and
//! the level-ups for both heroes, and streams what changed to the guest. The guest drives only its
//! own hero and sends where it stands; everything else it shows is the host's.
//!
//! Without `--host` or `--join` none of this is present and the mode is the single-player game.

mod wire;

use std::collections::HashMap;

use benilla_protocol::messages::{ObjectFields, ObjectType};
use benilla_world::model_fade::DespawnFade;
use benilla_world::schedule::WorldStage;
use bevy::prelude::*;

use super::data;
use super::enemies::{Enemy, Gem};
use super::hero::{self, Hero};
use super::hud::{self, Choice, HudFont, LevelUpSub, Offer};
use super::units::{self, Fx, FxLog, FxRec};
use super::{Phase, Rng, Run, RunEntity, Selection};
use crate::creature_anim::MovementState;
use crate::net::{FieldChanged, Guid, NetEntity, ObjectStore};
use wire::{
    decode_guest, decode_host, encode_guest, encode_host, Body, CardW, HudW, Link, LinkEvent,
    Motion, Spawn, ToGuest, ToHost, PROTOCOL,
};

pub(super) const REQUEST_PAUSE: u8 = 0;
pub(super) const REQUEST_RESUME: u8 = 1;

/// The simulation chain; the host copies the team's numbers into its guest's run before it.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct Simulation;

/// A replicated pickup whose own animation holds still, as a gem's does on the host.
#[derive(Component)]
pub(super) struct Still;

/// On the host, the guest's hero: its own run (health, spells, cards) and what its player sent.
#[derive(Component)]
pub(super) struct Mate {
    pub(super) run: Run,
    /// The ground under the guest's cursor, for its aimed mobility spell.
    pub(super) aim: Option<Vec3>,
    /// The guest pressed its mobility key.
    pub(super) cast: bool,
    /// A Blink or Charge moved the hero here: the guest is told, and its stale positions are
    /// ignored for a moment.
    pub(super) teleport: Option<(Vec3, f32)>,
    hold_t: f32,
    /// The guest is still behind its loading screen; the run waits for it.
    pub(super) loading: bool,
    since: f32,
    /// The guest's cards on offer, and whether they went out.
    offer: Vec<Choice>,
    offer_sent: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Host,
    Guest,
}

/// The guest's pick at its menu, as the host keeps it.
#[derive(Clone, Copy, Debug)]
struct PeerPick {
    class: usize,
    display: u32,
    race: u8,
    sex: u8,
}

#[derive(Resource)]
pub(super) struct Coop {
    role: Role,
    link: Link,
    /// Guest: where it joins. Host: the port it listens on, for the status line.
    addr: String,
    /// The TCP link is up.
    up: bool,
    /// The two sides said hello on the same protocol.
    welcomed: bool,
    /// Guest: it left the session (or was refused); it plays alone from here.
    left: bool,
    status: String,
    /// Host: the guest's hero pick.
    peer: Option<PeerPick>,
    /// Host: a run is going.
    in_run: bool,
    /// Host: the guest is part of this run, so the world streams to it.
    replicating: bool,
    /// Guest: it is in the host's run.
    joined: bool,
    host_in_run: bool,
    host_pick: String,
    peer_hp: f32,
    peer_max: f32,
    peer_dead: bool,
    peer_revive: f32,
    /// Guest: when to try the host again.
    retry_at: Option<f32>,
    /// Guest: the last pick sent, by [`Selection::version`].
    sent_pick: Option<u32>,
    /// Host: what goes out this frame.
    out: Vec<ToGuest>,
    /// Host: the link could not keep up.
    dropped: Option<String>,
}

impl Coop {
    fn new(role: Role, link: Link, addr: String, status: String) -> Self {
        Self {
            role,
            link,
            addr,
            up: false,
            welcomed: false,
            left: false,
            status,
            peer: None,
            in_run: false,
            replicating: false,
            joined: false,
            host_in_run: false,
            host_pick: String::new(),
            peer_hp: 0.0,
            peer_max: 0.0,
            peer_dead: false,
            peer_revive: 0.0,
            retry_at: None,
            sent_pick: None,
            out: Vec::new(),
            dropped: None,
        }
    }

    /// This game is a co-op guest: it shows the host's run instead of running its own.
    pub(super) fn is_guest(&self) -> bool {
        self.role == Role::Guest && !self.left
    }

    /// Guest: ask the host to pause ([`REQUEST_PAUSE`]) or resume ([`REQUEST_RESUME`]).
    pub(super) fn request(&self, kind: u8) {
        self.to_host(ToHost::Request(kind));
    }

    /// Guest: the card it chose.
    pub(super) fn send_card(&self, i: usize) {
        self.to_host(ToHost::Card(i.min(u8::MAX as usize) as u8));
    }

    /// Guest: leave the host's game and play alone.
    pub(super) fn leave(&mut self) {
        self.to_host(ToHost::Bye);
        self.left = true;
        self.joined = false;
        self.status = "You left the co-op game. Start the game with Join again to rejoin.".into();
    }

    fn to_host(&self, m: ToHost) {
        if self.up {
            let _ = self.link.send(encode_host(&[m]));
        }
    }
}

/// Simulation runs here: everywhere but on a co-op guest.
pub(super) fn not_guest(coop: Option<Res<Coop>>) -> bool {
    !coop.is_some_and(|c| c.is_guest())
}

/// The run waits while a co-op guest is still loading.
pub(super) fn mates_ready(mates: Query<&Mate>) -> bool {
    mates.iter().all(|m| !m.loading)
}

fn is_host(coop: Option<Res<Coop>>) -> bool {
    coop.is_some_and(|c| c.role == Role::Host)
}

fn is_guest(coop: Option<Res<Coop>>) -> bool {
    coop.is_some_and(|c| c.is_guest())
}

/// `--host [port]` or `--join <ip[:port]>` on the command line (or `WOW_COOP=host[:port]` /
/// `WOW_COOP=join:<ip[:port]>`).
fn from_args() -> Option<(Role, String)> {
    #[cfg(target_arch = "wasm32")]
    {
        None
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        for (i, a) in args.iter().enumerate() {
            let next = args.get(i + 1).filter(|n| !n.starts_with("--")).cloned();
            match a.as_str() {
                "--host" => return Some((Role::Host, next.unwrap_or_default())),
                "--join" => return next.map(|n| (Role::Guest, n)),
                _ => {}
            }
        }
        let env = std::env::var("WOW_COOP").ok()?;
        if let Some(rest) = env.strip_prefix("join:") {
            return Some((Role::Guest, rest.to_string()));
        }
        if let Some(rest) = env.strip_prefix("host") {
            return Some((Role::Host, rest.trim_start_matches(':').to_string()));
        }
        None
    }
}

pub(super) fn plugin(app: &mut App) {
    let Some((role, arg)) = from_args() else {
        return;
    };
    let coop = match role {
        Role::Host => {
            let port = arg.parse().unwrap_or(wire::DEFAULT_PORT);
            let ip = wire::local_ip().unwrap_or_else(|| "your IP".into());
            app.init_resource::<FxLog>();
            Coop::new(
                Role::Host,
                wire::host(port),
                port.to_string(),
                waiting_text(&ip, port),
            )
        }
        Role::Guest => Coop::new(
            Role::Guest,
            wire::join(arg.clone()),
            arg.clone(),
            format!("Connecting to {arg}…"),
        ),
    };
    info!("survivors co-op: {:?} ({})", coop.role, coop.addr);
    app.insert_resource(coop)
        .init_resource::<Replica>()
        .init_resource::<HostRepl>()
        .configure_sets(Update, Simulation.after(WorldStage::Input))
        .add_systems(
            Update,
            (host_link, host_start)
                .chain()
                .run_if(is_host)
                .before(WorldStage::Input),
        )
        .add_systems(
            Update,
            sync_mates
                .run_if(is_host)
                .after(WorldStage::Input)
                .before(Simulation),
        )
        .add_systems(
            Update,
            guest_link.run_if(is_guest).before(WorldStage::Input),
        )
        .add_systems(
            Update,
            (guest_input, guest_smooth)
                .run_if(is_guest)
                .after(WorldStage::Input),
        )
        .add_systems(Update, coop_ui.after(WorldStage::Input))
        .add_systems(OnEnter(Phase::Menu), back_at_menu)
        .add_systems(
            PostUpdate,
            (host_replicate, host_fx, host_hud, host_flush)
                .chain()
                .run_if(is_host),
        );
}

fn waiting_text(ip: &str, port: u16) -> String {
    format!(
        "Hosting co-op on port {port}. Your friend joins with your IP: {ip} \
         (or your Tailscale / ZeroTier IP). Waiting for them…"
    )
}

fn phase_code(p: Phase) -> u8 {
    match p {
        Phase::Menu => 0,
        Phase::Playing => 1,
        Phase::LevelUp => 2,
        Phase::Paused => 3,
        Phase::GameOver => 4,
    }
}

fn phase_from(c: u8) -> Phase {
    match c {
        1 => Phase::Playing,
        2 => Phase::LevelUp,
        3 => Phase::Paused,
        4 => Phase::GameOver,
        _ => Phase::Menu,
    }
}

/// "Orc Warrior": a pick, for the other player's status line.
fn pick_name(race: u8, class: &data::ClassDef) -> String {
    format!("{} {}", crate::char_select::race_name(race), class.name)
}

fn selection_name(sel: &Selection) -> String {
    match (
        sel.class_def(),
        sel.look.and_then(|i| sel.looks.get(i)).copied(),
    ) {
        (Some(c), Some((_, race, _))) => pick_name(race, c),
        _ => "a hero".into(),
    }
}

fn yaw_of(tf: &Transform) -> f32 {
    units::yaw_toward(tf.rotation * Vec3::NEG_Z)
}

fn card_out(c: Choice) -> CardW {
    match c {
        Choice::NewAbility(def) => CardW::NewAbility(def.key.to_string()),
        Choice::Upgrade(k) => CardW::Upgrade(k.min(255) as u8),
        Choice::NewPassive(p) => CardW::NewPassive(p.key.to_string()),
        Choice::UpPassive(k) => CardW::UpPassive(k.min(255) as u8),
        Choice::Heal => CardW::Heal,
    }
}

fn card_in(c: &CardW, run: &Run) -> Option<Choice> {
    Some(match c {
        CardW::NewAbility(k) => {
            Choice::NewAbility(data::ABILITIES.iter().find(|a| a.key == k.as_str())?)
        }
        CardW::Upgrade(i) => {
            let i = usize::from(*i);
            (i < run.abilities.len()).then_some(Choice::Upgrade(i))?
        }
        CardW::NewPassive(k) => {
            Choice::NewPassive(data::PASSIVES.iter().find(|p| p.key == k.as_str())?)
        }
        CardW::UpPassive(i) => {
            let i = usize::from(*i);
            (i < run.passives.len()).then_some(Choice::UpPassive(i))?
        }
        CardW::Heal => Choice::Heal,
    })
}

// ── Host ──────────────────────────────────────────────────────────────────────────────────────

/// The guest's messages: hello, its pick, where its hero stands, its keys and cards.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn host_link(
    mut commands: Commands,
    mut coop: ResMut<Coop>,
    phase: Res<State<Phase>>,
    mut next: ResMut<NextState<Phase>>,
    mut run: ResMut<Run>,
    mut rng: ResMut<Rng>,
    mut mates: Query<(Entity, &mut Transform, &mut MovementState, &mut Mate)>,
) {
    let mut events: Vec<LinkEvent> = Vec::new();
    while let Ok(ev) = coop.link.events.try_recv() {
        events.push(ev);
    }
    if let Some(why) = coop.dropped.take() {
        events.push(LinkEvent::Down(why));
    }
    let p = *phase.get();
    for ev in events {
        let msgs = match ev {
            LinkEvent::Up(ip) => {
                coop.up = true;
                coop.welcomed = false;
                coop.status = format!("A friend is connecting from {ip}…");
                continue;
            }
            LinkEvent::Failed(why) => {
                coop.status = format!("Co-op could not start: {why}");
                continue;
            }
            LinkEvent::Down(why) => {
                let was = coop.welcomed;
                coop.up = false;
                coop.welcomed = false;
                coop.peer = None;
                coop.replicating = false;
                for (e, ..) in &mates {
                    commands.entity(e).try_despawn();
                }
                if was && p != Phase::Menu {
                    run.banner = Some(("Your friend left the game.".into(), 4.0));
                }
                // A level-up that waited on the guest's card goes on.
                if p == Phase::LevelUp && run.pending_levels == 0 {
                    next.set(Phase::Playing);
                }
                let ip = wire::local_ip().unwrap_or_else(|| "your IP".into());
                coop.status = format!(
                    "Your friend disconnected ({why}). {}",
                    waiting_text(&ip, coop.addr.parse().unwrap_or(wire::DEFAULT_PORT))
                );
                continue;
            }
            LinkEvent::Frame(f) => decode_host(&f),
        };
        for m in msgs {
            if !coop.welcomed && !matches!(m, ToHost::Hello { .. }) {
                continue;
            }
            match m {
                ToHost::Hello { protocol } => {
                    if protocol != PROTOCOL {
                        coop.out.push(ToGuest::Reject(
                            "The host runs a different version of the game. Use the same exe."
                                .into(),
                        ));
                        coop.status =
                            "A friend tried to join with a different version of the game.".into();
                    } else {
                        coop.welcomed = true;
                        coop.out.push(ToGuest::Welcome {
                            protocol: PROTOCOL,
                            host: String::new(),
                        });
                        coop.status = "A friend joined and is picking a hero…".into();
                    }
                }
                ToHost::Pick {
                    class,
                    display,
                    race,
                    sex,
                } => {
                    let class = usize::from(class);
                    if class < data::CLASSES.len() {
                        coop.peer = Some(PeerPick {
                            class,
                            display,
                            race,
                            sex,
                        });
                        coop.status = format!(
                            "{} joined! Pick a zone and press Enter World to play together.",
                            pick_name(race, &data::CLASSES[class])
                        );
                    }
                }
                ToHost::Input {
                    motion,
                    aim,
                    loading,
                } => {
                    for (_, mut tf, mut mv, mut mate) in &mut mates {
                        if mate.hold_t <= 0.0 {
                            tf.translation = motion.pos;
                            tf.rotation = Quat::from_rotation_y(motion.yaw);
                        }
                        mv.speed = motion.speed;
                        mv.flags = motion.flags;
                        mv.vertical_speed = motion.vy;
                        mate.aim = aim;
                        mate.loading = loading;
                    }
                }
                ToHost::Mobility => {
                    for (.., mut mate) in &mut mates {
                        mate.cast = true;
                    }
                }
                ToHost::Card(i) => {
                    for (.., mut mate) in &mut mates {
                        let m = &mut *mate;
                        let Some(&choice) = m.offer.get(usize::from(i)) else {
                            continue;
                        };
                        hud::apply_choice(&mut m.run, choice);
                        m.offer.clear();
                        m.offer_sent = false;
                        if m.run.pending_levels > 0 {
                            m.offer = hud::choices_for(&m.run, &mut rng);
                        }
                    }
                    let mates_done = mates.iter().all(|(.., m)| m.run.pending_levels == 0);
                    if p == Phase::LevelUp && run.pending_levels == 0 && mates_done {
                        next.set(Phase::Playing);
                    }
                }
                ToHost::Request(k) => match (k, p) {
                    (REQUEST_PAUSE, Phase::Playing) => next.set(Phase::Paused),
                    (REQUEST_PAUSE | REQUEST_RESUME, Phase::Paused) => next.set(Phase::Playing),
                    _ => {}
                },
                ToHost::Bye => coop.dropped = Some("they left".into()),
            }
        }
    }
}

/// A run began: the guest's hero joins it beside the host's, and the guest is told to enter.
#[allow(clippy::too_many_arguments)]
fn host_start(
    mut commands: Commands,
    mut coop: ResMut<Coop>,
    phase: Res<State<Phase>>,
    mut run: ResMut<Run>,
    selection: Res<Selection>,
) {
    if *phase.get() == Phase::Menu || coop.in_run {
        return;
    }
    coop.in_run = true;
    let Some(p) = coop.peer.filter(|_| coop.welcomed) else {
        return;
    };
    let class = &data::CLASSES[p.class];
    let pos = hero::arena(selection.stage_def()) + Vec3::new(2.5, 0.0, 0.0);
    let serial = run.guid();
    let e = hero::spawn_hero(&mut commands, class, p.display, p.race, p.sex, serial, pos);
    commands.entity(e).insert(Mate {
        run: hud::fresh_run(class, p.race),
        aim: None,
        cast: false,
        teleport: None,
        hold_t: 0.0,
        loading: true,
        since: 0.0,
        offer: Vec::new(),
        offer_sent: false,
    });
    coop.replicating = true;
    coop.out.push(ToGuest::Start {
        map: selection.stage.min(u16::MAX as usize) as u16,
        serial,
        pos,
    });
}

/// The team's numbers (clock, kills, level, experience) into the guest's run, before the
/// simulation reads it.
fn sync_mates(time: Res<Time<Real>>, run: Res<Run>, mut mates: Query<&mut Mate>) {
    let dt = time.delta_secs();
    for mut m in &mut mates {
        let m = &mut *m;
        m.run.elapsed = run.elapsed;
        m.run.kills = run.kills;
        m.run.level = run.level;
        m.run.xp = run.xp;
        m.run.bosses = run.bosses;
        m.run.rings = run.rings;
        m.run.won = run.won;
        m.run.next_guid = run.next_guid;
        m.hold_t = (m.hold_t - dt).max(0.0);
        // A guest that never reports in does not hold the run forever.
        m.since += dt;
        if m.since > 45.0 {
            m.loading = false;
        }
    }
}

/// What the guest has been sent, per host entity.
#[derive(Resource, Default)]
struct HostRepl {
    known: HashMap<Entity, Known>,
    move_t: f32,
}

struct Known {
    guid: u64,
    fields: Vec<(u16, u32)>,
    motion: Motion,
}

fn diff_fields(old: &[(u16, u32)], now: &[(u16, u32)]) -> Vec<(u16, u32)> {
    let before: HashMap<u16, u32> = old.iter().copied().collect();
    now.iter()
        .filter(|(i, v)| before.get(i) != Some(v))
        .copied()
        .collect()
}

fn moved(a: &Motion, b: &Motion) -> bool {
    a.pos.distance_squared(b.pos) > 0.0004
        || (a.yaw - b.yaw).abs() > 0.02
        || a.speed != b.speed
        || a.flags != b.flags
        || a.vy != b.vy
}

/// The world, streamed: new objects, changed fields, motion (20 times a second), fades and
/// removals.
#[allow(clippy::type_complexity)]
fn host_replicate(
    time: Res<Time<Real>>,
    mut coop: ResMut<Coop>,
    mut repl: ResMut<HostRepl>,
    objs: Query<
        (
            Entity,
            &Guid,
            &NetEntity,
            &Transform,
            Ref<ObjectStore>,
            Option<&MovementState>,
            Option<&Enemy>,
            Has<Gem>,
            Has<Mate>,
        ),
        With<RunEntity>,
    >,
    fades: Query<Entity, Added<DespawnFade>>,
) {
    if !(coop.replicating && coop.welcomed) {
        repl.known.clear();
        return;
    }
    let repl = &mut *repl;
    repl.move_t -= time.delta_secs();
    let moves_due = repl.move_t <= 0.0;
    if moves_due {
        repl.move_t = 0.05;
    }
    let mut out = Vec::new();
    let gone: Vec<Entity> = repl
        .known
        .keys()
        .filter(|e| !objs.contains(**e))
        .copied()
        .collect();
    for e in gone {
        if let Some(k) = repl.known.remove(&e) {
            out.push(ToGuest::Despawn(k.guid));
        }
    }
    let mut motions = Vec::new();
    for (e, guid, net, tf, store, mv, enemy, gem, mate) in &objs {
        let motion = Motion {
            guid: guid.0,
            pos: tf.translation,
            yaw: yaw_of(tf),
            speed: mv.map_or(0.0, |m| m.speed),
            flags: mv.map_or(0, |m| m.flags),
            vy: mv.map_or(0.0, |m| m.vertical_speed),
        };
        match repl.known.get_mut(&e) {
            None => {
                let fields: Vec<(u16, u32)> = store.0.raw_fields().collect();
                if mate {
                    // The guest built its own hero under this guid; only the fields go.
                    out.push(ToGuest::Fields {
                        guid: guid.0,
                        pairs: fields.clone(),
                    });
                } else {
                    let body = match net.kind {
                        benilla_protocol::EntityKind::GameObject => Body::GameObject,
                        benilla_protocol::EntityKind::DynamicObject => Body::Dynamic,
                        _ => Body::Unit,
                    };
                    out.push(ToGuest::Spawn(Spawn {
                        guid: guid.0,
                        body,
                        display: net.display_id,
                        scale: net.scale,
                        pos: tf.translation,
                        yaw: motion.yaw,
                        enemy: enemy.is_some(),
                        boss: enemy.is_some_and(|e| e.boss),
                        still: gem,
                        fields: fields.clone(),
                    }));
                }
                repl.known.insert(
                    e,
                    Known {
                        guid: guid.0,
                        fields,
                        motion,
                    },
                );
            }
            Some(k) => {
                if store.is_changed() {
                    let now: Vec<(u16, u32)> = store.0.raw_fields().collect();
                    let pairs = diff_fields(&k.fields, &now);
                    if !pairs.is_empty() {
                        out.push(ToGuest::Fields {
                            guid: guid.0,
                            pairs,
                        });
                    }
                    k.fields = now;
                }
                if moves_due && !mate && moved(&k.motion, &motion) {
                    motions.push(motion);
                    k.motion = motion;
                }
            }
        }
    }
    if !motions.is_empty() {
        out.push(ToGuest::Moves(motions));
    }
    for e in &fades {
        if let Some(k) = repl.known.get(&e) {
            out.push(ToGuest::Fade(k.guid));
        }
    }
    coop.out.extend(out);
}

/// The casts, impacts, swings and numbers [`Fx`] played this frame.
fn host_fx(mut coop: ResMut<Coop>, mut log: ResMut<FxLog>, guids: Query<&Guid>) {
    let recs = std::mem::take(&mut log.0);
    if !(coop.replicating && coop.welcomed) {
        return;
    }
    let g = |e: Entity| guids.get(e).ok().map(|g| g.0);
    let mut out = Vec::with_capacity(recs.len());
    for r in recs {
        match r {
            FxRec::Go {
                caster,
                spell,
                hits,
                dest,
            } => {
                if let Some(caster) = g(caster) {
                    out.push(ToGuest::Go {
                        caster,
                        spell,
                        hits: hits.into_iter().filter_map(g).collect(),
                        dest,
                    });
                }
            }
            FxRec::Impact { target, spell } => {
                if let Some(target) = g(target) {
                    out.push(ToGuest::Impact { target, spell });
                }
            }
            FxRec::Swing {
                attacker,
                victim,
                damage,
            } => {
                if let (Some(attacker), Some(victim)) = (g(attacker), g(victim)) {
                    out.push(ToGuest::Swing {
                        attacker,
                        victim,
                        damage,
                    });
                }
            }
            FxRec::Number {
                anchor,
                amount,
                crit,
                color,
            } => {
                if let Some(anchor) = g(anchor) {
                    out.push(ToGuest::Number {
                        anchor,
                        amount,
                        crit,
                        color,
                    });
                }
            }
        }
    }
    coop.out.extend(out);
}

#[derive(Default)]
struct HudLast {
    phase: Option<Phase>,
    tick: u32,
    banner: Option<String>,
    lobby: Option<(String, bool)>,
}

/// The screen, the guest's own run, its cards, the host's announcements, and any Blink.
#[allow(clippy::too_many_arguments)]
fn host_hud(
    mut coop: ResMut<Coop>,
    phase: Res<State<Phase>>,
    run: Res<Run>,
    selection: Res<Selection>,
    mut mates: Query<&mut Mate>,
    mut rng: ResMut<Rng>,
    mut last: Local<HudLast>,
) {
    if !coop.welcomed {
        *last = HudLast::default();
        return;
    }
    let p = *phase.get();
    let lobby = (
        selection_name(&selection),
        coop.in_run && !coop.replicating,
    );
    if last.lobby.as_ref() != Some(&lobby) {
        coop.out.push(ToGuest::Lobby {
            in_run: lobby.1,
            host_pick: lobby.0.clone(),
        });
        last.lobby = Some(lobby);
    }
    if coop.replicating || p == Phase::Menu {
        last.tick = last.tick.wrapping_add(1);
        if last.phase != Some(p) || last.tick % 30 == 0 {
            coop.out.push(ToGuest::Phase(phase_code(p)));
            last.phase = Some(p);
        }
    }
    if !coop.replicating {
        return;
    }
    let host_max = run.stats().max_hp;
    for mut m in &mut mates {
        let m = &mut *m;
        if p == Phase::LevelUp && m.run.pending_levels > 0 && m.offer.is_empty() {
            m.offer = hud::choices_for(&m.run, &mut rng);
            m.offer_sent = false;
        }
        let r = &m.run;
        coop.out.push(ToGuest::Hud(Box::new(HudW {
            elapsed: r.elapsed,
            kills: r.kills,
            level: r.level,
            xp: r.xp,
            hp: r.hp,
            abilities: r
                .abilities
                .iter()
                .map(|o| (o.def.key.to_string(), o.level, o.cd))
                .collect(),
            passives: r
                .passives
                .iter()
                .map(|(p, l)| (p.key.to_string(), *l))
                .collect(),
            shield: r.shield,
            shield_t: r.shield_t,
            immune_t: r.immune_t,
            thorns_t: r.thorns_t,
            heal_t: r.heal_t,
            mob_cd: r.mobile.cd,
            mob_haste: r.mobile.haste,
            mob_haste_t: r.mobile.haste_t,
            mob_vanish_t: r.mobile.vanish_t,
            pending: r.pending_levels,
            bosses: r.bosses,
            won: r.won,
            dead_t: r.dead_t,
            revive: r.revive,
            damage_done: r.damage_done,
            peer_hp: run.hp,
            peer_max: host_max,
            peer_dead: run.dead_t.is_some(),
            peer_revive: run.revive,
        })));
        if !m.offer.is_empty() && !m.offer_sent {
            coop.out
                .push(ToGuest::Offer(m.offer.iter().map(|&c| card_out(c)).collect()));
            m.offer_sent = true;
        }
        if let Some((pos, yaw)) = m.teleport.take() {
            coop.out.push(ToGuest::Teleport { pos, yaw });
            m.hold_t = 0.5;
        }
    }
    let banner = run.banner.as_ref().map(|(t, _)| t.clone());
    if banner != last.banner {
        if let Some((text, secs)) = &run.banner {
            // The guest announces its own mobility spell when its own run learns it.
            if !text.starts_with("You have learned") {
                coop.out.push(ToGuest::Banner {
                    text: text.clone(),
                    secs: *secs,
                });
            }
        }
        last.banner = banner;
    }
}

fn host_flush(mut coop: ResMut<Coop>) {
    if coop.out.is_empty() {
        return;
    }
    let msgs = std::mem::take(&mut coop.out);
    if !coop.up {
        return;
    }
    if !coop.link.send(encode_guest(&msgs)) {
        coop.dropped = Some("the connection could not keep up".into());
    }
}

// ── Guest ─────────────────────────────────────────────────────────────────────────────────────

/// The guest's copies of the host's objects, by guid.
#[derive(Resource, Default)]
struct Replica {
    by_guid: HashMap<u64, Entity>,
    /// The guest's own hero.
    own: Option<u64>,
    /// Updates for objects spawned this frame, retried next frame once they exist.
    deferred: Vec<ToGuest>,
}

/// Where the host last put a replicated object; the guest eases toward it.
#[derive(Component)]
struct NetTarget {
    pos: Vec3,
    yaw: f32,
}

fn merge_fields(
    store: &mut ObjectStore,
    entity: Entity,
    guid: u64,
    pairs: &[(u16, u32)],
    fields: &mut MessageWriter<FieldChanged>,
) {
    let kind = store.0.object_type().unwrap_or(ObjectType::Unit);
    let mut edges = Vec::new();
    store
        .0
        .merge_diff(ObjectFields::from_pairs(pairs), |index, old, new| {
            edges.push((index, old, new));
        });
    for (index, old, new) in edges {
        fields.write(FieldChanged {
            entity,
            guid,
            kind,
            index,
            old,
            new,
        });
    }
}

fn apply_hud(run: &mut Run, coop: &mut Coop, h: &HudW) {
    let before = run.level;
    run.elapsed = h.elapsed;
    run.kills = h.kills;
    run.level = h.level;
    run.xp = h.xp;
    run.hp = h.hp;
    let same_abilities = run.abilities.len() == h.abilities.len()
        && run
            .abilities
            .iter()
            .zip(&h.abilities)
            .all(|(o, (k, l, _))| o.def.key == k.as_str() && o.level == *l);
    if same_abilities {
        for (o, (_, _, cd)) in run.abilities.iter_mut().zip(&h.abilities) {
            o.cd = *cd;
        }
    } else {
        run.abilities = h
            .abilities
            .iter()
            .filter_map(|(k, l, cd)| {
                let def = data::ABILITIES.iter().find(|a| a.key == k.as_str())?;
                Some(super::Owned {
                    def,
                    level: *l,
                    cd: *cd,
                })
            })
            .collect();
        run.version += 1;
    }
    let same_passives = run.passives.len() == h.passives.len()
        && run
            .passives
            .iter()
            .zip(&h.passives)
            .all(|((p, l), (k, hl))| p.key == k.as_str() && l == hl);
    if !same_passives {
        run.passives = h
            .passives
            .iter()
            .filter_map(|(k, l)| Some((data::PASSIVES.iter().find(|p| p.key == k.as_str())?, *l)))
            .collect();
        run.version += 1;
    }
    run.shield = h.shield;
    run.shield_t = h.shield_t;
    run.immune_t = h.immune_t;
    run.thorns_t = h.thorns_t;
    run.heal_t = h.heal_t;
    run.mobile.cd = h.mob_cd;
    run.mobile.haste = h.mob_haste;
    run.mobile.haste_t = h.mob_haste_t;
    run.mobile.vanish_t = h.mob_vanish_t;
    run.pending_levels = h.pending;
    run.bosses = h.bosses;
    run.won = h.won;
    run.dead_t = h.dead_t;
    run.revive = h.revive;
    run.damage_done = h.damage_done;
    coop.peer_hp = h.peer_hp;
    coop.peer_max = h.peer_max;
    coop.peer_dead = h.peer_dead;
    coop.peer_revive = h.peer_revive;
    let learn = super::mobility::LEARN_LEVEL;
    if before < learn && run.level >= learn {
        if let Some(m) = super::mobility::for_class(run.class.id) {
            run.banner = Some((format!("You have learned {}! Press F.", m.name), 4.0));
        }
    }
}

/// Everything the host sent: the screen, the world, the effects and the guest's own run.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn guest_link(
    mut commands: Commands,
    mut coop: ResMut<Coop>,
    mut replica: ResMut<Replica>,
    time: Res<Time<Real>>,
    phase: Res<State<Phase>>,
    mut next: ResMut<NextState<Phase>>,
    mut run: ResMut<Run>,
    mut selection: ResMut<Selection>,
    mut offer: ResMut<Offer>,
    mut objs: Query<(
        &mut Transform,
        Option<&mut MovementState>,
        &mut ObjectStore,
        Option<&mut Enemy>,
        Option<&mut NetTarget>,
    )>,
    heroes: Query<Entity, With<Hero>>,
    mut fx: Fx,
    mut fields: MessageWriter<FieldChanged>,
) {
    let now = time.elapsed_secs();
    let p = *phase.get();
    if !coop.up && coop.retry_at.is_some_and(|t| now >= t) && p == Phase::Menu {
        coop.retry_at = None;
        coop.link = wire::join(coop.addr.clone());
        coop.status = format!("Connecting to {}…", coop.addr);
    }
    let mut msgs: Vec<ToGuest> = std::mem::take(&mut replica.deferred);
    while let Ok(ev) = coop.link.events.try_recv() {
        match ev {
            LinkEvent::Up(a) => {
                coop.up = true;
                coop.status = format!("Connected to {a}, saying hello…");
                coop.to_host(ToHost::Hello { protocol: PROTOCOL });
            }
            LinkEvent::Frame(f) => msgs.extend(decode_guest(&f)),
            LinkEvent::Down(why) | LinkEvent::Failed(why) => {
                coop.up = false;
                coop.welcomed = false;
                coop.sent_pick = None;
                if coop.joined {
                    coop.joined = false;
                    run.banner = Some(("Lost the connection to the host.".into(), 4.0));
                    next.set(Phase::Menu);
                }
                if !coop.left {
                    coop.status = format!("{why}. Trying again in a few seconds…");
                    coop.retry_at = Some(now + 5.0);
                }
            }
        }
    }
    if coop.left {
        return;
    }

    for m in msgs {
        match m {
            ToGuest::Welcome { .. } => {
                coop.welcomed = true;
                coop.sent_pick = None;
                coop.status =
                    "Connected! Pick your hero — the host picks the zone and starts the run."
                        .into();
            }
            ToGuest::Reject(why) => {
                coop.status = format!("The host turned us away: {why}");
                coop.left = true;
            }
            ToGuest::Lobby { in_run, host_pick } => {
                coop.host_in_run = in_run;
                coop.host_pick = host_pick;
            }
            ToGuest::Phase(c) => {
                let want = phase_from(c);
                if want == Phase::Menu {
                    if coop.joined {
                        coop.joined = false;
                    }
                } else if !coop.joined {
                    continue;
                }
                if want != p {
                    next.set(want);
                }
            }
            ToGuest::Start { map, serial, pos } => {
                selection.map = usize::from(map).min(data::MAPS.len() - 1);
                let look = selection.look.and_then(|i| selection.looks.get(i)).copied();
                let (Some(class), Some((display, race, sex))) = (selection.class_def(), look)
                else {
                    coop.status = "Pick a hero first, then ask the host to start again.".into();
                    continue;
                };
                if !hud::start_run(&mut run, &mut selection) {
                    continue;
                }
                for e in &heroes {
                    commands.entity(e).try_despawn();
                }
                for (_, e) in replica.by_guid.drain() {
                    commands.entity(e).try_despawn();
                }
                let e = hero::spawn_hero(&mut commands, class, display, race, sex, serial, pos);
                let guid = units::creature_guid(serial);
                replica.by_guid.insert(guid, e);
                replica.own = Some(guid);
                coop.joined = true;
                next.set(Phase::Playing);
            }
            ToGuest::Spawn(s) => {
                if !coop.joined {
                    continue;
                }
                if let Some(&e) = replica.by_guid.get(&s.guid) {
                    match objs.get_mut(e) {
                        Ok((_, _, mut store, ..)) => {
                            merge_fields(&mut store, e, s.guid, &s.fields, &mut fields);
                        }
                        Err(_) => replica.deferred.push(ToGuest::Fields {
                            guid: s.guid,
                            pairs: s.fields,
                        }),
                    }
                    continue;
                }
                let kind = match s.body {
                    Body::Unit => benilla_protocol::EntityKind::Unit,
                    Body::GameObject => benilla_protocol::EntityKind::GameObject,
                    Body::Dynamic => benilla_protocol::EntityKind::DynamicObject,
                };
                let e = units::spawn_replica(
                    &mut commands,
                    s.guid,
                    kind,
                    s.display,
                    s.scale,
                    &s.fields,
                    s.pos,
                    s.yaw,
                );
                let mut ec = commands.entity(e);
                ec.insert(NetTarget {
                    pos: s.pos,
                    yaw: s.yaw,
                });
                if s.still {
                    ec.insert(Still);
                }
                if s.enemy {
                    // Only the minimap reads it here: a blip, a boss's or not.
                    ec.insert(Enemy {
                        def: &data::ENEMIES[0],
                        hp: 1.0,
                        max_hp: 1.0,
                        speed: 0.0,
                        damage: 0.0,
                        radius: 0.6,
                        xp: 0,
                        boss: s.boss,
                        attack_cd: 0.0,
                        slow: 1.0,
                        slow_t: 0.0,
                        root_t: 0.0,
                        stun_t: 0.0,
                        fear_t: 0.0,
                        weaken: 1.0,
                        weaken_t: 0.0,
                        dots: Vec::new(),
                        dead: false,
                    });
                }
                replica.by_guid.insert(s.guid, e);
            }
            ToGuest::Fields { guid, pairs } => {
                let Some(&e) = replica.by_guid.get(&guid) else {
                    continue;
                };
                match objs.get_mut(e) {
                    Ok((_, _, mut store, enemy, _)) => {
                        merge_fields(&mut store, e, guid, &pairs, &mut fields);
                        if let Some(mut en) = enemy {
                            if pairs
                                .iter()
                                .any(|&(i, v)| i == units::FIELD_HEALTH_INDEX && v == 0)
                            {
                                en.dead = true;
                            }
                        }
                    }
                    Err(_) => replica.deferred.push(ToGuest::Fields { guid, pairs }),
                }
            }
            ToGuest::Moves(list) => {
                let mut later = Vec::new();
                for m in list {
                    if replica.own == Some(m.guid) {
                        continue;
                    }
                    let Some(&e) = replica.by_guid.get(&m.guid) else {
                        continue;
                    };
                    match objs.get_mut(e) {
                        Ok((mut tf, mv, _, _, target)) => {
                            match target {
                                Some(mut t) => {
                                    t.pos = m.pos;
                                    t.yaw = m.yaw;
                                }
                                None => {
                                    tf.translation = m.pos;
                                    tf.rotation = Quat::from_rotation_y(m.yaw);
                                }
                            }
                            if let Some(mut mv) = mv {
                                mv.speed = m.speed;
                                mv.flags = m.flags;
                                mv.vertical_speed = m.vy;
                            }
                        }
                        Err(_) => later.push(m),
                    }
                }
                if !later.is_empty() {
                    replica.deferred.push(ToGuest::Moves(later));
                }
            }
            ToGuest::Fade(guid) => {
                // The fade despawns it here as on the host.
                if let Some(e) = replica.by_guid.remove(&guid) {
                    commands.entity(e).try_insert(DespawnFade::default());
                }
            }
            ToGuest::Despawn(guid) => {
                if let Some(e) = replica.by_guid.remove(&guid) {
                    commands.entity(e).try_despawn();
                }
            }
            ToGuest::Go {
                caster,
                spell,
                hits,
                dest,
            } => {
                if let Some(&c) = replica.by_guid.get(&caster) {
                    let hits: Vec<Entity> = hits
                        .iter()
                        .filter_map(|g| replica.by_guid.get(g).copied())
                        .collect();
                    fx.cast(c, spell, &hits, dest);
                }
            }
            ToGuest::Impact { target, spell } => {
                if let Some(&t) = replica.by_guid.get(&target) {
                    fx.impact(t, spell);
                }
            }
            ToGuest::Swing {
                attacker,
                victim,
                damage,
            } => {
                if let (Some(&a), Some(&v)) = (
                    replica.by_guid.get(&attacker),
                    replica.by_guid.get(&victim),
                ) {
                    fx.swing(a, v, damage);
                }
            }
            ToGuest::Number {
                anchor,
                amount,
                crit,
                color,
            } => {
                if let Some(&a) = replica.by_guid.get(&anchor) {
                    fx.number(a, amount, crit, color);
                }
            }
            ToGuest::Hud(h) => {
                if coop.joined {
                    apply_hud(&mut run, &mut coop, &h);
                }
            }
            ToGuest::Offer(cards) => {
                let list: Vec<Choice> = cards.iter().filter_map(|c| card_in(c, &run)).collect();
                offer.set(list);
            }
            ToGuest::Banner { text, secs } => {
                run.banner = Some((text, secs));
            }
            ToGuest::Teleport { pos, yaw } => {
                if let Some(&e) = replica.own.and_then(|g| replica.by_guid.get(&g)) {
                    if let Ok((mut tf, ..)) = objs.get_mut(e) {
                        tf.translation = pos;
                        tf.rotation = Quat::from_rotation_y(yaw);
                    }
                }
            }
        }
    }

    // The pick at the menu goes to the host whenever it changes.
    if coop.welcomed && coop.sent_pick != Some(selection.version) {
        let look = selection.look.and_then(|i| selection.looks.get(i)).copied();
        if let (Some(class), Some((display, race, sex))) = (selection.class, look) {
            coop.to_host(ToHost::Pick {
                class: class.min(255) as u8,
                display,
                race,
                sex,
            });
            coop.sent_pick = Some(selection.version);
        }
    }
}

/// Where the guest's hero stands, its aim, and its mobility key, to the host.
#[allow(clippy::type_complexity)]
fn guest_input(
    coop: Res<Coop>,
    replica: Res<Replica>,
    phase: Res<State<Phase>>,
    keys: Res<ButtonInput<KeyCode>>,
    aim: Res<hero::AimPoint>,
    loading: Res<crate::loading_screen::LoadingScreen>,
    hero: Query<(&Transform, &MovementState), (With<Hero>, Without<Mate>)>,
) {
    if !coop.joined || !coop.up || *phase.get() == Phase::Menu {
        return;
    }
    let (Some(guid), Ok((tf, mv))) = (replica.own, hero.single()) else {
        return;
    };
    coop.to_host(ToHost::Input {
        motion: Motion {
            guid,
            pos: tf.translation,
            yaw: yaw_of(tf),
            speed: mv.speed,
            flags: mv.flags,
            vy: mv.vertical_speed,
        },
        aim: aim.0,
        loading: loading.covering(),
    });
    if *phase.get() == Phase::Playing && keys.just_pressed(super::mobility::KEY) {
        coop.to_host(ToHost::Mobility);
    }
}

/// Replicated objects ease toward where the host last put them.
fn guest_smooth(time: Res<Time<Real>>, mut q: Query<(&mut Transform, &NetTarget)>) {
    let k = 1.0 - (-time.delta_secs() * 14.0).exp();
    for (mut tf, t) in &mut q {
        let d = t.pos - tf.translation;
        if d.length() > 10.0 {
            tf.translation = t.pos;
        } else if d.length_squared() > 1e-6 {
            tf.translation += d * k;
        }
        let want = Quat::from_rotation_y(t.yaw);
        if tf.rotation.angle_between(want) > 1e-3 {
            tf.rotation = tf.rotation.slerp(want, k);
        }
    }
}

/// Back at the menu the run is over for both sides; the next one starts fresh.
fn back_at_menu(
    coop: Option<ResMut<Coop>>,
    mut replica: ResMut<Replica>,
    mut repl: ResMut<HostRepl>,
) {
    let Some(mut coop) = coop else { return };
    coop.in_run = false;
    coop.replicating = false;
    coop.joined = false;
    replica.by_guid.clear();
    replica.own = None;
    replica.deferred.clear();
    repl.known.clear();
}

// ── The two lines of co-op text ───────────────────────────────────────────────────────────────

#[derive(Component)]
struct StatusText;

#[derive(Component)]
struct FriendText;

/// The lobby line at the menu, the friend line in a run, and the level-up subtitle while the other
/// player chooses.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn coop_ui(
    mut commands: Commands,
    coop: Option<Res<Coop>>,
    font: Option<Res<HudFont>>,
    phase: Res<State<Phase>>,
    run: Res<Run>,
    offer: Res<Offer>,
    mates: Query<&Mate>,
    mut status: Query<(&mut Text, &mut Node), (With<StatusText>, Without<FriendText>)>,
    mut friend: Query<(&mut Text, &mut Node), (With<FriendText>, Without<StatusText>)>,
    mut subs: Query<&mut Text, (With<LevelUpSub>, Without<StatusText>, Without<FriendText>)>,
    mut built: Local<bool>,
) {
    let Some(coop) = coop else { return };
    if !*built {
        let Some(font) = font else { return };
        *built = true;
        commands.spawn((
            StatusText,
            hud::centered_bundle(&font.0, "", 17.0, Color::srgb(1.0, 0.85, 0.3)),
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(8.0),
                left: Val::Percent(22.0),
                width: Val::Percent(56.0),
                justify_content: JustifyContent::Center,
                ..default()
            },
            GlobalZIndex(50),
        ));
        commands.spawn((
            FriendText,
            hud::text_bundle(&font.0, "", 16.0, Color::srgb(0.75, 0.95, 0.75)),
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(16.0),
                top: Val::Percent(40.0),
                max_width: Val::Px(360.0),
                ..default()
            },
            GlobalZIndex(50),
        ));
        return;
    }
    let p = *phase.get();
    let set = |node: &mut Node, on: bool| {
        let d = if on { Display::Flex } else { Display::None };
        if node.display != d {
            node.display = d;
        }
    };

    // The lobby line.
    if let Ok((mut text, mut node)) = status.single_mut() {
        let line = match coop.role {
            Role::Guest if coop.welcomed && !coop.left => {
                let host = if coop.host_pick.is_empty() {
                    "the host".to_string()
                } else {
                    format!("the host ({})", coop.host_pick)
                };
                if coop.host_in_run {
                    format!(
                        "Connected to {host}. They are in a run right now — you join the next one."
                    )
                } else {
                    format!(
                        "Connected to {host}. Pick your hero — the host picks the zone and \
                         starts the run."
                    )
                }
            }
            _ => coop.status.clone(),
        };
        if text.0 != line {
            text.0 = line;
        }
        set(&mut node, p == Phase::Menu);
    }

    // The friend line.
    let in_run = matches!(p, Phase::Playing | Phase::LevelUp | Phase::Paused);
    let me_down = run.dead_t.is_some();
    let friend_line = match coop.role {
        Role::Host => mates.iter().next().map(|m| {
            let r = &m.run;
            (
                pick_name(r.race, r.class),
                r.hp,
                r.stats().max_hp,
                r.dead_t.is_some(),
                r.revive,
            )
        }),
        Role::Guest if coop.joined => Some((
            if coop.host_pick.is_empty() {
                "The host".to_string()
            } else {
                coop.host_pick.clone()
            },
            coop.peer_hp,
            coop.peer_max,
            coop.peer_dead,
            coop.peer_revive,
        )),
        Role::Guest => None,
    };
    if let Ok((mut text, mut node)) = friend.single_mut() {
        let line = match &friend_line {
            Some((name, hp, max, dead, revive)) => {
                let mut s = if *dead {
                    format!(
                        "Friend: {name} is DOWN!\nStand next to them to revive ({:.1} / {:.0} s)",
                        revive,
                        hero::REVIVE_SECS
                    )
                } else {
                    format!("Friend: {name}\nHealth {:.0} / {:.0}", hp.max(0.0), max)
                };
                if me_down {
                    s.push_str(&format!(
                        "\n\nYou are down — your friend can revive you by standing next to you \
                         ({:.1} / {:.0} s)",
                        run.revive,
                        hero::REVIVE_SECS
                    ));
                }
                s
            }
            None => String::new(),
        };
        let show = in_run && !line.is_empty();
        if text.0 != line {
            text.0 = line;
        }
        set(&mut node, show);
    }

    // The level-up subtitle while the other player is still choosing.
    if p == Phase::LevelUp {
        let waiting = offer.len() == 0 && (friend_line.is_some());
        let want = if waiting {
            "Waiting for your friend to choose…"
        } else {
            "Choose your power"
        };
        for mut t in &mut subs {
            if t.0 != want {
                t.0 = want.to_string();
            }
        }
    }
}
