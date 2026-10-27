//! The co-op wire: a hand-rolled little-endian encoding of the messages host and guest trade, and
//! the TCP link that carries them (one length-prefixed frame per batch of messages).
//!
//! The host is authoritative for everything but the guest's own movement: it runs the horde, the
//! spells and the level-ups, and streams what changed. The guest drives its own hero and sends
//! where it stands.

use bevy::math::Vec3;

/// Bumped whenever a message changes shape: a host and guest on different builds refuse each other.
pub(crate) const PROTOCOL: u32 = 1;

/// The port a host listens on unless told otherwise.
pub(crate) const DEFAULT_PORT: u16 = 7777;

/// Largest frame either side accepts.
const MAX_FRAME: usize = 32 << 20;

// ── Encoding ──────────────────────────────────────────────────────────────────────────────────

#[derive(Default)]
pub(crate) struct W(pub(crate) Vec<u8>);

impl W {
    pub(crate) fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub(crate) fn bool(&mut self, v: bool) {
        self.0.push(u8::from(v));
    }
    pub(crate) fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn vec3(&mut self, v: Vec3) {
        self.f32(v.x);
        self.f32(v.y);
        self.f32(v.z);
    }
    pub(crate) fn str(&mut self, s: &str) {
        let b = s.as_bytes();
        let n = b.len().min(u16::MAX as usize);
        self.u16(n as u16);
        self.0.extend_from_slice(&b[..n]);
    }
    pub(crate) fn opt_f32(&mut self, v: Option<f32>) {
        match v {
            Some(v) => {
                self.u8(1);
                self.f32(v);
            }
            None => self.u8(0),
        }
    }
    pub(crate) fn opt_vec3(&mut self, v: Option<Vec3>) {
        match v {
            Some(v) => {
                self.u8(1);
                self.vec3(v);
            }
            None => self.u8(0),
        }
    }
    pub(crate) fn len(&mut self, n: usize) {
        self.u32(n as u32);
    }
}

/// A reader over one frame; every getter is `None` past the end, which ends the frame's decode.
pub(crate) struct R<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> R<'a> {
    pub(crate) fn new(b: &'a [u8]) -> Self {
        Self { b, at: 0 }
    }
    pub(crate) fn done(&self) -> bool {
        self.at >= self.b.len()
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let s = self.b.get(self.at..end)?;
        self.at = end;
        Some(s)
    }
    pub(crate) fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }
    pub(crate) fn bool(&mut self) -> Option<bool> {
        self.u8().map(|v| v != 0)
    }
    pub(crate) fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|s| u16::from_le_bytes([s[0], s[1]]))
    }
    pub(crate) fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    pub(crate) fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|s| {
            let mut a = [0u8; 8];
            a.copy_from_slice(s);
            u64::from_le_bytes(a)
        })
    }
    pub(crate) fn f32(&mut self) -> Option<f32> {
        self.u32().map(f32::from_bits)
    }
    pub(crate) fn f64(&mut self) -> Option<f64> {
        self.u64().map(f64::from_bits)
    }
    pub(crate) fn vec3(&mut self) -> Option<Vec3> {
        Some(Vec3::new(self.f32()?, self.f32()?, self.f32()?))
    }
    pub(crate) fn str(&mut self) -> Option<String> {
        let n = self.u16()? as usize;
        let s = self.take(n)?;
        Some(String::from_utf8_lossy(s).into_owned())
    }
    pub(crate) fn opt_f32(&mut self) -> Option<Option<f32>> {
        Some(if self.u8()? != 0 {
            Some(self.f32()?)
        } else {
            None
        })
    }
    pub(crate) fn opt_vec3(&mut self) -> Option<Option<Vec3>> {
        Some(if self.u8()? != 0 {
            Some(self.vec3()?)
        } else {
            None
        })
    }
    /// A count, capped so a corrupt frame cannot ask for a huge allocation.
    pub(crate) fn len(&mut self) -> Option<usize> {
        let n = self.u32()? as usize;
        (n <= self.b.len()).then_some(n)
    }
}

// ── Messages ──────────────────────────────────────────────────────────────────────────────────

/// What a replicated object is, for the guest to build the same body.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Body {
    Unit,
    GameObject,
    Dynamic,
}

impl Body {
    fn code(self) -> u8 {
        match self {
            Body::Unit => 0,
            Body::GameObject => 1,
            Body::Dynamic => 2,
        }
    }
    fn from_code(c: u8) -> Self {
        match c {
            1 => Body::GameObject,
            2 => Body::Dynamic,
            _ => Body::Unit,
        }
    }
}

/// One object's position and gait.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Motion {
    pub guid: u64,
    pub pos: Vec3,
    pub yaw: f32,
    pub speed: f32,
    pub flags: u32,
    pub vy: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Spawn {
    pub guid: u64,
    pub body: Body,
    pub display: Option<u32>,
    pub scale: f32,
    pub pos: Vec3,
    pub yaw: f32,
    pub enemy: bool,
    pub boss: bool,
    /// A pickup whose own animation is held still.
    pub still: bool,
    pub fields: Vec<(u16, u32)>,
}

/// A level-up card, by what it names: indexes are into the guest's own ability and passive lists.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CardW {
    NewAbility(String),
    Upgrade(u8),
    NewPassive(String),
    UpPassive(u8),
    Heal,
}

/// The guest's own run as the host keeps it, for its HUD.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct HudW {
    pub elapsed: f32,
    pub kills: u32,
    pub level: u32,
    pub xp: f32,
    pub hp: f32,
    pub abilities: Vec<(String, u8, f32)>,
    pub passives: Vec<(String, u8)>,
    pub shield: f32,
    pub shield_t: f32,
    pub immune_t: f32,
    pub thorns_t: f32,
    pub heal_t: f32,
    pub mob_cd: f32,
    pub mob_haste: f32,
    pub mob_haste_t: f32,
    pub mob_vanish_t: f32,
    pub pending: u32,
    pub bosses: u32,
    pub won: bool,
    pub dead_t: Option<f32>,
    pub revive: f32,
    pub damage_done: f64,
    /// The host's own hero, for the guest's friend panel.
    pub peer_hp: f32,
    pub peer_max: f32,
    pub peer_dead: bool,
    pub peer_revive: f32,
}

/// Host to guest.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ToGuest {
    Welcome { protocol: u32, host: String },
    Reject(String),
    Phase(u8),
    /// The host is at its menu (`false`) or in a run the guest is not part of (`true`).
    Lobby { in_run: bool, host_pick: String },
    Start { map: u16, serial: u64, pos: Vec3 },
    Spawn(Spawn),
    Fields { guid: u64, pairs: Vec<(u16, u32)> },
    Moves(Vec<Motion>),
    Fade(u64),
    Despawn(u64),
    Go { caster: u64, spell: u32, hits: Vec<u64>, dest: Option<Vec3> },
    Impact { target: u64, spell: u32 },
    Swing { attacker: u64, victim: u64, damage: f32 },
    Number { anchor: u64, amount: f32, crit: bool, color: u32 },
    Hud(Box<HudW>),
    Offer(Vec<CardW>),
    Banner { text: String, secs: f32 },
    Teleport { pos: Vec3, yaw: f32 },
}

/// Guest to host.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ToHost {
    Hello { protocol: u32 },
    /// The guest's pick at its menu: class index, and `(display, race, sex)`.
    Pick { class: u8, display: u32, race: u8, sex: u8 },
    Input { motion: Motion, aim: Option<Vec3>, loading: bool },
    Mobility,
    Card(u8),
    /// 0 pause/unpause, 1 resume.
    Request(u8),
    Bye,
}

fn put_motion(w: &mut W, m: &Motion) {
    w.u64(m.guid);
    w.vec3(m.pos);
    w.f32(m.yaw);
    w.f32(m.speed);
    w.u32(m.flags);
    w.f32(m.vy);
}

fn get_motion(r: &mut R) -> Option<Motion> {
    Some(Motion {
        guid: r.u64()?,
        pos: r.vec3()?,
        yaw: r.f32()?,
        speed: r.f32()?,
        flags: r.u32()?,
        vy: r.f32()?,
    })
}

fn put_pairs(w: &mut W, pairs: &[(u16, u32)]) {
    w.len(pairs.len());
    for &(i, v) in pairs {
        w.u16(i);
        w.u32(v);
    }
}

fn get_pairs(r: &mut R) -> Option<Vec<(u16, u32)>> {
    let n = r.len()?;
    let mut out = Vec::with_capacity(n.min(512));
    for _ in 0..n {
        out.push((r.u16()?, r.u32()?));
    }
    Some(out)
}

fn put_guids(w: &mut W, v: &[u64]) {
    w.len(v.len());
    for &g in v {
        w.u64(g);
    }
}

fn get_guids(r: &mut R) -> Option<Vec<u64>> {
    let n = r.len()?;
    let mut out = Vec::with_capacity(n.min(512));
    for _ in 0..n {
        out.push(r.u64()?);
    }
    Some(out)
}

impl ToGuest {
    pub(crate) fn put(&self, w: &mut W) {
        match self {
            ToGuest::Welcome { protocol, host } => {
                w.u8(0);
                w.u32(*protocol);
                w.str(host);
            }
            ToGuest::Reject(s) => {
                w.u8(1);
                w.str(s);
            }
            ToGuest::Phase(p) => {
                w.u8(2);
                w.u8(*p);
            }
            ToGuest::Lobby { in_run, host_pick } => {
                w.u8(3);
                w.bool(*in_run);
                w.str(host_pick);
            }
            ToGuest::Start { map, serial, pos } => {
                w.u8(4);
                w.u16(*map);
                w.u64(*serial);
                w.vec3(*pos);
            }
            ToGuest::Spawn(s) => {
                w.u8(5);
                w.u64(s.guid);
                w.u8(s.body.code());
                w.u32(s.display.map_or(0, |d| d.saturating_add(1)));
                w.f32(s.scale);
                w.vec3(s.pos);
                w.f32(s.yaw);
                w.u8(u8::from(s.enemy) | u8::from(s.boss) << 1 | u8::from(s.still) << 2);
                put_pairs(w, &s.fields);
            }
            ToGuest::Fields { guid, pairs } => {
                w.u8(6);
                w.u64(*guid);
                put_pairs(w, pairs);
            }
            ToGuest::Moves(v) => {
                w.u8(7);
                w.len(v.len());
                for m in v {
                    put_motion(w, m);
                }
            }
            ToGuest::Fade(g) => {
                w.u8(8);
                w.u64(*g);
            }
            ToGuest::Despawn(g) => {
                w.u8(9);
                w.u64(*g);
            }
            ToGuest::Go {
                caster,
                spell,
                hits,
                dest,
            } => {
                w.u8(10);
                w.u64(*caster);
                w.u32(*spell);
                put_guids(w, hits);
                w.opt_vec3(*dest);
            }
            ToGuest::Impact { target, spell } => {
                w.u8(11);
                w.u64(*target);
                w.u32(*spell);
            }
            ToGuest::Swing {
                attacker,
                victim,
                damage,
            } => {
                w.u8(12);
                w.u64(*attacker);
                w.u64(*victim);
                w.f32(*damage);
            }
            ToGuest::Number {
                anchor,
                amount,
                crit,
                color,
            } => {
                w.u8(13);
                w.u64(*anchor);
                w.f32(*amount);
                w.bool(*crit);
                w.u32(*color);
            }
            ToGuest::Hud(h) => {
                w.u8(14);
                w.f32(h.elapsed);
                w.u32(h.kills);
                w.u32(h.level);
                w.f32(h.xp);
                w.f32(h.hp);
                w.len(h.abilities.len());
                for (k, l, cd) in &h.abilities {
                    w.str(k);
                    w.u8(*l);
                    w.f32(*cd);
                }
                w.len(h.passives.len());
                for (k, l) in &h.passives {
                    w.str(k);
                    w.u8(*l);
                }
                for v in [
                    h.shield,
                    h.shield_t,
                    h.immune_t,
                    h.thorns_t,
                    h.heal_t,
                    h.mob_cd,
                    h.mob_haste,
                    h.mob_haste_t,
                    h.mob_vanish_t,
                ] {
                    w.f32(v);
                }
                w.u32(h.pending);
                w.u32(h.bosses);
                w.bool(h.won);
                w.opt_f32(h.dead_t);
                w.f32(h.revive);
                w.f64(h.damage_done);
                w.f32(h.peer_hp);
                w.f32(h.peer_max);
                w.bool(h.peer_dead);
                w.f32(h.peer_revive);
            }
            ToGuest::Offer(cards) => {
                w.u8(15);
                w.len(cards.len());
                for c in cards {
                    match c {
                        CardW::NewAbility(k) => {
                            w.u8(0);
                            w.str(k);
                        }
                        CardW::Upgrade(i) => {
                            w.u8(1);
                            w.u8(*i);
                        }
                        CardW::NewPassive(k) => {
                            w.u8(2);
                            w.str(k);
                        }
                        CardW::UpPassive(i) => {
                            w.u8(3);
                            w.u8(*i);
                        }
                        CardW::Heal => w.u8(4),
                    }
                }
            }
            ToGuest::Banner { text, secs } => {
                w.u8(16);
                w.str(text);
                w.f32(*secs);
            }
            ToGuest::Teleport { pos, yaw } => {
                w.u8(17);
                w.vec3(*pos);
                w.f32(*yaw);
            }
        }
    }

    pub(crate) fn get(r: &mut R) -> Option<Self> {
        Some(match r.u8()? {
            0 => ToGuest::Welcome {
                protocol: r.u32()?,
                host: r.str()?,
            },
            1 => ToGuest::Reject(r.str()?),
            2 => ToGuest::Phase(r.u8()?),
            3 => ToGuest::Lobby {
                in_run: r.bool()?,
                host_pick: r.str()?,
            },
            4 => ToGuest::Start {
                map: r.u16()?,
                serial: r.u64()?,
                pos: r.vec3()?,
            },
            5 => {
                let guid = r.u64()?;
                let body = Body::from_code(r.u8()?);
                let display = r.u32()?.checked_sub(1);
                let scale = r.f32()?;
                let pos = r.vec3()?;
                let yaw = r.f32()?;
                let flags = r.u8()?;
                let fields = get_pairs(r)?;
                ToGuest::Spawn(Spawn {
                    guid,
                    body,
                    display,
                    scale,
                    pos,
                    yaw,
                    enemy: flags & 1 != 0,
                    boss: flags & 2 != 0,
                    still: flags & 4 != 0,
                    fields,
                })
            }
            6 => ToGuest::Fields {
                guid: r.u64()?,
                pairs: get_pairs(r)?,
            },
            7 => {
                let n = r.len()?;
                let mut v = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    v.push(get_motion(r)?);
                }
                ToGuest::Moves(v)
            }
            8 => ToGuest::Fade(r.u64()?),
            9 => ToGuest::Despawn(r.u64()?),
            10 => ToGuest::Go {
                caster: r.u64()?,
                spell: r.u32()?,
                hits: get_guids(r)?,
                dest: r.opt_vec3()?,
            },
            11 => ToGuest::Impact {
                target: r.u64()?,
                spell: r.u32()?,
            },
            12 => ToGuest::Swing {
                attacker: r.u64()?,
                victim: r.u64()?,
                damage: r.f32()?,
            },
            13 => ToGuest::Number {
                anchor: r.u64()?,
                amount: r.f32()?,
                crit: r.bool()?,
                color: r.u32()?,
            },
            14 => {
                let mut h = HudW {
                    elapsed: r.f32()?,
                    kills: r.u32()?,
                    level: r.u32()?,
                    xp: r.f32()?,
                    hp: r.f32()?,
                    ..Default::default()
                };
                let n = r.len()?;
                for _ in 0..n {
                    h.abilities.push((r.str()?, r.u8()?, r.f32()?));
                }
                let n = r.len()?;
                for _ in 0..n {
                    h.passives.push((r.str()?, r.u8()?));
                }
                h.shield = r.f32()?;
                h.shield_t = r.f32()?;
                h.immune_t = r.f32()?;
                h.thorns_t = r.f32()?;
                h.heal_t = r.f32()?;
                h.mob_cd = r.f32()?;
                h.mob_haste = r.f32()?;
                h.mob_haste_t = r.f32()?;
                h.mob_vanish_t = r.f32()?;
                h.pending = r.u32()?;
                h.bosses = r.u32()?;
                h.won = r.bool()?;
                h.dead_t = r.opt_f32()?;
                h.revive = r.f32()?;
                h.damage_done = r.f64()?;
                h.peer_hp = r.f32()?;
                h.peer_max = r.f32()?;
                h.peer_dead = r.bool()?;
                h.peer_revive = r.f32()?;
                ToGuest::Hud(Box::new(h))
            }
            15 => {
                let n = r.len()?;
                let mut v = Vec::with_capacity(n.min(8));
                for _ in 0..n {
                    v.push(match r.u8()? {
                        0 => CardW::NewAbility(r.str()?),
                        1 => CardW::Upgrade(r.u8()?),
                        2 => CardW::NewPassive(r.str()?),
                        3 => CardW::UpPassive(r.u8()?),
                        _ => CardW::Heal,
                    });
                }
                ToGuest::Offer(v)
            }
            16 => ToGuest::Banner {
                text: r.str()?,
                secs: r.f32()?,
            },
            17 => ToGuest::Teleport {
                pos: r.vec3()?,
                yaw: r.f32()?,
            },
            _ => return None,
        })
    }
}

impl ToHost {
    pub(crate) fn put(&self, w: &mut W) {
        match self {
            ToHost::Hello { protocol } => {
                w.u8(100);
                w.u32(*protocol);
            }
            ToHost::Pick {
                class,
                display,
                race,
                sex,
            } => {
                w.u8(101);
                w.u8(*class);
                w.u32(*display);
                w.u8(*race);
                w.u8(*sex);
            }
            ToHost::Input {
                motion,
                aim,
                loading,
            } => {
                w.u8(102);
                put_motion(w, motion);
                w.opt_vec3(*aim);
                w.bool(*loading);
            }
            ToHost::Mobility => w.u8(103),
            ToHost::Card(i) => {
                w.u8(104);
                w.u8(*i);
            }
            ToHost::Request(k) => {
                w.u8(105);
                w.u8(*k);
            }
            ToHost::Bye => w.u8(106),
        }
    }

    pub(crate) fn get(r: &mut R) -> Option<Self> {
        Some(match r.u8()? {
            100 => ToHost::Hello {
                protocol: r.u32()?,
            },
            101 => ToHost::Pick {
                class: r.u8()?,
                display: r.u32()?,
                race: r.u8()?,
                sex: r.u8()?,
            },
            102 => ToHost::Input {
                motion: get_motion(r)?,
                aim: r.opt_vec3()?,
                loading: r.bool()?,
            },
            103 => ToHost::Mobility,
            104 => ToHost::Card(r.u8()?),
            105 => ToHost::Request(r.u8()?),
            106 => ToHost::Bye,
            _ => return None,
        })
    }
}

/// Encode a batch into one frame body.
pub(crate) fn encode_guest(msgs: &[ToGuest]) -> Vec<u8> {
    let mut w = W::default();
    for m in msgs {
        m.put(&mut w);
    }
    w.0
}

pub(crate) fn encode_host(msgs: &[ToHost]) -> Vec<u8> {
    let mut w = W::default();
    for m in msgs {
        m.put(&mut w);
    }
    w.0
}

/// Decode a frame; a malformed tail is dropped with what decoded before it.
pub(crate) fn decode_guest(b: &[u8]) -> Vec<ToGuest> {
    let mut r = R::new(b);
    let mut out = Vec::new();
    while !r.done() {
        match ToGuest::get(&mut r) {
            Some(m) => out.push(m),
            None => break,
        }
    }
    out
}

pub(crate) fn decode_host(b: &[u8]) -> Vec<ToHost> {
    let mut r = R::new(b);
    let mut out = Vec::new();
    while !r.done() {
        match ToHost::get(&mut r) {
            Some(m) => out.push(m),
            None => break,
        }
    }
    out
}

// ── The link ──────────────────────────────────────────────────────────────────────────────────

/// What the link thread tells the game.
#[derive(Debug)]
pub(crate) enum LinkEvent {
    /// A peer is on the line (the host's listener accepted one, or the guest's connect went
    /// through).
    Up(String),
    Frame(Vec<u8>),
    /// The peer went away; the reason is for the status line.
    Down(String),
    /// The listener or the connect itself failed.
    Failed(String),
}

/// The game's end of a link: frames out, events in. Sending while no peer is connected drops the
/// frame.
pub(crate) struct Link {
    out: crossbeam_channel::Sender<Vec<u8>>,
    pub(crate) events: crossbeam_channel::Receiver<LinkEvent>,
}

impl Link {
    /// Queue a frame; `false` when the link is backed up past what a live connection drains, which
    /// the caller treats as a lost connection.
    pub(crate) fn send(&self, frame: Vec<u8>) -> bool {
        !matches!(
            self.out.try_send(frame),
            Err(crossbeam_channel::TrySendError::Full(_))
        )
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::{Link, LinkEvent, MAX_FRAME};
    use std::io::{Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Frames queued for the peer before the link is called too slow.
    const QUEUE: usize = 600;

    type Slot = Arc<Mutex<Option<(u64, TcpStream)>>>;

    fn read_frames(mut s: TcpStream, events: crossbeam_channel::Sender<LinkEvent>) -> String {
        let mut len = [0u8; 4];
        loop {
            if let Err(e) = s.read_exact(&mut len) {
                return format!("connection closed ({e})");
            }
            let n = u32::from_le_bytes(len) as usize;
            if n > MAX_FRAME {
                return "bad frame from the other side".into();
            }
            let mut buf = vec![0u8; n];
            if let Err(e) = s.read_exact(&mut buf) {
                return format!("connection closed ({e})");
            }
            if events.send(LinkEvent::Frame(buf)).is_err() {
                return "game closed".into();
            }
        }
    }

    /// Drain the outgoing queue into whatever stream is current.
    fn write_loop(out: crossbeam_channel::Receiver<Vec<u8>>, slot: Slot) {
        while let Ok(frame) = out.recv() {
            let mut guard = match slot.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let Some((_, stream)) = guard.as_mut() else {
                continue;
            };
            let mut buf = Vec::with_capacity(frame.len() + 4);
            buf.extend_from_slice(&(frame.len() as u32).to_le_bytes());
            buf.extend_from_slice(&frame);
            if stream.write_all(&buf).is_err() {
                let _ = stream.shutdown(Shutdown::Both);
                *guard = None;
            }
        }
    }

    fn tune(s: &TcpStream) {
        let _ = s.set_nodelay(true);
        let _ = s.set_read_timeout(None);
        let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
    }

    /// Listen on `port`; one guest at a time, later ones are turned away by the game.
    pub(crate) fn host(port: u16) -> Link {
        let (out_tx, out_rx) = crossbeam_channel::bounded::<Vec<u8>>(QUEUE);
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        let slot: Slot = Arc::new(Mutex::new(None));
        {
            let slot = slot.clone();
            std::thread::Builder::new()
                .name("coop-write".into())
                .spawn(move || write_loop(out_rx, slot))
                .ok();
        }
        std::thread::Builder::new()
            .name("coop-listen".into())
            .spawn(move || {
                let listener = match TcpListener::bind(("0.0.0.0", port)) {
                    Ok(l) => l,
                    Err(e) => {
                        let _ = ev_tx.send(LinkEvent::Failed(format!(
                            "could not open port {port}: {e}"
                        )));
                        return;
                    }
                };
                let mut serial = 0u64;
                for conn in listener.incoming() {
                    let Ok(stream) = conn else { continue };
                    {
                        let busy = slot.lock().map(|g| g.is_some()).unwrap_or(false);
                        if busy {
                            // A second guest: the game is two-player.
                            let _ = stream.shutdown(Shutdown::Both);
                            continue;
                        }
                    }
                    tune(&stream);
                    serial += 1;
                    let id = serial;
                    let peer = stream
                        .peer_addr()
                        .map(|a| a.ip().to_string())
                        .unwrap_or_else(|_| "?".into());
                    let Ok(reader) = stream.try_clone() else {
                        continue;
                    };
                    if let Ok(mut g) = slot.lock() {
                        *g = Some((id, stream));
                    }
                    if ev_tx.send(LinkEvent::Up(peer)).is_err() {
                        return;
                    }
                    let ev = ev_tx.clone();
                    let slot2 = slot.clone();
                    std::thread::Builder::new()
                        .name("coop-read".into())
                        .spawn(move || {
                            let why = read_frames(reader, ev.clone());
                            if let Ok(mut g) = slot2.lock() {
                                if g.as_ref().is_some_and(|(i, _)| *i == id) {
                                    if let Some((_, s)) = g.take() {
                                        let _ = s.shutdown(Shutdown::Both);
                                    }
                                }
                            }
                            let _ = ev.send(LinkEvent::Down(why));
                        })
                        .ok();
                }
            })
            .ok();
        Link {
            out: out_tx,
            events: ev_rx,
        }
    }

    /// Connect to `addr` (`ip` or `ip:port`; a bare ip takes the default port).
    pub(crate) fn join(addr: String) -> Link {
        let (out_tx, out_rx) = crossbeam_channel::bounded::<Vec<u8>>(QUEUE);
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        let slot: Slot = Arc::new(Mutex::new(None));
        {
            let slot = slot.clone();
            std::thread::Builder::new()
                .name("coop-write".into())
                .spawn(move || write_loop(out_rx, slot))
                .ok();
        }
        std::thread::Builder::new()
            .name("coop-connect".into())
            .spawn(move || {
                let target = with_port(&addr);
                let addrs: Vec<SocketAddr> = match target.to_socket_addrs() {
                    Ok(a) => a.collect(),
                    Err(e) => {
                        let _ = ev_tx.send(LinkEvent::Failed(format!(
                            "could not understand the address {addr:?}: {e}"
                        )));
                        return;
                    }
                };
                let mut last = String::from("no address");
                for a in addrs {
                    match TcpStream::connect_timeout(&a, Duration::from_secs(8)) {
                        Ok(stream) => {
                            tune(&stream);
                            let Ok(reader) = stream.try_clone() else {
                                continue;
                            };
                            if let Ok(mut g) = slot.lock() {
                                *g = Some((1, stream));
                            }
                            if ev_tx.send(LinkEvent::Up(a.to_string())).is_err() {
                                return;
                            }
                            let why = read_frames(reader, ev_tx.clone());
                            if let Ok(mut g) = slot.lock() {
                                *g = None;
                            }
                            let _ = ev_tx.send(LinkEvent::Down(why));
                            return;
                        }
                        Err(e) => last = e.to_string(),
                    }
                }
                let _ = ev_tx.send(LinkEvent::Failed(format!(
                    "could not reach the host at {target}: {last}"
                )));
            })
            .ok();
        Link {
            out: out_tx,
            events: ev_rx,
        }
    }

    /// `ip` → `ip:7777`; an `ip:port` or a bracketed IPv6 with a port is left as given.
    pub(crate) fn with_port(addr: &str) -> String {
        let a = addr.trim();
        if a.parse::<SocketAddr>().is_ok() {
            return a.to_string();
        }
        if a.parse::<std::net::Ipv6Addr>().is_ok() {
            return format!("[{a}]:{}", super::DEFAULT_PORT);
        }
        if a.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
            return a.to_string();
        }
        format!("{a}:{}", super::DEFAULT_PORT)
    }

    /// This machine's address on its default route, for the host's status line. Sends nothing: a
    /// UDP "connect" only picks the interface.
    pub(crate) fn local_ip() -> Option<String> {
        let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        s.connect("8.8.8.8:80").ok()?;
        Some(s.local_addr().ok()?.ip().to_string())
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::{host, join, local_ip};

#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) use native::with_port;

#[cfg(target_arch = "wasm32")]
pub(crate) fn host(_port: u16) -> Link {
    unreachable_link("co-op is not available in the browser build")
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn join(_addr: String) -> Link {
    unreachable_link("co-op is not available in the browser build")
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn local_ip() -> Option<String> {
    None
}

#[cfg(target_arch = "wasm32")]
fn unreachable_link(why: &str) -> Link {
    let (out, _rx) = crossbeam_channel::bounded(1);
    let (tx, events) = crossbeam_channel::unbounded();
    let _ = tx.send(LinkEvent::Failed(why.into()));
    Link { out, events }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_guest_message_round_trips() {
        let msgs = vec![
            ToGuest::Welcome {
                protocol: PROTOCOL,
                host: "Orc Warrior".into(),
            },
            ToGuest::Reject("full".into()),
            ToGuest::Phase(3),
            ToGuest::Lobby {
                in_run: true,
                host_pick: "Human Mage".into(),
            },
            ToGuest::Start {
                map: 4,
                serial: 77,
                pos: Vec3::new(1.0, 2.0, 3.0),
            },
            ToGuest::Spawn(Spawn {
                guid: 0xF130_0000_0000_0042,
                body: Body::GameObject,
                display: Some(0),
                scale: 2.5,
                pos: Vec3::ONE,
                yaw: 0.5,
                enemy: true,
                boss: false,
                still: true,
                fields: vec![(0, 1), (22, 100)],
            }),
            ToGuest::Spawn(Spawn {
                guid: 1,
                body: Body::Dynamic,
                display: None,
                scale: 1.0,
                pos: Vec3::ZERO,
                yaw: 0.0,
                enemy: false,
                boss: true,
                still: false,
                fields: vec![],
            }),
            ToGuest::Fields {
                guid: 9,
                pairs: vec![(47, 17), (95, 2)],
            },
            ToGuest::Moves(vec![Motion {
                guid: 3,
                pos: Vec3::new(-4.0, 5.0, 6.0),
                yaw: 1.0,
                speed: 7.0,
                flags: 1,
                vy: -2.0,
            }]),
            ToGuest::Fade(5),
            ToGuest::Despawn(6),
            ToGuest::Go {
                caster: 1,
                spell: 133,
                hits: vec![2, 3],
                dest: Some(Vec3::X),
            },
            ToGuest::Impact {
                target: 2,
                spell: 403,
            },
            ToGuest::Swing {
                attacker: 2,
                victim: 1,
                damage: 12.0,
            },
            ToGuest::Number {
                anchor: 2,
                amount: 123.0,
                crit: true,
                color: 0xFFFF_0000,
            },
            ToGuest::Hud(Box::new(HudW {
                elapsed: 12.5,
                kills: 40,
                level: 6,
                xp: 3.5,
                hp: 99.0,
                abilities: vec![("fireball".into(), 3, 0.25)],
                passives: vec![("might".into(), 2)],
                dead_t: Some(1.0),
                damage_done: 1234.5,
                peer_dead: true,
                ..Default::default()
            })),
            ToGuest::Offer(vec![
                CardW::NewAbility("frost_nova".into()),
                CardW::Upgrade(1),
                CardW::NewPassive("speed".into()),
                CardW::UpPassive(0),
                CardW::Heal,
            ]),
            ToGuest::Banner {
                text: "Hogger approaches!".into(),
                secs: 4.0,
            },
            ToGuest::Teleport {
                pos: Vec3::Y,
                yaw: 2.0,
            },
        ];
        let back = decode_guest(&encode_guest(&msgs));
        assert_eq!(back, msgs);
    }

    #[test]
    fn every_host_message_round_trips() {
        let msgs = vec![
            ToHost::Hello { protocol: PROTOCOL },
            ToHost::Pick {
                class: 2,
                display: 3000,
                race: 5,
                sex: 1,
            },
            ToHost::Input {
                motion: Motion {
                    guid: 8,
                    pos: Vec3::ONE,
                    yaw: 0.1,
                    speed: 7.0,
                    flags: 0x10,
                    vy: 0.0,
                },
                aim: None,
                loading: true,
            },
            ToHost::Mobility,
            ToHost::Card(2),
            ToHost::Request(0),
            ToHost::Bye,
        ];
        let back = decode_host(&encode_host(&msgs));
        assert_eq!(back, msgs);
    }

    #[test]
    fn a_truncated_frame_keeps_what_decoded() {
        let mut b = encode_guest(&[ToGuest::Phase(1), ToGuest::Fade(7)]);
        b.truncate(b.len() - 3);
        assert_eq!(decode_guest(&b), vec![ToGuest::Phase(1)]);
    }

    #[test]
    fn a_bare_address_takes_the_default_port() {
        assert_eq!(with_port("10.0.0.2"), "10.0.0.2:7777");
        assert_eq!(with_port("10.0.0.2:9000"), "10.0.0.2:9000");
        assert_eq!(with_port("my-pc"), "my-pc:7777");
        assert_eq!(with_port("::1"), "[::1]:7777");
        assert_eq!(with_port("[::1]:5000"), "[::1]:5000");
    }

    #[test]
    fn two_links_talk_over_loopback() {
        let host = host(47_777);
        std::thread::sleep(std::time::Duration::from_millis(200));
        let guest = join("127.0.0.1:47777".into());
        let wait = |l: &Link| l.events.recv_timeout(std::time::Duration::from_secs(5));
        assert!(matches!(wait(&guest), Ok(LinkEvent::Up(_))));
        assert!(matches!(wait(&host), Ok(LinkEvent::Up(_))));
        assert!(guest.send(encode_host(&[ToHost::Hello { protocol: PROTOCOL }])));
        match wait(&host) {
            Ok(LinkEvent::Frame(f)) => {
                assert_eq!(decode_host(&f), vec![ToHost::Hello { protocol: PROTOCOL }]);
            }
            other => panic!("expected a frame, got {other:?}"),
        }
        assert!(host.send(encode_guest(&[ToGuest::Phase(1)])));
        match wait(&guest) {
            Ok(LinkEvent::Frame(f)) => assert_eq!(decode_guest(&f), vec![ToGuest::Phase(1)]),
            other => panic!("expected a frame, got {other:?}"),
        }
    }
}
