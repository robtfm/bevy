//! Material bind group management for bindless resources.
//!
//! In bindless mode, Bevy's renderer groups materials into small bind groups.
//! This allocator manages each bind group, assigning slots to materials as
//! appropriate.

use crate::Material;
use bevy_derive::{Deref, DerefMut};
use bevy_ecs::{
    system::Resource,
    world::{FromWorld, World},
};
use bevy_reflect::{std_traits::ReflectDefault, Reflect};
use bevy_render::{
    render_resource::{
        BindGroup, BindGroupEntry, BindGroupLayout, BindGroupLayoutEntry, BindingResource,
        BindingType, Buffer, BufferBinding, BufferInitDescriptor, BufferUsages,
        OwnedBindingResource, Sampler, SamplerDescriptor, TextureViewDimension,
        UnpreparedBindGroup, WgpuSampler, WgpuTextureView,
    },
    renderer::RenderDevice,
    settings::WgpuFeatures,
    texture::FallbackImage,
};
use bevy_utils::{default, tracing::error, HashMap};
use core::any;
use core::iter;
use core::marker::PhantomData;
use core::num::NonZero;

/// An object that creates and stores bind groups for a single material type.
///
/// This object collects bindless materials into groups as appropriate and
/// assigns slots as materials are created.
#[derive(Resource)]
pub struct MaterialBindGroupAllocator<M>
where
    M: Material,
{
    /// The data that the allocator keeps about each bind group.
    pub bind_groups: Vec<MaterialBindGroup<M>>,

    /// Stores IDs of material bind groups that have at least one slot
    /// available.
    free_bind_groups: Vec<u32>,

    /// The layout for this bind group.
    bind_group_layout: BindGroupLayout,

    /// Dummy buffers that are assigned to unused slots.
    fallback_buffers: MaterialFallbackBuffers,

    /// Whether this material is actually using bindless resources.
    ///
    /// This takes the availability of bindless resources on this platform into
    /// account.
    bindless_enabled: bool,

    phantom: PhantomData<M>,
}

/// Information that the allocator keeps about each bind group.
pub struct MaterialBindGroup<M>
where
    M: Material,
{
    /// The actual bind group.
    pub bind_group: Option<BindGroup>,

    /// The bind group data for each slot.
    ///
    /// This is `None` if the slot is unallocated and `Some` if the slot is
    /// full.
    unprepared_bind_groups: Vec<Option<UnpreparedBindGroup<M::Data>>>,

    /// A bitfield that contains a 0 if the slot is free or a 1 if the slot is
    /// full.
    ///
    /// We keep this value so that we can quickly find the next free slot when
    /// we go to allocate.
    used_slot_bitmap: u32,
}

/// Where the GPU data for a material is located.
///
/// In bindless mode, materials are gathered into bind groups, and the slot is
/// necessary to locate the material data within that group. If not in bindless
/// mode, bind groups and materials are in 1:1 correspondence, and the slot
/// index is always 0.
#[derive(Clone, Copy, Debug, Default, Reflect)]
pub struct MaterialBindingId {
    /// The index of the bind group (slab) where the GPU data is located.
    pub group: MaterialBindGroupIndex,
    /// The slot within that bind group.
    pub slot: MaterialBindGroupSlot,
}

/// The index of each material bind group.
///
/// In bindless mode, each bind group contains multiple materials. In
/// non-bindless mode, each bind group contains only one material.
#[derive(Clone, Copy, Debug, Default, Reflect, PartialEq, Deref, DerefMut)]
#[reflect(Default)]
pub struct MaterialBindGroupIndex(pub u32);

impl From<u32> for MaterialBindGroupIndex {
    fn from(value: u32) -> Self {
        MaterialBindGroupIndex(value)
    }
}

/// The index of the slot containing material data within each material bind
/// group.
///
/// In bindless mode, this slot is needed to locate the material data in each
/// bind group, since multiple materials are packed into a single slab. In
/// non-bindless mode, this slot is always 0.
#[derive(Clone, Copy, Debug, Default, Reflect, Deref, DerefMut)]
#[reflect(Default)]
pub struct MaterialBindGroupSlot(pub u32);

impl From<u32> for MaterialBindGroupSlot {
    fn from(value: u32) -> Self {
        MaterialBindGroupSlot(value)
    }
}

/// A temporary data structure that contains references to bindless resources.
///
/// We need this because the `wgpu` bindless API takes a slice of references.
/// Thus we need to create intermediate vectors of bindless resources in order
/// to satisfy the lifetime requirements.
enum BindingResourceArray<'a> {
    Buffers(Vec<BufferBinding<'a>>),
    TextureViews(TextureViewDimension, Vec<&'a WgpuTextureView>),
    Samplers(Vec<&'a WgpuSampler>),
}

/// Contains dummy resources that we use to pad out bindless arrays.
///
/// On DX12, every binding array slot must be filled, so we have to fill unused
/// slots.
#[derive(Resource)]
pub struct FallbackBindlessResources {
    /// A dummy sampler that we fill unused slots in bindless sampler arrays
    /// with.
    fallback_sampler: Sampler,
}

struct MaterialFallbackBuffers(HashMap<u32, Buffer>);

/// The minimum byte size of each fallback buffer.
const MIN_BUFFER_SIZE: u64 = 16;

impl<M> MaterialBindGroupAllocator<M>
where
    M: Material,
{
    /// Creates or recreates any bind groups that were modified this frame.
    pub(crate) fn prepare_bind_groups(
        &mut self,
        render_device: &RenderDevice,
        fallback_image: &FallbackImage,
        fallback_resources: &FallbackBindlessResources,
    ) {
        for bind_group in &mut self.bind_groups {
            bind_group.rebuild_bind_group_if_necessary(
                render_device,
                &self.bind_group_layout,
                fallback_image,
                fallback_resources,
                &self.fallback_buffers,
                self.bindless_enabled,
            );
        }
    }

    /// Returns the bind group with the given index, if it exists.
    #[inline]
    pub(crate) fn get(&self, index: MaterialBindGroupIndex) -> Option<&MaterialBindGroup<M>> {
        self.bind_groups.get(index.0 as usize)
    }

    /// Allocates a new binding slot and returns its ID.
    pub(crate) fn allocate(&mut self) -> MaterialBindingId {
        let group_index = self.free_bind_groups.pop().unwrap_or_else(|| {
            let group_index = self.bind_groups.len() as u32;
            self.bind_groups
                .push(MaterialBindGroup::new(self.bindless_enabled));
            group_index
        });

        let bind_group = &mut self.bind_groups[group_index as usize];
        let slot_index = bind_group.allocate();

        if !bind_group.is_full() {
            self.free_bind_groups.push(group_index);
        }

        MaterialBindingId {
            group: group_index.into(),
            slot: slot_index,
        }
    }

    /// Assigns an unprepared bind group to the group and slot specified in the
    /// [`MaterialBindingId`].
    pub(crate) fn init(
        &mut self,
        material_binding_id: MaterialBindingId,
        unprepared_bind_group: UnpreparedBindGroup<M::Data>,
    ) {
        self.bind_groups[material_binding_id.group.0 as usize]
            .init(material_binding_id.slot, unprepared_bind_group);
    }

    /// Marks the slot corresponding to the given [`MaterialBindingId`] as free.
    pub(crate) fn free(&mut self, material_binding_id: MaterialBindingId) {
        let bind_group = &mut self.bind_groups[material_binding_id.group.0 as usize];
        let was_full = bind_group.is_full();

        bind_group.free(material_binding_id.slot);

        // If the group that this material belonged to was full, it now contains
        // at least one free slot, so add the group to the `free_bind_groups`
        // list.
        if was_full {
            debug_assert!(!self.free_bind_groups.contains(&material_binding_id.group.0));
            self.free_bind_groups.push(*material_binding_id.group);
        }
    }
}

impl<M> MaterialBindGroup<M>
where
    M: Material,
{
    /// Returns a new bind group.
    fn new(bindless_enabled: bool) -> MaterialBindGroup<M> {
        let count = if !bindless_enabled {
            1
        } else {
            M::BINDLESS_SLOT_COUNT.unwrap_or(1)
        };

        MaterialBindGroup {
            bind_group: None,
            unprepared_bind_groups: iter::repeat_with(|| None).take(count as usize).collect(),
            used_slot_bitmap: 0,
        }
    }

    /// Allocates a new slot and returns its index.
    ///
    /// This bind group must not be full.
    fn allocate(&mut self) -> MaterialBindGroupSlot {
        debug_assert!(!self.is_full());

        // Mark the slot as used.
        let slot = self.used_slot_bitmap.trailing_ones();
        self.used_slot_bitmap |= 1 << slot;

        slot.into()
    }

    /// Assigns the given unprepared bind group to the given slot.
    fn init(
        &mut self,
        slot: MaterialBindGroupSlot,
        unprepared_bind_group: UnpreparedBindGroup<M::Data>,
    ) {
        self.unprepared_bind_groups[slot.0 as usize] = Some(unprepared_bind_group);

        // Invalidate the cached bind group so that we rebuild it again.
        self.bind_group = None;
    }

    /// Marks the given slot as free.
    fn free(&mut self, slot: MaterialBindGroupSlot) {
        self.unprepared_bind_groups[slot.0 as usize] = None;
        self.used_slot_bitmap &= !(1 << slot.0);

        // Invalidate the cached bind group so that we rebuild it again.
        self.bind_group = None;
    }

    /// Returns true if all the slots are full or false if at least one slot in
    /// this bind group is free.
    fn is_full(&self) -> bool {
        self.used_slot_bitmap == (1 << (self.unprepared_bind_groups.len() as u32)) - 1
    }

    /// Returns the actual bind group, or `None` if it hasn't been created yet.
    pub fn get_bind_group(&self) -> Option<&BindGroup> {
        self.bind_group.as_ref()
    }

    /// Recreates the bind group for this material bind group containing the
    /// data for every material in it.
    fn rebuild_bind_group_if_necessary(
        &mut self,
        render_device: &RenderDevice,
        bind_group_layout: &BindGroupLayout,
        fallback_image: &FallbackImage,
        fallback_bindless_resources: &FallbackBindlessResources,
        fallback_buffers: &MaterialFallbackBuffers,
        bindless_enabled: bool,
    ) {
        if self.bind_group.is_some() {
            return;
        }

        let Some(first_bind_group) = self
            .unprepared_bind_groups
            .iter()
            .find_map(|slot| slot.as_ref())
        else {
            return;
        };

        // If bindless isn't enabled, create a trivial bind group containing
        // only one material's worth of data.
        if !bindless_enabled {
            let entries = first_bind_group
                .bindings
                .iter()
                .map(|(index, binding)| BindGroupEntry {
                    binding: *index,
                    resource: binding.get_binding(),
                })
                .collect::<Vec<_>>();

            self.bind_group =
                Some(render_device.create_bind_group(M::label(), bind_group_layout, &entries));
            return;
        }

        // Creates the intermediate binding resource vectors.
        let Some(binding_resource_arrays) = self.recreate_binding_resource_arrays(
            first_bind_group,
            fallback_image,
            fallback_bindless_resources,
            fallback_buffers,
        ) else {
            return;
        };

        // Now build the actual resource arrays for `wgpu`.
        let entries = binding_resource_arrays
            .iter()
            .map(|&(&binding, ref binding_resource_array)| BindGroupEntry {
                binding,
                resource: match *binding_resource_array {
                    BindingResourceArray::Buffers(ref vec) => {
                        BindingResource::BufferArray(&vec[..])
                    }
                    BindingResourceArray::TextureViews(_, ref vec) => {
                        BindingResource::TextureViewArray(&vec[..])
                    }
                    BindingResourceArray::Samplers(ref vec) => {
                        BindingResource::SamplerArray(&vec[..])
                    }
                },
            })
            .collect::<Vec<_>>();

        self.bind_group =
            Some(render_device.create_bind_group(M::label(), bind_group_layout, &entries));
    }

    /// Recreates the binding arrays for each material in this bind group.
    fn recreate_binding_resource_arrays<'a>(
        &'a self,
        first_bind_group: &'a UnpreparedBindGroup<M::Data>,
        fallback_image: &'a FallbackImage,
        fallback_bindless_resources: &'a FallbackBindlessResources,
        fallback_buffers: &'a MaterialFallbackBuffers,
    ) -> Option<Vec<(&'a u32, BindingResourceArray<'a>)>> {
        // Initialize the arrays.
        let mut binding_resource_arrays = first_bind_group
            .bindings
            .iter()
            .map(|(index, binding)| match *binding {
                OwnedBindingResource::Buffer(..) => (index, BindingResourceArray::Buffers(vec![])),
                OwnedBindingResource::TextureView(dimension, _) => {
                    (index, BindingResourceArray::TextureViews(dimension, vec![]))
                }
                OwnedBindingResource::Sampler(..) => {
                    (index, BindingResourceArray::Samplers(vec![]))
                }
            })
            .collect::<Vec<_>>();

        for maybe_unprepared_bind_group in self.unprepared_bind_groups.iter() {
            match *maybe_unprepared_bind_group {
                None => {
                    // Push dummy resources for this slot.
                    for binding_resource_array in &mut binding_resource_arrays {
                        match *binding_resource_array {
                            (binding, BindingResourceArray::Buffers(ref mut vec)) => {
                                vec.push(BufferBinding {
                                    buffer: &fallback_buffers.0[binding],
                                    offset: 0,
                                    size: None,
                                });
                            }
                            (
                                _,
                                BindingResourceArray::TextureViews(texture_dimension, ref mut vec),
                            ) => vec.push(&fallback_image.get(texture_dimension).texture_view),
                            (_, BindingResourceArray::Samplers(ref mut vec)) => {
                                vec.push(&fallback_bindless_resources.fallback_sampler);
                            }
                        }
                    }
                }

                Some(ref unprepared_bind_group) => {
                    // Push the resources for this slot.
                    //
                    // All materials in this group must have the same type of
                    // binding (buffer, texture view, sampler) in each bind
                    // group entry.
                    for (
                        binding_index,
                        (&mut (binding, ref mut binding_resource_array), (_, binding_resource)),
                    ) in binding_resource_arrays
                        .iter_mut()
                        .zip(unprepared_bind_group.bindings.0.iter())
                        .enumerate()
                    {
                        match (binding_resource_array, binding_resource) {
                            (
                                &mut BindingResourceArray::Buffers(ref mut vec),
                                OwnedBindingResource::Buffer(buffer),
                            ) => match NonZero::new(buffer.size()) {
                                None => vec.push(BufferBinding {
                                    buffer: &fallback_buffers.0[binding],
                                    offset: 0,
                                    size: None,
                                }),
                                Some(size) => vec.push(BufferBinding {
                                    buffer,
                                    offset: 0,
                                    size: Some(size),
                                }),
                            },
                            (
                                &mut BindingResourceArray::TextureViews(_, ref mut vec),
                                OwnedBindingResource::TextureView(_, texture_view),
                            ) => vec.push(texture_view),
                            (
                                &mut BindingResourceArray::Samplers(ref mut vec),
                                OwnedBindingResource::Sampler(sampler),
                            ) => vec.push(sampler),
                            _ => {
                                error!(
                                    "Mismatched bind group layouts for material \
                                    {} at bind group {}; can't combine bind \
                                    groups into a single bindless bind group!",
                                    any::type_name::<M>(),
                                    binding_index,
                                );
                                return None;
                            }
                        }
                    }
                }
            }
        }

        Some(binding_resource_arrays)
    }

    /// Returns the associated extra data for the material with the given slot.
    pub fn get_extra_data(&self, slot: MaterialBindGroupSlot) -> &M::Data {
        &self.unprepared_bind_groups[slot.0 as usize]
            .as_ref()
            .unwrap()
            .data
    }
}

impl<M> FromWorld for MaterialBindGroupAllocator<M>
where
    M: Material,
{
    fn from_world(world: &mut World) -> Self {
        // Create a new bind group allocator.
        let render_device = world.resource::<RenderDevice>();
        let bind_group_layout_entries = M::bind_group_layout_entries(render_device);
        let bind_group_layout =
            render_device.create_bind_group_layout(M::label(), &bind_group_layout_entries);
        let fallback_buffers =
            MaterialFallbackBuffers::new(render_device, &bind_group_layout_entries);
        MaterialBindGroupAllocator {
            bind_groups: vec![],
            free_bind_groups: vec![],
            bind_group_layout,
            fallback_buffers,
            bindless_enabled: material_uses_bindless_resources::<M>(render_device),
            phantom: PhantomData,
        }
    }
}

/// Returns true if the material will *actually* use bindless resources or false
/// if it won't.
///
/// This takes the platform support (or lack thereof) for bindless resources
/// into account.
pub fn material_uses_bindless_resources<M>(render_device: &RenderDevice) -> bool
where
    M: Material,
{
    M::BINDLESS_SLOT_COUNT.is_some()
        && render_device
            .features()
            .contains(WgpuFeatures::BUFFER_BINDING_ARRAY | WgpuFeatures::TEXTURE_BINDING_ARRAY)
}

impl FromWorld for FallbackBindlessResources {
    fn from_world(world: &mut World) -> Self {
        let render_device = world.resource::<RenderDevice>();
        FallbackBindlessResources {
            fallback_sampler: render_device.create_sampler(&SamplerDescriptor {
                label: Some("fallback sampler"),
                ..default()
            }),
        }
    }
}

impl MaterialFallbackBuffers {
    /// Creates a new set of fallback buffers containing dummy allocations.
    ///
    /// We populate unused bind group slots with these.
    fn new(
        render_device: &RenderDevice,
        bind_group_layout_entries: &[BindGroupLayoutEntry],
    ) -> MaterialFallbackBuffers {
        let mut fallback_buffers = HashMap::new();
        for bind_group_layout_entry in bind_group_layout_entries {
            // Create a dummy buffer of the appropriate size.
            let BindingType::Buffer {
                min_binding_size, ..
            } = bind_group_layout_entry.ty
            else {
                continue;
            };
            let mut size: u64 = match min_binding_size {
                None => 0,
                Some(min_binding_size) => min_binding_size.into(),
            };
            size = size.max(MIN_BUFFER_SIZE);

            fallback_buffers.insert(
                bind_group_layout_entry.binding,
                render_device.create_buffer_with_data(&BufferInitDescriptor {
                    label: Some("fallback buffer"),
                    contents: &vec![0; size as usize],
                    usage: BufferUsages::UNIFORM | BufferUsages::STORAGE,
                }),
            );
        }

        MaterialFallbackBuffers(fallback_buffers)
    }
}
