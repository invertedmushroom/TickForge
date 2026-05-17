use bevy::prelude::*;
use std::collections::VecDeque;

pub struct CombatLogPlugin;

impl Plugin for CombatLogPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CombatLog>();
        app.add_systems(Startup, spawn_combat_log_panel);
        app.add_systems(
            Update,
            (
                poll_combat_events,
                poll_world_events,
                update_combat_log_text,
            )
                .chain(),
        );
    }
}

const MAX_LOG_LINES: usize = 20;

/// Ring buffer of recent combat log messages.
#[derive(Resource, Default)]
pub struct CombatLog {
    pub entries: VecDeque<LogEntry>,
    /// Target entity whose next Damage line should be suppressed (already shown via Blocked).
    blocked_suppress_target: Option<u64>,
}

pub struct LogEntry {
    pub text: String,
    #[allow(dead_code)] // reserved for per-line color rendering
    pub color: Color,
    pub age: f32,
}

impl CombatLog {
    pub fn push(&mut self, text: String, color: Color) {
        if self.entries.len() >= MAX_LOG_LINES {
            self.entries.pop_front();
        }
        self.entries.push_back(LogEntry {
            text,
            color,
            age: 0.0,
        });
    }
}

#[derive(Component)]
struct CombatLogPanel;

fn spawn_combat_log_panel(mut commands: Commands) {
    commands.spawn((
        Text::new(""),
        TextFont {
            font_size: 14.0,
            ..default()
        },
        TextColor(Color::srgba(1.0, 1.0, 1.0, 0.85)),
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(10.0),
            bottom: Val::Px(10.0),
            max_width: Val::Px(500.0),
            ..default()
        },
        CombatLogPanel,
    ));
}

/// Poll SpacetimeDB combat_event table for new events and translate to log lines.
#[cfg(feature = "connected")]
fn poll_combat_events(
    stdb_events: Option<Res<crate::spacetime::SpacetimeEvents>>,
    local_player: Res<crate::spacetime::LocalPlayerEntity>,
    mut log: ResMut<CombatLog>,
    mut death_events: EventWriter<crate::vfx::DeathNotification>,
    mut damage_events: EventWriter<crate::vfx::DamageNumberEvent>,
    mut proj_launch: EventWriter<crate::vfx::ProjectileLaunchEvent>,
    mut skill_obj_remove: EventWriter<crate::vfx::SkillObjectRemoveEvent>,
    mut hazard_spawn: EventWriter<crate::vfx::HazardSpawnEvent>,
    mut charge_start: EventWriter<crate::vfx::ChargeStartEvent>,
    mut charge_tier: EventWriter<crate::vfx::ChargeTierReachedEvent>,
    mut hitbox_spawn: EventWriter<crate::vfx::HitboxSpawnedEvent>,
    mut hitbox_dmg: EventWriter<crate::vfx::HitboxDamageFrameEvent>,
    mut hitbox_remove: EventWriter<crate::vfx::HitboxRemovedEvent>,
    mut buff_applied: EventWriter<crate::vfx::BuffAppliedVfxEvent>,
    mut teleported: EventWriter<crate::vfx::TeleportVfxEvent>,
) {
    use game_client::module_bindings::*;

    let Some(events_res) = stdb_events else {
        return;
    };

    let mut new_events = Vec::new();
    while let Ok(ev) = events_res.combat_event_rx.try_recv() {
        new_events.push(ev);
    }
    // Sort by sequence to ensure proper ordering within a tick batch.
    new_events.sort_by_key(|ev| ev.event_sequence);

    for ev in &new_events {
        let is_us_source = local_player.entity_id == Some(ev.source_entity);
        let is_us_target = local_player.entity_id == Some(ev.target_entity);

        // Track Blocked/Covered → Damage suppression: these already show the damage,
        // so skip the redundant Damage line that immediately follows.
        if matches!(
            &ev.event_kind,
            CombatEventKind::Blocked(_) | CombatEventKind::Covered(_)
        ) {
            log.blocked_suppress_target = Some(ev.target_entity);
        }
        let suppress_damage = if matches!(&ev.event_kind, CombatEventKind::Damage(_)) {
            if log.blocked_suppress_target == Some(ev.target_entity) {
                log.blocked_suppress_target = None;
                true
            } else {
                false
            }
        } else {
            false
        };

        let (text, color) = format_combat_event(&ev, is_us_source, is_us_target);
        if !text.is_empty() && !suppress_damage {
            log.push(text, color);
        }

        // Emit floating damage number.
        if let CombatEventKind::Damage(ref d) = ev.event_kind {
            damage_events.send(crate::vfx::DamageNumberEvent {
                target_entity_id: ev.target_entity,
                amount: d.amount,
                is_crit: false,
                is_self: is_us_target,
            });
        }

        // Emit floating heal number (green, negative convention).
        if let CombatEventKind::Healed(ref h) = ev.event_kind {
            damage_events.send(crate::vfx::DamageNumberEvent {
                target_entity_id: ev.target_entity,
                amount: -h.amount,
                is_crit: false,
                is_self: is_us_target,
            });
        }

        // Emit death notification for VFX (death marker + "YOU DIED" screen).
        if matches!(&ev.event_kind, CombatEventKind::EntityDied(_)) {
            death_events.send(crate::vfx::DeathNotification {
                entity_id: ev.target_entity,
                is_local_player: is_us_target,
            });
        }

        // Emit projectile launch/remove and charge VFX events.
        // Server tick rate is 20Hz; convert speed from units/tick to units/second.
        const TICK_RATE: f32 = 20.0;
        match &ev.event_kind {
            CombatEventKind::ProjectileLaunched(p) => {
                proj_launch.send(crate::vfx::ProjectileLaunchEvent {
                    execution_id: p.execution_id,
                    origin: Vec3::new(p.origin_x, p.origin_y, p.origin_z),
                    direction: Vec3::new(p.direction_x, p.direction_y, p.direction_z),
                    speed: p.speed * TICK_RATE,
                    max_range: p.max_range,
                });
            }
            CombatEventKind::HazardSpawned(h) => {
                // SelfOnly abilities (e.g. Flame Aura) already get a caster-following
                // hitbox visual via CastStart — skip the static ground disc.
                use crate::ability_bar::{ClientTargetingMode, all_abilities};
                let is_self_only = all_abilities()
                    .iter()
                    .find(|a| a.id == h.ability_id)
                    .map(|a| a.targeting == ClientTargetingMode::SelfOnly)
                    .unwrap_or(false);
                if !is_self_only {
                    hazard_spawn.send(crate::vfx::HazardSpawnEvent {
                        execution_id: h.execution_id,
                        ability_id: h.ability_id,
                        source_entity_id: ev.source_entity,
                        position: Vec3::new(h.pos_x, h.pos_y, h.pos_z),
                        radius: h.radius,
                    });
                }
            }
            CombatEventKind::SkillObjectRemoved(exec_id) => {
                skill_obj_remove.send(crate::vfx::SkillObjectRemoveEvent {
                    execution_id: *exec_id,
                });
            }
            CombatEventKind::ChargeStart(c) => {
                charge_start.send(crate::vfx::ChargeStartEvent {
                    source_entity_id: ev.source_entity,
                    ability_id: c.ability_id,
                    max_ticks: c.max_ticks,
                });
            }
            CombatEventKind::ChargeTierReached(c) => {
                charge_tier.send(crate::vfx::ChargeTierReachedEvent {
                    source_entity_id: ev.source_entity,
                    ability_id: c.ability_id,
                    tier: c.tier as u32,
                });
            }
            // Proxy CastStart → HitboxSpawned event so melee shapes appear on cast.
            // Skip HazardZone abilities (GroundTarget/CasterOffset) — those get
            // their own HazardSpawnEvent with the correct world position.
            CombatEventKind::CastStart(c) => {
                use crate::ability_bar::{ClientTargetingMode, all_abilities};
                let is_hazard = all_abilities()
                    .iter()
                    .find(|a| a.id == c.ability_id)
                    .map(|a| {
                        matches!(
                            a.targeting,
                            ClientTargetingMode::GroundTarget | ClientTargetingMode::CasterOffset
                        )
                    })
                    .unwrap_or(false);
                if !is_hazard {
                    hitbox_spawn.send(crate::vfx::HitboxSpawnedEvent {
                        source_entity_id: ev.source_entity,
                        ability_id: c.ability_id,
                    });
                }
            }
            // Proxy SkillHit → HitboxDamageFrame event so shape flashes red on contact.
            // For periodic / lingering abilities, don't immediately remove the hitbox
            // visual — it persists until its linger timer expires.
            CombatEventKind::SkillHit(ability_id) => {
                hitbox_dmg.send(crate::vfx::HitboxDamageFrameEvent {
                    source_entity_id: ev.source_entity,
                    ability_id: *ability_id,
                });
                let is_periodic = crate::ability_bar::all_abilities()
                    .iter()
                    .find(|a| a.id == *ability_id)
                    .map(|a| a.damage_interval_ticks > 0)
                    .unwrap_or(false);
                if !is_periodic {
                    hitbox_remove.send(crate::vfx::HitboxRemovedEvent {
                        source_entity_id: ev.source_entity,
                        ability_id: *ability_id,
                    });
                }
            }
            CombatEventKind::BuffApplied(b) => {
                use game_core::combat::status::BuffKind;
                let is_boon = crate::hud::all_buffs()
                    .iter()
                    .find(|t| t.buff_id == b.buff_id)
                    .map(|t| t.buff_kind == BuffKind::Boon)
                    .unwrap_or(true);
                buff_applied.send(crate::vfx::BuffAppliedVfxEvent {
                    target_entity_id: ev.target_entity,
                    buff_id: b.buff_id,
                    is_boon,
                });
            }
            CombatEventKind::Teleported(t) => {
                teleported.send(crate::vfx::TeleportVfxEvent {
                    entity_id: ev.source_entity,
                    from: Vec3::new(t.from_x, t.from_y, t.from_z),
                    to: Vec3::new(t.to_x, t.to_y, t.to_z),
                });
            }
            _ => {}
        }
    }
}

#[cfg(not(feature = "connected"))]
fn poll_combat_events() {}

/// Poll SpacetimeDB world_event table for new events and translate to log lines.
#[cfg(feature = "connected")]
fn poll_world_events(
    stdb_events: Option<Res<crate::spacetime::SpacetimeEvents>>,
    local_player: Res<crate::spacetime::LocalPlayerEntity>,
    mut log: ResMut<CombatLog>,
) {
    use game_client::module_bindings::*;

    let Some(events_res) = stdb_events else {
        return;
    };

    let mut new_events = Vec::new();
    while let Ok(ev) = events_res.world_event_rx.try_recv() {
        new_events.push(ev);
    }
    new_events.sort_by_key(|ev| ev.event_sequence);

    for ev in &new_events {
        let is_us = local_player.entity_id == Some(ev.entity_id);
        let entity_label = if is_us {
            "You".to_string()
        } else {
            format!("#{}", ev.entity_id)
        };

        let (text, color) = match &ev.event_kind {
            WorldEventKind::EntitySpawned(kind) => {
                let kind_str = match kind {
                    EntityKind::Player => "Player",
                    EntityKind::Npc => "NPC",
                    EntityKind::Boss => "Boss",
                    EntityKind::Projectile => "Projectile",
                    EntityKind::Hazard => "Hazard",
                    EntityKind::Prop => "Prop",
                };
                (
                    format!("{kind_str} {entity_label} spawned"),
                    Color::srgb(0.5, 0.8, 1.0),
                )
            }
            WorldEventKind::EntityDespawned => (
                format!("{entity_label} despawned"),
                Color::srgb(0.5, 0.5, 0.5),
            ),
            WorldEventKind::PickupCollected(item_id) => (
                format!("{entity_label} collected item #{item_id}"),
                Color::srgb(0.3, 1.0, 0.5),
            ),
            WorldEventKind::InteractTriggered(target_id) => (
                format!("{entity_label} interacted with #{target_id}"),
                Color::srgb(0.7, 0.9, 1.0),
            ),
        };
        log.push(text, color);
    }
}

#[cfg(not(feature = "connected"))]
fn poll_world_events() {}

#[cfg(feature = "connected")]
fn format_combat_event(
    ev: &game_client::module_bindings::CombatEvent,
    is_us_source: bool,
    is_us_target: bool,
) -> (String, Color) {
    use game_client::module_bindings::CombatEventKind;

    let src = if is_us_source {
        "You".to_string()
    } else {
        format!("#{}", ev.source_entity)
    };
    let tgt = if is_us_target {
        "You".to_string()
    } else {
        format!("#{}", ev.target_entity)
    };

    match &ev.event_kind {
        CombatEventKind::Damage(d) => {
            let color = if is_us_target {
                Color::srgb(1.0, 0.3, 0.3) // red — we took damage
            } else if is_us_source {
                Color::srgb(1.0, 1.0, 0.3) // yellow — we dealt damage
            } else {
                Color::srgb(0.7, 0.7, 0.7)
            };
            let damage_type = match d.damage_type {
                game_client::module_bindings::DamageType::Physical => "Physical",
                game_client::module_bindings::DamageType::Magical => "Magical",
                game_client::module_bindings::DamageType::True => "True",
            };
            (
                format!("{src} hit {tgt} for {:.0} {damage_type}", d.amount),
                color,
            )
        }
        CombatEventKind::Healed(h) => {
            let color = if is_us_target {
                Color::srgb(0.2, 1.0, 0.3) // green — we were healed
            } else {
                Color::srgb(0.5, 0.9, 0.5)
            };
            (format!("{src} healed {tgt} for {:.0}", h.amount), color)
        }
        CombatEventKind::SkillHit(_) => {
            // Suppressed — redundant with Damage line and CastStart.
            (String::new(), Color::srgba(0.0, 0.0, 0.0, 0.0))
        }
        CombatEventKind::EntityDied(killer) => {
            let color = if is_us_target {
                Color::srgb(1.0, 0.0, 0.0)
            } else {
                Color::srgb(0.8, 0.4, 0.0)
            };
            let killer_str = match killer {
                Some(kid) if local_player_matches(*kid, is_us_source, ev.source_entity) => {
                    "You".to_string()
                }
                Some(kid) => format!("#{kid}"),
                None => "???".to_string(),
            };
            (format!("{tgt} was killed by {killer_str}"), color)
        }
        CombatEventKind::Dodged(ability_id) => {
            let name = ability_name(*ability_id);
            (format!("{tgt} dodged {name}!"), Color::srgb(0.2, 1.0, 0.6))
        }
        CombatEventKind::Blocked(b) => {
            let name = ability_name(b.ability_id);
            let prefix = if b.perfect {
                "PERFECT BLOCK"
            } else {
                "Blocked"
            };
            (
                format!("{tgt} {prefix} {name} ({:.0} dmg)", b.damage_taken),
                Color::srgb(0.5, 0.7, 1.0),
            )
        }
        CombatEventKind::BuffApplied(b) => (
            format!(
                "{tgt} gained buff #{} ({} ticks)",
                b.buff_id, b.duration_ticks
            ),
            Color::srgb(0.3, 1.0, 0.3),
        ),
        CombatEventKind::BuffExpired(buff_id) => (
            format!("{tgt} lost buff #{buff_id}"),
            Color::srgb(0.6, 0.6, 0.6),
        ),
        CombatEventKind::TelegraphWarning(w) => (
            format!("⚠ {src} telegraph on {tgt} (impact tick {})", w.impact_tick),
            Color::srgb(1.0, 0.6, 0.0),
        ),
        CombatEventKind::LockOnAcquired => (
            format!("🎯 {src} locked on to {tgt}"),
            Color::srgb(1.0, 0.5, 0.0),
        ),
        CombatEventKind::LockOnSessionStarted(s) => {
            let name = ability_name(s.ability_id);
            (
                format!("{src} opened lock-on with {name}"),
                Color::srgb(1.0, 0.8, 0.2),
            )
        }
        CombatEventKind::LockOnCanceled(c) => {
            let canceled_target = if is_us_target || c.target == ev.target_entity {
                tgt.clone()
            } else {
                format!("#{}", c.target)
            };
            (
                format!("{src} canceled lock-on on {canceled_target}"),
                Color::srgb(0.7, 0.7, 0.7),
            )
        }
        CombatEventKind::LockOnFired(f) => (
            format!("{src} fired lock-on at {} target(s)", f.targets.len()),
            Color::srgb(1.0, 0.75, 0.25),
        ),
        CombatEventKind::CastStart(c) => {
            let name = ability_name(c.ability_id);
            let color = Color::srgb(0.8, 0.6, 1.0); // purple — cast start
            (
                format!("{src} casting {name} ({} ticks)", c.cast_duration_ticks),
                color,
            )
        }
        CombatEventKind::ChargeStart(c) => {
            let name = ability_name(c.ability_id);
            (
                format!("{src} charging {name} (max {} ticks)", c.max_ticks),
                Color::srgb(1.0, 0.8, 0.2),
            )
        }
        CombatEventKind::ChargeTierReached(c) => {
            let name = ability_name(c.ability_id);
            (
                format!("{src} {name} reached charge tier {}", c.tier),
                Color::srgb(1.0, 0.9, 0.3),
            )
        }
        CombatEventKind::BlockStart => (format!("{src} raised block"), Color::srgb(0.5, 0.7, 1.0)),
        CombatEventKind::BlockEnd => (format!("{src} dropped block"), Color::srgb(0.5, 0.6, 0.8)),
        CombatEventKind::Covered(c) => {
            let name = ability_name(c.ability_id);
            let blocker_label = if local_player_matches(c.blocker, is_us_source, ev.source_entity) {
                "You".to_string()
            } else {
                format!("#{}", c.blocker)
            };
            (
                format!(
                    "{blocker_label} covered {tgt} from {name} ({:.0} dmg)",
                    c.damage_taken
                ),
                Color::srgb(0.4, 0.8, 0.9),
            )
        }
        // Projectile events produce VFX, not log lines — return empty.
        CombatEventKind::ProjectileLaunched(_)
        | CombatEventKind::HazardSpawned(_)
        | CombatEventKind::SkillObjectRemoved(_) => {
            (String::new(), Color::srgba(0.0, 0.0, 0.0, 0.0))
        }
        CombatEventKind::Teleported(t) => (
            format!(
                "{src} teleported ({:.1}, {:.1}) -> ({:.1}, {:.1})",
                t.from_x, t.from_z, t.to_x, t.to_z,
            ),
            Color::srgb(0.4, 0.95, 1.0),
        ),
        CombatEventKind::Knockback(k) => (
            format!("{tgt} knocked back (force {:.0})", k.force),
            Color::srgb(1.0, 0.5, 0.1),
        ),
        CombatEventKind::Launched => (
            format!("{tgt} launched into the air!"),
            Color::srgb(1.0, 0.5, 0.1),
        ),
        CombatEventKind::Stunned(s) => (
            format!("{tgt} stunned ({} ticks)", s.duration_ticks),
            Color::srgb(0.9, 0.8, 0.1),
        ),
        CombatEventKind::KnockedDown(k) => (
            format!("{tgt} knocked down ({} ticks)", k.duration_ticks),
            Color::srgb(0.9, 0.7, 0.1),
        ),
        CombatEventKind::Pulled => (format!("{tgt} pulled!"), Color::srgb(1.0, 0.5, 0.1)),
        CombatEventKind::Slept(s) => (
            format!("{tgt} fell asleep ({} ticks)", s.duration_ticks),
            Color::srgb(0.6, 0.4, 0.9),
        ),
        CombatEventKind::Silenced(s) => (
            format!("{tgt} silenced ({} ticks)", s.duration_ticks),
            Color::srgb(0.7, 0.3, 0.7),
        ),
        CombatEventKind::Feared(f) => (
            format!("{tgt} feared ({} ticks)", f.duration_ticks),
            Color::srgb(0.5, 0.1, 0.5),
        ),
        CombatEventKind::StabilityConsumed(buff_id) => (
            format!("{tgt} resisted CC (stability buff #{buff_id} consumed)"),
            Color::srgb(0.2, 0.8, 1.0),
        ),
        CombatEventKind::WeaponSwapped(set) => (
            format!("{src} swapped to weapon set {set}"),
            Color::srgb(0.6, 0.9, 1.0),
        ),
        CombatEventKind::CcCleared(c) => (
            format!("{tgt} broke free of {:?}", c.cc_effect),
            Color::srgb(0.2, 1.0, 0.8),
        ),
        CombatEventKind::Cleansed(c) => (
            format!("{tgt} cleansed {} condition(s)", c.count),
            Color::srgb(0.2, 1.0, 0.8),
        ),
        CombatEventKind::Stunbreak => (format!("{src} broke free!"), Color::srgb(0.1, 1.0, 1.0)),
        CombatEventKind::CcImmune(_) => (
            format!("{tgt} is immune to CC!"),
            Color::srgb(0.5, 0.5, 0.5),
        ),
    }
}

#[cfg(feature = "connected")]
fn local_player_matches(kid: u64, is_us_source: bool, source_entity: u64) -> bool {
    is_us_source && kid == source_entity
}

fn ability_name(id: u32) -> &'static str {
    crate::ability_bar::all_abilities()
        .iter()
        .find(|a| a.id == id)
        .map(|a| a.name.as_str())
        .unwrap_or("Unknown")
}

fn update_combat_log_text(
    time: Res<Time>,
    mut log: ResMut<CombatLog>,
    mut commands: Commands,
    panel_q: Query<bevy::ecs::entity::Entity, With<CombatLogPanel>>,
    children_q: Query<&Children>,
    span_q: Query<bevy::ecs::entity::Entity, With<CombatLogSpan>>,
) {
    // Age entries and cull old ones (fade after 15s).
    let dt = time.delta_secs();
    for entry in log.entries.iter_mut() {
        entry.age += dt;
    }
    while log.entries.front().is_some_and(|e| e.age > 15.0) {
        log.entries.pop_front();
    }

    let Ok(panel_entity) = panel_q.get_single() else {
        return;
    };

    // Remove old span children.
    if let Ok(children) = children_q.get(panel_entity) {
        for &child in children.iter() {
            if span_q.get(child).is_ok() {
                commands.entity(child).despawn();
            }
        }
    }

    // Spawn a TextSpan child per log entry with its own color.
    commands.entity(panel_entity).with_children(|parent| {
        for (i, entry) in log.entries.iter().enumerate() {
            let mut line = String::new();
            if i > 0 {
                line.push('\n');
            }
            line.push_str(&entry.text);

            // Fade alpha for older entries.
            let alpha = if entry.age > 10.0 {
                ((15.0 - entry.age) / 5.0).clamp(0.0, 1.0)
            } else {
                1.0
            };
            let Color::Srgba(c) = entry.color else {
                parent.spawn((
                    TextSpan::new(line),
                    TextFont {
                        font_size: 14.0,
                        ..default()
                    },
                    TextColor(entry.color.with_alpha(alpha)),
                    CombatLogSpan,
                ));
                continue;
            };

            parent.spawn((
                TextSpan::new(line),
                TextFont {
                    font_size: 14.0,
                    ..default()
                },
                TextColor(Color::srgba(c.red, c.green, c.blue, alpha)),
                CombatLogSpan,
            ));
        }
    });
}

/// Tag for combat log text span children.
#[derive(Component)]
struct CombatLogSpan;
