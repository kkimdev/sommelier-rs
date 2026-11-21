/*
Copyright 2026 Google LLC

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

     https://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

//! The state the proxy keeps for one live guest object, which the object's
//! entry in the [`super::ShadowTable`] owns.
//!
//! An object's data lives in its entry, so it cannot outlive the object or
//! be read under a reused ID: the transition that ends the object takes it
//! out, and `state::lifecycle` hands it to its owner's `forget_*` function,
//! or drops it. Each kind of data belongs to one interface ([`Data::IFACE`]),
//! and the table attaches it only to a live object of that interface.

use crate::protocols::Iface;

/// The data of one kind, kept for live guest objects of one interface.
pub(crate) trait Data: Sized + Into<ObjectData> {
    /// The interface of the objects that keep it.
    const IFACE: Iface;
    /// The data, if `data` is of this kind.
    fn of(data: &ObjectData) -> Option<&Self>;
    /// [`Self::of`], mutably.
    fn of_mut(data: &mut ObjectData) -> Option<&mut Self>;
}

/// Declare [`ObjectData`], one variant per kind of data, and each kind's
/// [`Data`] implementation.
macro_rules! object_data {
    ($($(#[$doc:meta])* $variant:ident($ty:ty) for $iface:ident,)*) => {
        /// What a live guest object keeps: nothing, or the data of its
        /// interface.
        #[derive(Debug, Default)]
        pub(crate) enum ObjectData {
            /// Nothing, as most objects keep.
            #[default]
            None,
            $($(#[$doc])* $variant($ty),)*
        }

        impl ObjectData {
            /// The interface of the objects that keep this data, if any.
            pub(crate) fn iface(&self) -> Option<Iface> {
                match self {
                    Self::None => None,
                    $(Self::$variant(_) => Some(Iface::$iface),)*
                }
            }
        }

        $(
            impl Data for $ty {
                const IFACE: Iface = Iface::$iface;

                fn of(data: &ObjectData) -> Option<&Self> {
                    match data {
                        ObjectData::$variant(data) => Some(data),
                        _ => None,
                    }
                }

                fn of_mut(data: &mut ObjectData) -> Option<&mut Self> {
                    match data {
                        ObjectData::$variant(data) => Some(data),
                        _ => None,
                    }
                }
            }

            impl From<$ty> for ObjectData {
                fn from(data: $ty) -> Self {
                    Self::$variant(data)
                }
            }
        )*
    };
}

object_data! {
    /// A guest `wl_shm`, which the proxy implements.
    Shm(crate::handler::shm::GuestShm) for WlShm,
    /// A guest `wl_shm_pool`, which the proxy implements.
    ShmPool(crate::handler::shm::PoolData) for WlShmPool,
    /// A guest `zwp_linux_buffer_params_v1`.
    Params(crate::handler::linux_dmabuf::ParamsData) for ZwpLinuxBufferParamsV1,
    /// A guest `zwp_linux_dmabuf_v1`.
    DmabufFactory(crate::handler::linux_dmabuf::DmabufFactory) for ZwpLinuxDmabufV1,
    /// A synthetic guest `zwp_linux_dmabuf_feedback_v1`.
    DmabufFeedback(crate::handler::linux_dmabuf::DmabufFeedback) for ZwpLinuxDmabufFeedbackV1,
    /// A guest `wp_viewport`.
    Viewport(crate::handler::compositor::Viewport) for WpViewport,
    /// A synthetic guest `gtk_shell1`.
    GtkShell(crate::handler::gtk_shell::GtkShell) for GtkShell1,
    /// A synthetic guest `gtk_surface1`.
    GtkSurface(crate::handler::gtk_shell::GtkSurface) for GtkSurface1,
    DataControlSource(crate::handler::data_control::Source) for ExtDataControlSourceV1,
}
