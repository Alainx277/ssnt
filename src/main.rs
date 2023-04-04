#![allow(clippy::type_complexity)]

mod admin;
mod body;
mod camera;
mod components;
mod config;
mod construction;
mod event;
mod interaction;
mod items;
mod job;
mod movement;
mod round;
mod scene;
mod ui;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use admin::AdminPlugin;
use bevy::app::ScheduleRunnerSettings;
use bevy::asset::AssetPlugin;
use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy::scene::ScenePlugin;
use bevy::tasks::{AsyncComputeTaskPool, Task};
use bevy_egui::EguiPlugin;
use bevy_inspector_egui::{WorldInspectorParams, WorldInspectorPlugin};
use bevy_rapier3d::plugin::{NoUserData, RapierPhysicsPlugin};
use bevy_rapier3d::prelude::Collider;
use byond::tgm::TgmLoader;
use camera::TopDownCamera;
use clap::{Parser, Subcommand};
use futures_lite::future;
use maps::TileMapData;
use networking::identity::EntityCommandsExt as NetworkingEntityCommandsExt;
use networking::spawning::ClientControlled;
use networking::{ClientEvent, NetworkRole, NetworkingPlugin, UserData};

/// How many ticks the server runs per second
const SERVER_TPS: u32 = 60;

#[derive(Parser, Resource)]
struct Args {
    #[clap(subcommand)]
    command: Option<ArgCommands>,
}

#[derive(Subcommand)]
enum ArgCommands {
    /// host a server
    Host {
        #[clap(default_value_t = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 33998u16)))]
        bind_address: SocketAddr,
        /// overrides the public address of the server.
        /// set this when hosting behind NAT (ex. a home router)
        #[clap(long)]
        public_address: Option<IpAddr>,
    },
    /// join a game
    Join { address: SocketAddr, name: String },
}

fn main() {
    let args = Args::parse();
    let role = match args.command {
        Some(ArgCommands::Host { .. }) => NetworkRole::Server,
        Some(ArgCommands::Join { .. }) | None => NetworkRole::Client,
    };
    let networking_plugin = NetworkingPlugin { role };

    let mut app = App::new();
    app.register_type::<Player>();

    match role {
        NetworkRole::Server => {
            match config::load_server_config() {
                Ok(config) => app.insert_resource(config),
                Err(err) => {
                    error!("Error loading server configuration: {}", err);
                    return;
                }
            };

            app.insert_resource(ScheduleRunnerSettings {
                run_mode: bevy::app::RunMode::Loop {
                    wait: Some(Duration::from_secs_f64(1f64 / SERVER_TPS as f64)),
                },
            })
            .add_plugins(MinimalPlugins)
            .add_plugin(TransformPlugin)
            .add_plugin(AssetPlugin::default())
            .add_plugin(LogPlugin::default())
            .add_plugin(ScenePlugin)
            .add_plugin(HierarchyPlugin)
            .add_plugin(networking_plugin)
            .add_system(convert_tgm_map)
            .add_system(create_tilemap_from_converted)
            .add_asset::<byond::tgm::TileMap>()
            .add_asset::<Mesh>() // TODO: remove once no longer needed by rapier
            .add_asset::<Scene>() // TODO: remove once no longer needed by rapier
            // Register types used in scenes manually.
            // The server will not do anything with them, but needs it so it can load scene files.
            .register_type::<bevy::pbr::PointLight>()
            .register_type::<bevy::pbr::CubemapVisibleEntities>()
            .register_type::<bevy::render::primitives::CubemapFrusta>()
            .register_type::<bevy::render::view::Visibility>()
            .register_type::<bevy::render::view::ComputedVisibility>()
            .register_type::<Handle<bevy::pbr::StandardMaterial>>()
            .register_type::<Vec<Entity>>()
            .add_asset_loader(TgmLoader)
            .add_startup_system(setup_server)
            .add_startup_system(config::server_startup);
        }
        NetworkRole::Client => {
            app.add_plugins(DefaultPlugins)
                .add_plugin(networking_plugin)
                .add_plugin(camera::CameraPlugin)
                .add_plugin(EguiPlugin)
                .insert_resource(WorldInspectorParams {
                    enabled: true,
                    ..Default::default()
                })
                .add_plugin(WorldInspectorPlugin::new())
                .add_plugin(ui::UiPlugin)
                // .add_plugin(bevy_rapier3d::render::RapierDebugRenderPlugin::default())
                .insert_resource(ClearColor(Color::rgb(
                    44.0 / 255.0,
                    68.0 / 255.0,
                    107.0 / 255.0,
                )))
                .add_startup_system(setup_client)
                .add_system(set_camera_target)
                .add_system(clean_entities_on_disconnect)
                .add_state(GameState::Splash);
        }
    };
    app.add_plugin(RapierPhysicsPlugin::<NoUserData>::default())
        .add_plugin(physics::PhysicsPlugin)
        .add_plugin(scene::ScenePlugin)
        .add_plugin(movement::MovementPlugin)
        .add_plugin(maps::MapPlugin)
        .add_plugin(AdminPlugin)
        .add_plugin(items::ItemPlugin)
        .add_plugin(body::BodyPlugin)
        .add_plugin(round::RoundPlugin)
        .add_plugin(job::JobPlugin)
        .add_plugin(interaction::InteractionPlugin)
        .add_plugin(construction::ConstructionPlugin)
        .insert_resource(args)
        .add_startup_system(setup_shared)
        // Temporary version of https://github.com/bevyengine/bevy/pull/6578
        .register_type::<smallvec::SmallVec<[Entity; 8]>>()
        // Types that bevy doesn't register yet
        .register_type::<bevy::pbr::NotShadowCaster>()
        .run();
}

#[derive(Clone, Eq, PartialEq, Debug, Hash)]
enum GameState {
    Splash,
    MainMenu,
    Joining,
    Game,
}

/// A component that prevents an entity from being deleted when joining or leaving a server.
#[derive(Component)]
#[component(storage = "SparseSet")]
struct KeepOnServerChange;

#[derive(Component, Reflect)]
#[reflect(Component)]
pub struct Player {
    pub target_velocity: Vec2,
    pub acceleration: f32,
    pub max_acceleration_force: f32,
    pub max_velocity: f32,
    pub target_direction: Vec2,
}

impl Default for Player {
    fn default() -> Self {
        Self {
            max_velocity: 5.0,
            acceleration: 20.0,
            max_acceleration_force: 1000.0,
            target_velocity: Vec2::ZERO,
            target_direction: Vec2::ZERO,
        }
    }
}

#[derive(Clone, Resource)]
pub struct Map {
    pub handle: Handle<byond::tgm::TileMap>,
    pub spawned: bool,
}

fn setup_shared(mut commands: Commands) {
    // Spawn ground plane
    commands.spawn((
        TransformBundle::from(Transform::from_xyz(0.0, -0.5, 0.0)),
        Collider::cuboid(1000.0, 0.5, 1000.0),
        KeepOnServerChange,
    ));
}

fn setup_server(args: Res<Args>, mut commands: Commands) {
    match args.command.as_ref().unwrap() {
        &ArgCommands::Host {
            bind_address,
            public_address,
        } => {
            commands.insert_resource(networking::create_server(bind_address, public_address));
        }
        _ => panic!("Missing commandline argument"),
    };
}

fn setup_client(
    mut commands: Commands,
    args: Res<Args>,
    mut client_events: EventWriter<ClientEvent>,
    mut state: ResMut<State<GameState>>,
) {
    // TODO: Replace with on-station lights
    commands.insert_resource(AmbientLight {
        brightness: 0.01,
        ..Default::default()
    });

    let temporary_camera_target = commands.spawn(GlobalTransform::default()).id();

    commands.spawn((
        Camera3dBundle {
            transform: Transform::from_xyz(-2.0, 2.5, 5.0).looking_at(Vec3::ZERO, Vec3::Y),
            ..Default::default()
        },
        TopDownCamera::new(temporary_camera_target),
        camera::MainCamera,
        KeepOnServerChange,
    ));

    if let Some(ArgCommands::Join { address, name }) = &args.command {
        state.overwrite_set(GameState::MainMenu).unwrap();
        client_events.send(ClientEvent::Join(*address));
        commands.insert_resource(UserData {
            username: name.clone(),
        });
    }
}

/// Delete all entities when leaving a server, except entities with [`KeepOnServerChange`].
fn clean_entities_on_disconnect(
    mut events: EventReader<ClientEvent>,
    to_delete: Query<Entity, (Without<Parent>, Without<KeepOnServerChange>)>,
    mut commands: Commands,
) {
    let has_disconnected = events
        .iter()
        .any(|e| matches!(e, ClientEvent::Disconnected(_)));
    if !has_disconnected {
        return;
    }

    // TODO: Optimize deletion?
    for entity in to_delete.iter() {
        commands.entity(entity).despawn_recursive();
    }
}

fn set_camera_target(
    query: Query<Entity, Added<ClientControlled>>,
    mut camera: Query<&mut TopDownCamera, Without<ClientControlled>>,
) {
    for entity in query.iter() {
        if let Ok(mut camera) = camera.get_single_mut() {
            camera.target = entity;
        }
    }
}

#[derive(Component)]
struct ConvertByondMap(Task<TileMapData>);

fn convert_tgm_map(
    mut commands: Commands,
    map_resource: Option<ResMut<Map>>,
    tilemaps: Res<Assets<byond::tgm::TileMap>>,
) {
    if let Some(res) = map_resource {
        if let Some(map) = tilemaps.get(&res.handle) {
            let map_copy = byond::tgm::TileMap::clone(map);
            let thread_pool = AsyncComputeTaskPool::get();
            let task =
                thread_pool.spawn(async move { byond::tgm::conversion::to_map_data(&map_copy) });
            let new_entity = commands.spawn(ConvertByondMap(task)).id();
            info!("Scheduled tgm map conversion (entity={:?})", new_entity);
            commands.remove_resource::<Map>();
        }
    }
}

fn create_tilemap_from_converted(
    mut commands: Commands,
    mut map_tasks: Query<(Entity, &mut ConvertByondMap)>,
) {
    for (entity, mut map_task) in map_tasks.iter_mut() {
        if let Some(map_data) = future::block_on(future::poll_once(&mut map_task.0)) {
            commands
                .entity(entity)
                .remove::<ConvertByondMap>()
                .insert((map_data, SpatialBundle::default()))
                .networked();
            info!("Map conversion finished and applied (entity={:?})", entity);
        }
    }
}
