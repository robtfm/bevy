use bevy_utils::all_tuples_with_size;
use wgpu::{BindGroupEntry, BindingResource};

use super::{Sampler, TextureView};

/// Helper for constructing bindgroups.
///
/// Allows constructing the descriptor's entries as:
/// ```
/// render_device.create_bind_group(
///     Some("my_bind_group"),
///     &my_layout,
///     BindGroupEntries::with_indexes((
///         (2, &my_sampler),
///         (3, my_uniform),
///     )).as_slice(),
/// );
/// ```
///
/// instead of
///
/// ```
/// render_device.create_bind_group(
///     Some("my_bind_group"),
///     &my_layout,
///     &[
///         BindGroupEntry {
///             binding: 2,
///             resource: BindingResource::Sampler(&my_sampler),
///         },
///         BindGroupEntry {
///             binding: 3,
///             resource: my_uniform,
///         },
///     ],
/// );
/// ```
///
/// or
///
/// ```
/// render_device.create_bind_group(
///     Some("my_bind_group"),
///     &my_layout,
///     BindGroupEntries::sequential((
///         &my_sampler,
///         my_uniform,
///     )).as_slice(),
/// );
/// ```
///
/// instead of
///
/// ```
/// render_device.create_bind_group(&BindGroupDescriptor {
///     Some("my_bind_group"),
///     &my_layout,
///     &[
///         BindGroupEntry {
///             binding: 0,
///             resource: BindingResource::Sampler(&my_sampler),
///         },
///         BindGroupEntry {
///             binding: 1,
///             resource: my_uniform,
///         },
///     ],
/// );
/// ```

pub struct BindGroupEntries<'b, const N: usize> {
    entries: [BindGroupEntry<'b>; N],
}

impl<'b, const N: usize> BindGroupEntries<'b, N> {
    #[inline]
    pub fn sequential(resources: impl IntoBindingArray<'b, N>) -> Self {
        let mut i = 0;
        Self {
            entries: resources.into_array().map(|resource| {
                let binding = i;
                i += 1;
                BindGroupEntry { binding, resource }
            }),
        }
    }

    #[inline]
    pub fn with_indexes(indexed_resources: impl IntoIndexedBindingArray<'b, N>) -> Self {
        Self {
            entries: indexed_resources
                .into_array()
                .map(|(binding, resource)| BindGroupEntry { binding, resource }),
        }
    }
}

impl<'b, const N: usize> std::ops::Deref for BindGroupEntries<'b, N> {
    type Target = [BindGroupEntry<'b>];

    fn deref(&self) -> &[BindGroupEntry<'b>] {
        &self.entries
    }
}

pub trait IntoBinding<'a> {
    fn into_binding(self) -> BindingResource<'a>;
}

impl<'a> IntoBinding<'a> for &'a TextureView {
    #[inline]
    fn into_binding(self) -> BindingResource<'a> {
        BindingResource::TextureView(self)
    }
}

impl<'a> IntoBinding<'a> for &'a [&'a wgpu::TextureView] {
    #[inline]
    fn into_binding(self) -> BindingResource<'a> {
        BindingResource::TextureViewArray(self)
    }
}

impl<'a> IntoBinding<'a> for &'a Sampler {
    #[inline]
    fn into_binding(self) -> BindingResource<'a> {
        BindingResource::Sampler(self)
    }
}

impl<'a> IntoBinding<'a> for BindingResource<'a> {
    #[inline]
    fn into_binding(self) -> BindingResource<'a> {
        self
    }
}

pub trait IntoBindingArray<'b, const N: usize> {
    fn into_array(self) -> [BindingResource<'b>; N];
}

macro_rules! impl_to_binding_slice {
    ($N: expr, $(($T: ident, $I: ident)),*) => {
        impl<'b, $($T: IntoBinding<'b>),*> IntoBindingArray<'b, $N> for ($($T,)*) {
            #[inline]
            fn into_array(self) -> [BindingResource<'b>; $N] {
                let ($($I,)*) = self;
                [$($I.into_binding(), )*]
            }
        }
    }
}

all_tuples_with_size!(impl_to_binding_slice, 1, 32, T, s);

pub trait IntoIndexedBindingArray<'b, const N: usize> {
    fn into_array(self) -> [(u32, BindingResource<'b>); N];
}

macro_rules! impl_to_indexed_binding_slice {
    ($N: expr, $(($T: ident, $S: ident, $I: ident)),*) => {
        impl<'b, $($T: IntoBinding<'b>),*> IntoIndexedBindingArray<'b, $N> for ($((u32, $T),)*) {
            #[inline]
            fn into_array(self) -> [(u32, BindingResource<'b>); $N] {
                let ($(($S, $I),)*) = self;
                [$(($S, $I.into_binding())), *]
            }
        }
    }
}

all_tuples_with_size!(impl_to_indexed_binding_slice, 1, 32, T, n, s);

pub struct DynamicBindGroupEntries<'b> {
    entries: Vec<BindGroupEntry<'b>>,
}

impl<'b> DynamicBindGroupEntries<'b> {
    pub fn sequential<const N: usize>(entries: impl IntoBindingArray<'b, N>) -> Self {
        Self {
            entries: entries
                .into_array()
                .into_iter()
                .enumerate()
                .map(|(ix, resource)| BindGroupEntry {
                    binding: ix as u32,
                    resource,
                })
                .collect(),
        }
    }

    pub fn extend_sequential<const N: usize>(
        mut self,
        entries: impl IntoBindingArray<'b, N>,
    ) -> Self {
        let start = self.entries.last().unwrap().binding + 1;
        self.entries.extend(
            entries
                .into_array()
                .into_iter()
                .enumerate()
                .map(|(ix, resource)| BindGroupEntry {
                    binding: start + ix as u32,
                    resource,
                }),
        );
        self
    }

    pub fn new_with_indexes<const N: usize>(entries: impl IntoIndexedBindingArray<'b, N>) -> Self {
        Self {
            entries: entries
                .into_array()
                .into_iter()
                .map(|(binding, resource)| BindGroupEntry { binding, resource })
                .collect(),
        }
    }

    pub fn extend_with_indexes<const N: usize>(
        mut self,
        entries: impl IntoIndexedBindingArray<'b, N>,
    ) -> Self {
        self.entries.extend(
            entries
                .into_array()
                .into_iter()
                .map(|(binding, resource)| BindGroupEntry { binding, resource }),
        );
        self
    }
}

impl<'b> std::ops::Deref for DynamicBindGroupEntries<'b> {
    type Target = [BindGroupEntry<'b>];

    fn deref(&self) -> &[BindGroupEntry<'b>] {
        &self.entries
    }
}