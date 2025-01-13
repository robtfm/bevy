use super::*;

/// An ambient light, which lights the entire scene equally.
///
/// This resource is inserted by the [`PbrPlugin`] and by default it is set to a low ambient light.
///
/// It can also be added to a camera to override the resource (or default) ambient for that camera only.
/// 
/// # Examples
///
/// Make ambient light slightly brighter:
///
/// ```
/// # use bevy_ecs::system::ResMut;
/// # use bevy_pbr::AmbientLight;
/// fn setup_ambient_light(mut ambient_light: ResMut<AmbientLight>) {
///    ambient_light.brightness = 100.0;
/// }
/// ```
#[derive(Resource, Component, Clone, Debug, ExtractResource, Reflect)]
#[reflect(Resource, Component)]
pub struct AmbientLight {
    pub color: Color,
    /// A direct scale factor multiplied with `color` before being passed to the shader.
    pub brightness: f32,
}

impl Default for AmbientLight {
    fn default() -> Self {
        Self {
            color: Color::WHITE,
            brightness: 80.0,
        }
    }
}
impl AmbientLight {
    pub const NONE: AmbientLight = AmbientLight {
        color: Color::WHITE,
        brightness: 0.0,
    };
}

impl ExtractComponent for AmbientLight {
    type QueryData = &'static Self;
    type QueryFilter = ();
    type Out = Self;

    fn extract_component(item: bevy_ecs::query::QueryItem<'_, Self::QueryData>) -> Option<Self::Out> {
        Some(item.clone())
    }
}
