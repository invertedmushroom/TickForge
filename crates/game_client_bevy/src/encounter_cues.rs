use bevy::prelude::*;
use std::collections::HashMap;

pub struct EncounterCuesPlugin;

impl Plugin for EncounterCuesPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ActiveEncounterCues>();
        app.add_systems(Update, expire_encounter_cues);
    }
}

/// A single live encounter cue with decoded geometry.
#[derive(Clone, Debug)]
#[allow(dead_code)] // pos/half_height/starts_at_tick reserved for future debug use
pub struct ActiveCueEntry {
    pub cue_id: String,
    pub anchor_entity: Option<u64>,
    pub pos: Vec3,
    pub inner_radius: f32,
    pub outer_radius: f32,
    pub half_height: f32,
    pub starts_at_tick: u64,
    pub expires_at_tick: u64,
}

/// Bevy resource: map of cue_id → live cue, populated from CombatEvent stream.
#[derive(Resource, Default)]
pub struct ActiveEncounterCues {
    pub cues: HashMap<String, ActiveCueEntry>,
}

impl ActiveEncounterCues {
    pub fn insert(&mut self, entry: ActiveCueEntry) {
        self.cues.insert(entry.cue_id.clone(), entry);
    }
}

/// Remove cues whose `expires_at_tick` has passed.
fn expire_encounter_cues(
    mut cues: ResMut<ActiveEncounterCues>,
    tick_counter: Option<Res<crate::spacetime::TickCounter>>,
) {
    let current_tick = tick_counter.map(|tc| tc.last_tick).unwrap_or(0);
    if current_tick == 0 {
        return;
    }
    cues.cues
        .retain(|_, entry| entry.expires_at_tick > current_tick);
}
