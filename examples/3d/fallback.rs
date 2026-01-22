//! A simple 3D scene with light shining over a cube sitting on a plane.

use bevy::{
    pbr::{ExtendedMaterial, MaterialExtension},
    prelude::*,
};
use bevy_render::{render_asset::RenderAssetBytesPerFrame, render_resource::AsBindGroup};

fn main() {
    App::new()
        .insert_resource(RenderAssetBytesPerFrame::new(1))
        .add_plugins(DefaultPlugins)
        .add_plugins(MaterialPlugin::<StandardMaterialWithFallback>::default())
        .add_systems(Update, spawn)
        .add_systems(Update, log_events)
        .run();
}

/// set up a simple 3D scene
fn spawn(
    input: Res<ButtonInput<KeyCode>>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<
        Assets<StandardMaterial>>,
    mut fallback_materials: ResMut<
        Assets<StandardMaterialWithFallback>,
    >,
    asset_server: Res<AssetServer>,
) {
    if !input.just_pressed(KeyCode::Space) {
        return;
    }

    let small_image = asset_server.load("Burning_Ship_0002_20210818_small.png");
    let small_material = StandardMaterial {
        base_color: Color::srgba(1.0, 1.0, 1.0, 0.75),
        base_color_texture: Some(small_image),
        alpha_mode: AlphaMode::Blend,
        ..Default::default()
    };

    let big_image = asset_server.load("Burning_Ship_0002_20210818_big.png");
    let big_material = ExtendedMaterial::<FallbackMaterial<StandardMaterial>> {
        base: StandardMaterial {
            base_color_texture: Some(big_image),
            ..Default::default()
        },
        extension: FallbackMaterial {
            fallback: Some(small_material),
        },
    };
    let big_material = fallback_materials.add(big_material);

    println!("big = {}", big_material.id());

    // circular base
    commands.spawn((
        Mesh3d(meshes.add(Circle::new(4.0))),
        MeshMaterial3d(materials.add(Color::WHITE)),
        Transform::from_rotation(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)),
    ));
    // cube
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::new(1.0, 1.0, 1.0))),
        MeshMaterial3d(big_material),
        Transform::from_xyz(0.0, 0.5, 0.0),
    ));
    // light
    commands.spawn((
        PointLight {
            shadows_enabled: true,
            ..default()
        },
        Transform::from_xyz(4.0, 8.0, 4.0),
    ));
    // camera
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(-0.8333, 1.5, 3.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

fn log_events(mut evs: EventReader<AssetEvent<StandardMaterial>>) {
    for ev in evs.read() {
        error!("{ev:?}");
    }
}

#[derive(Clone, AsBindGroup, Asset, TypePath)]
struct FallbackMaterial<B: Material> {
    fallback: Option<B>,
}

impl<B: Material + Clone> MaterialExtension for FallbackMaterial<B> {
    type Base = B;

    fn fallback_asset(&self, _base: &B) -> Option<ExtendedMaterial<Self>> {
        Some(ExtendedMaterial::<Self> {
            base: self.fallback.clone()?,
            extension: FallbackMaterial { fallback: None },
        })
    }
}

type StandardMaterialWithFallback = ExtendedMaterial<FallbackMaterial<StandardMaterial>>;