use bevy::{
    ecs::system::EntityCommands,
    input::Input,
    math::{Mat4, Vec2, Vec3},
    pbr::PbrBundle,
    prelude::*,
    transform::TransformBundle,
    utils::HashMap,
    window::Windows,
};
use bevy_egui::{egui::Window, EguiContext};
use bevy_rapier3d::{
    plugin::RapierContext,
    prelude::{Collider, RigidBody, Velocity},
    rapier::prelude::ColliderShape,
};
use networking::{
    identity::{EntityCommandsExt, NetworkIdentities, NetworkIdentity},
    messaging::{AppExt, MessageEvent, MessageReceivers, MessageSender},
    spawning::{PrefabPath, ServerEntityEvent, SpawningSystems},
    transform::{NetworkTransform, NetworkedTransform},
    NetworkManager,
};
use serde::{Deserialize, Serialize};

use crate::{camera::MainCamera, GameState};

#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum Spawnable {
    Cube,
    Sphere,
}

struct SpawnableDefinition {
    mesh: Handle<Mesh>,
    shape: ColliderShape,
}

#[derive(Resource)]
struct SpawnerAssets {
    spawnables: HashMap<Spawnable, SpawnableDefinition>,
}

fn load_spawner_assets(mut commands: Commands, mut meshes: Option<ResMut<Assets<Mesh>>>) {
    let cube_mesh = meshes
        .as_mut()
        .map(|m| m.add(Mesh::from(shape::Cube::default())));
    let sphere_mesh = meshes.as_mut().map(|m| {
        m.add(Mesh::from(shape::UVSphere {
            sectors: 128,
            stacks: 64,
            ..Default::default()
        }))
    });

    let mut spawnables: HashMap<Spawnable, SpawnableDefinition> = Default::default();
    spawnables.insert(
        Spawnable::Cube,
        SpawnableDefinition {
            mesh: cube_mesh.unwrap_or_default(),
            shape: ColliderShape::cuboid(0.5, 0.5, 0.5),
        },
    );
    spawnables.insert(
        Spawnable::Sphere,
        SpawnableDefinition {
            mesh: sphere_mesh.unwrap_or_default(),
            shape: ColliderShape::ball(1.0),
        },
    );

    commands.insert_resource(SpawnerAssets { spawnables });
}

#[derive(Default, Resource)]
struct SpawnerUiState {
    to_spawn: Option<Spawnable>,
}

fn spawning_ui(mut egui_context: ResMut<EguiContext>, mut state: ResMut<SpawnerUiState>) {
    Window::new("Spawning").show(egui_context.ctx_mut(), |ui| {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut state.to_spawn, None, "None");
            ui.selectable_value(&mut state.to_spawn, Some(Spawnable::Cube), "Cube");
            ui.selectable_value(&mut state.to_spawn, Some(Spawnable::Sphere), "Sphere");
        });
    });
}

#[derive(Serialize, Deserialize, Clone)]
enum SpawnerMessage {
    Request((Vec3, Spawnable)),
    Spawned((NetworkIdentity, Spawnable)),
}

#[allow(clippy::too_many_arguments)]
fn spawn_requesting(
    ui_state: Res<SpawnerUiState>,
    buttons: Res<Input<MouseButton>>,
    mut context: ResMut<EguiContext>,
    rapier_context: Res<RapierContext>,
    windows: Res<Windows>,
    cameras: Query<(&Camera, &GlobalTransform), With<MainCamera>>,
    mut sender: MessageSender,
) {
    if ui_state.to_spawn.is_none() {
        return;
    }

    if !buttons.just_pressed(MouseButton::Left) {
        return;
    }

    let window = match windows.get_primary() {
        Some(w) => w,
        None => return,
    };

    if context
        .try_ctx_for_window_mut(window.id())
        .map(|c| c.wants_pointer_input())
        == Some(true)
    {
        return;
    }

    let (camera, camera_transform) = match cameras.iter().next() {
        Some(o) => o,
        None => return,
    };
    let cursor_position = match window.cursor_position() {
        Some(p) => p,
        None => return,
    };

    let (origin, direction) = match ray_from_cursor(cursor_position, camera, camera_transform) {
        Some(r) => r,
        None => return,
    };

    if let Some((_, toi)) =
        rapier_context.cast_ray(origin, direction, 100.0, true, Default::default())
    {
        let hit_point = origin + direction * toi;
        info!(position=?hit_point, "Requesting object spawn");
        sender.send_to_server(&SpawnerMessage::Request((
            hit_point,
            ui_state.to_spawn.unwrap(),
        )));
    }
}

fn create_spawnable(
    commands: &mut EntityCommands,
    kind: Spawnable,
    assets: &SpawnerAssets,
    position: Vec3,
) {
    let definition = assets.spawnables.get(&kind).unwrap();

    commands.insert((
        RigidBody::Dynamic,
        Velocity::default(),
        Collider::from(definition.shape.clone()),
        TransformBundle::from(Transform::from_translation(position)),
        kind,
    ));
}

fn handle_spawn_request(
    mut messages: EventReader<MessageEvent<SpawnerMessage>>,
    mut commands: Commands,
    assets: Res<SpawnerAssets>,
) {
    for event in messages.iter() {
        if let SpawnerMessage::Request((position, kind)) = event.message {
            let mut builder = commands.spawn_empty();
            create_spawnable(&mut builder, kind, &assets, position);
            builder
                .insert((
                    PrefabPath("spawnable".to_owned()),
                    NetworkTransform::default(),
                ))
                .networked();
        }
    }
}

fn send_spawned_type(
    mut events: EventReader<ServerEntityEvent>,
    spawnables: Query<(&Spawnable, &NetworkIdentity)>,
    mut sender: MessageSender,
) {
    for event in events.iter() {
        if let ServerEntityEvent::Spawned((entity, connection)) = event {
            let (spawnable, identity) = match spawnables.get(*entity) {
                Ok(s) => s,
                Err(_) => continue,
            };

            sender.send(
                &SpawnerMessage::Spawned((*identity, *spawnable)),
                MessageReceivers::Single(*connection),
            );
        }
    }
}

fn receive_spawned_type(
    mut events: EventReader<MessageEvent<SpawnerMessage>>,
    identities: Res<NetworkIdentities>,
    mut commands: Commands,
    assets: Res<SpawnerAssets>,
) {
    for event in events.iter() {
        if let SpawnerMessage::Spawned((identity, spawnable)) = event.message {
            let entity = match identities.get_entity(identity) {
                Some(e) => e,
                None => {
                    warn!("Received spawned type for non-existent {:?}", identity);
                    continue;
                }
            };

            let mut builder = commands.entity(entity);
            create_spawnable(&mut builder, spawnable, &assets, Vec3::ZERO);
            builder.insert((
                NetworkedTransform::default(),
                PbrBundle {
                    mesh: assets.spawnables.get(&spawnable).unwrap().mesh.clone(),
                    ..Default::default()
                },
            ));
        }
    }
}

pub(crate) struct SpawningPlugin;

impl Plugin for SpawningPlugin {
    fn build(&self, app: &mut App) {
        app.add_network_message::<SpawnerMessage>()
            .add_startup_system(load_spawner_assets);

        if app
            .world
            .get_resource::<NetworkManager>()
            .unwrap()
            .is_server()
        {
            app.add_system(handle_spawn_request)
                .add_system(send_spawned_type.after(SpawningSystems::Spawn));
        } else {
            app.init_resource::<SpawnerUiState>()
                .add_system_set(
                    SystemSet::on_update(GameState::Game)
                        .with_system(spawning_ui.label("admin spawn ui")),
                )
                .add_system(spawn_requesting.after("admin spawn ui"))
                .add_system(receive_spawned_type.after(SpawningSystems::Spawn));
        }
    }
}

// Taken from https://github.com/aevyrie/bevy_mod_raycast/blob/51d9e2c99066ea769db27c0ae79d11b258fcef4f/src/primitives.rs#L192
pub fn ray_from_cursor(
    cursor_pos_screen: Vec2,
    camera: &Camera,
    camera_transform: &GlobalTransform,
) -> Option<(Vec3, Vec3)> {
    let view = camera_transform.compute_matrix();
    let screen_size = camera.logical_target_size()?;
    let projection = camera.projection_matrix();
    let far_ndc = projection.project_point3(Vec3::NEG_Z).z;
    let near_ndc = projection.project_point3(Vec3::Z).z;
    let cursor_ndc = (cursor_pos_screen / screen_size) * 2.0 - Vec2::ONE;
    let ndc_to_world: Mat4 = view * projection.inverse();
    let near = ndc_to_world.project_point3(cursor_ndc.extend(near_ndc));
    let far = ndc_to_world.project_point3(cursor_ndc.extend(far_ndc));
    let ray_direction = far - near;
    Some((near, ray_direction))
}
