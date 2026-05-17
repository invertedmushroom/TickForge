use bevy::prelude::*;
use bevy::diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin};

pub struct DiagnosticsPlugin;

impl Plugin for DiagnosticsPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(FrameTimeDiagnosticsPlugin);
        app.init_resource::<DiagnosticsState>();
        app.add_systems(Startup, spawn_diagnostics_panel);
        app.add_systems(Update, (toggle_diagnostics, update_diagnostics));
    }
}

/// Tracks diagnostic counters and panel visibility.
#[derive(Resource)]
pub struct DiagnosticsState {
    pub visible: bool,
    /// Intents sent this second (for rate display).
    pub intents_this_second: u32,
    /// Intents sent last complete second (displayed value).
    pub intents_per_second: u32,
    /// Accumulator for the 1-second window.
    pub intent_timer: f32,
    /// Ticks observed this second.
    pub ticks_this_second: u32,
    /// Ticks per second (displayed).
    pub ticks_per_second: u32,
    pub tick_timer: f32,
    /// Last tick ID forwarded to display.
    pub last_tick_id: u64,
    /// Previous tick ID to detect new ticks.
    prev_tick_id: u64,
}

impl Default for DiagnosticsState {
    fn default() -> Self {
        Self {
            visible: false,
            intents_this_second: 0,
            intents_per_second: 0,
            intent_timer: 0.0,
            ticks_this_second: 0,
            ticks_per_second: 0,
            tick_timer: 0.0,
            last_tick_id: 0,
            prev_tick_id: 0,
        }
    }
}

impl DiagnosticsState {
    /// Call each frame from systems that send intents to bump the counter.
    pub fn record_intent(&mut self) {
        self.intents_this_second += 1;
    }

    /// Call each frame with the latest tick ID to track tick rate.
    pub fn update_tick(&mut self, tick_id: u64) {
        if tick_id > self.prev_tick_id {
            self.ticks_this_second += tick_id.saturating_sub(self.prev_tick_id) as u32;
            self.prev_tick_id = tick_id;
        }
        self.last_tick_id = tick_id;
    }
}

#[derive(Component)]
struct DiagnosticsPanel;

fn spawn_diagnostics_panel(mut commands: Commands) {
    commands.spawn((
        Text::new(""),
        TextFont {
            font_size: 14.0,
            ..default()
        },
        TextColor(Color::srgba(0.0, 1.0, 0.0, 0.9)),
        Node {
            position_type: PositionType::Absolute,
            right: Val::Px(10.0),
            top: Val::Px(10.0),
            ..default()
        },
        Visibility::Hidden,
        DiagnosticsPanel,
    ));
}

fn toggle_diagnostics(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut state: ResMut<DiagnosticsState>,
    mut query: Query<&mut Visibility, With<DiagnosticsPanel>>,
) {
    if keyboard.just_pressed(KeyCode::F3) {
        state.visible = !state.visible;
        if let Ok(mut vis) = query.get_single_mut() {
            *vis = if state.visible { Visibility::Visible } else { Visibility::Hidden };
        }
    }
}

fn update_diagnostics(
    time: Res<Time>,
    diagnostics: Res<DiagnosticsStore>,
    mut state: ResMut<DiagnosticsState>,
    mut query: Query<&mut Text, With<DiagnosticsPanel>>,
    #[cfg(feature = "connected")]
    stdb: Option<Res<crate::spacetime::StdbConnection>>,
    #[cfg(feature = "connected")]
    tick_counter: Option<Res<crate::spacetime::TickCounter>>,
) {
    if !state.visible {
        return;
    }

    // Roll the 1-second windows.
    let dt = time.delta_secs();
    state.intent_timer += dt;
    state.tick_timer += dt;
    if state.intent_timer >= 1.0 {
        state.intents_per_second = state.intents_this_second;
        state.intents_this_second = 0;
        state.intent_timer -= 1.0;
    }
    if state.tick_timer >= 1.0 {
        state.ticks_per_second = state.ticks_this_second;
        state.ticks_this_second = 0;
        state.tick_timer -= 1.0;
    }

    // Update tick from TickCounter resource.
    #[cfg(feature = "connected")]
    if let Some(tc) = tick_counter {
        state.update_tick(tc.last_tick);
    }

    let fps = diagnostics
        .get(&bevy::diagnostic::FrameTimeDiagnosticsPlugin::FPS)
        .and_then(|d| d.smoothed())
        .unwrap_or(0.0);

    let mut entity_count: usize = 0;
    let mut connected = false;
    let mut last_committed: u64 = 0;
    let mut total_entities: usize = 0;
    let mut worker_count: usize = 0;
    #[cfg(feature = "connected")]
    {
        use spacetimedb_sdk::Table;
        use game_client::module_bindings::*;
        if let Some(stdb) = stdb {
            connected = stdb.connected.load(std::sync::atomic::Ordering::Relaxed);
            entity_count = stdb.conn.db.nearby_transforms().count() as usize;
            total_entities = stdb.conn.db.entity().count() as usize;
            worker_count = stdb.conn.db.trusted_worker().count() as usize;
            if let Some(cfg) = stdb.conn.db.module_config().key().find(&0) {
                last_committed = cfg.last_committed_tick;
            }
        }
    }

    let backlog = state.last_tick_id.saturating_sub(last_committed);

    let Ok(mut text) = query.get_single_mut() else { return };
    **text = format!(
        "--- Diagnostics (F3) ---\n\
         FPS: {fps:.0}\n\
         Connected: {connected}\n\
         Tick: {tick_id}\n\
         Tick rate: {tick_rate}/s\n\
         Nearby: {entities}\n\
         Total entities: {total}\n\
         Intent rate: {intent_rate}/s\n\
         Last committed: {last_committed}\n\
         Backlog: {backlog}\n\
         Workers: {workers}",
        tick_id = state.last_tick_id,
        tick_rate = state.ticks_per_second,
        entities = entity_count,
        total = total_entities,
        intent_rate = state.intents_per_second,
        workers = worker_count,
    );
}
