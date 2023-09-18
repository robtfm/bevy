//! Demonstrates using a custom extension to the `StandardMaterial` to modify the results of the builtin pbr shader.

use bevy::{pbr::ExtendedMaterial, prelude::*, render::render_resource::*};
use bevy_internal::reflect::TypePath;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(MaterialPlugin::<ExtendedMaterial<MyExtendedMaterial>>::default())
        .add_systems(Startup, setup)
        .add_systems(Update, rotate_things)
        .run();
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ExtendedMaterial<MyExtendedMaterial>>>,
) {
    // sphere
    commands.spawn(MaterialMeshBundle {
        mesh: meshes.add(
            Mesh::try_from(shape::Icosphere {
                radius: 1.0,
                subdivisions: 5,
            })
            .unwrap(),
        ),
        transform: Transform::from_xyz(0.0, 0.5, 0.0),
        material: materials.add(ExtendedMaterial {
            standard: StandardMaterial {
                base_color: Color::RED,
                ..Default::default()
            },
            extended: MyExtendedMaterial { quantize_steps: 3 },
        }),
        ..default()
    });

    // light
    commands.spawn((PointLightBundle::default(), Rotate));

    // camera
    commands.spawn(Camera3dBundle {
        transform: Transform::from_xyz(-2.0, 2.5, 5.0).looking_at(Vec3::ZERO, Vec3::Y),
        ..default()
    });
}

#[derive(Component)]
struct Rotate;

fn rotate_things(mut q: Query<&mut Transform, With<Rotate>>, time: Res<Time>) {
    for mut t in q.iter_mut() {
        t.translation = Vec3::new(
            time.elapsed_seconds().sin(),
            0.5,
            time.elapsed_seconds().cos(),
        ) * 4.0;
    }
}

#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
struct MyExtendedMaterial {
    #[uniform(100)]
    quantize_steps: u32,
}

impl Material for MyExtendedMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/quantize_shader.wgsl".into()
    }
}