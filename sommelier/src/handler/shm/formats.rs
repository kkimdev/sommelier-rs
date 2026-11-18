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

//! SHM pixel formats: the ones the guest's `wl_shm` objects offer (the
//! mandatory ones at once, each optional one once the host reports it),
//! their DRM codes, and their bytes per pixel.

use super::ShmHandler;
use crate::protocols::wayland::wl_shm;
use crate::state::Context;

/// Formats advertised by ChromiumOS Sommelier's wl_shm shim. Every format
/// after ARGB/XRGB has the same FourCC value in wl_shm and DRM, so the
/// linux-dmabuf shim uses them as DRM formats too.
const SUPPORTED_SHM_FORMATS: [u32; 6] = [
    wl_shm::FORMAT_ARGB8888,
    wl_shm::FORMAT_XRGB8888,
    wl_shm::FORMAT_NV12,
    wl_shm::FORMAT_RGB565,
    wl_shm::FORMAT_ABGR8888,
    wl_shm::FORMAT_XBGR8888,
];

const MANDATORY_SHM_FORMATS: [u32; 2] = [wl_shm::FORMAT_ARGB8888, wl_shm::FORMAT_XRGB8888];

pub(super) fn supported_shm_format(format: u32) -> bool {
    SUPPORTED_SHM_FORMATS.contains(&format)
}

fn shm_format_from_drm(format: u32) -> Option<u32> {
    match format {
        // DRM_FORMAT_ARGB8888/XRGB8888 use FourCC values while wl_shm uses
        // the enum values 0/1 for the same layouts.
        0x3432_5241 => Some(wl_shm::FORMAT_ARGB8888),
        0x3432_5258 => Some(wl_shm::FORMAT_XRGB8888),
        wl_shm::FORMAT_NV12 => Some(wl_shm::FORMAT_NV12),
        format if supported_shm_format(format) => Some(format),
        _ => None,
    }
}

pub(super) fn guest_shm_format_available(ctx: &Context, format: u32) -> bool {
    // Without VirtWL every guest SHM buffer reaches the host's wl_shm (see
    // `select_shm_backing`), so only the formats it lists count. VirtWL also
    // sends some buffers through the host's linux-dmabuf.
    let host_formats = if ctx.virtwayland_channel.is_some() {
        &ctx.host_shm_formats
    } else {
        &ctx.host_wl_shm_formats
    };
    MANDATORY_SHM_FORMATS.contains(&format) || host_formats.contains(&format)
}

/// Record a format the host reported through its wl_shm or linux-dmabuf and
/// enqueue it for the synthetic guest wl_shm objects already bound, if it is
/// available to them.
fn record_host_shm_format(ctx: &mut Context, format: u32) {
    if !supported_shm_format(format) {
        return;
    }
    // A format one source already reported may become available to the
    // guest only now; each guest binding sends a format once.
    ctx.host_shm_formats.insert(format);

    let guest_ids: Vec<u32> = ctx.guest_shms.keys().copied().collect();
    for guest_id in guest_ids {
        if ctx.stale_shm_guest_objects.contains(&guest_id) {
            continue;
        }
        if !guest_shm_format_available(ctx, format) {
            continue;
        }
        let should_send = ctx
            .guest_shms
            .get_mut(&guest_id)
            .is_some_and(|shm| shm.formats.insert(format));
        if should_send {
            ctx.host_to_client_queue
                .push(wl_shm::evt::format(guest_id, format));
        }
    }
}

pub(super) fn record_host_shm_wl_format(ctx: &mut Context, format: u32) {
    if supported_shm_format(format) {
        ctx.host_wl_shm_formats.insert(format);
    }
    record_host_shm_format(ctx, format);
}

/// Record a format reported by the internal host dmabuf object. ChromiumOS
/// uses those events as the capability source when the virtwl channel exposes
/// dmabuf-backed SHM buffers.
pub(crate) fn record_host_shm_drm_format(ctx: &mut Context, format: u32) {
    if let Some(shm_format) = shm_format_from_drm(format) {
        ctx.host_dmabuf_shm_formats.insert(shm_format);
        record_host_shm_format(ctx, shm_format);
    }
}

pub(crate) fn clear_host_shm_wl_formats(ctx: &mut Context) {
    let removed = std::mem::take(&mut ctx.host_wl_shm_formats);
    for format in removed {
        if !ctx.host_dmabuf_shm_formats.contains(&format) {
            ctx.host_shm_formats.remove(&format);
        }
    }
}

pub(crate) fn clear_host_shm_dmabuf_formats(ctx: &mut Context) {
    let removed = std::mem::take(&mut ctx.host_dmabuf_shm_formats);
    for format in removed {
        if !ctx.host_wl_shm_formats.contains(&format) {
            ctx.host_shm_formats.remove(&format);
        }
    }
}

/// Register synthetic guest wl_shm object `guest_id`, bound from host global
/// `global_name`, and send every capability known so far. ARGB/XRGB are
/// mandatory wl_shm formats; optional formats are sent only after the host
/// reports support.
pub(crate) fn register_guest_shm(ctx: &mut Context, guest_id: u32, global_name: u32) {
    // `forget_shm` clears the mark before the ID can be bound again.
    debug_assert!(!ctx.stale_shm_guest_objects.contains(&guest_id));
    let formats: Vec<u32> = SUPPORTED_SHM_FORMATS
        .iter()
        .copied()
        .filter(|format| guest_shm_format_available(ctx, *format))
        .collect();
    ctx.guest_shms.insert(
        guest_id,
        crate::state::GuestShm {
            global_name,
            formats: formats.iter().copied().collect(),
        },
    );

    for format in formats {
        ctx.host_to_client_queue
            .push(wl_shm::evt::format(guest_id, format));
    }
}

pub(super) fn format_bytes_per_pixel(format: u32) -> Option<usize> {
    match format {
        wl_shm::FORMAT_NV12 => Some(1),
        wl_shm::FORMAT_RGB565 => Some(2),
        wl_shm::FORMAT_ARGB8888
        | wl_shm::FORMAT_XRGB8888
        | wl_shm::FORMAT_ABGR8888
        | wl_shm::FORMAT_XBGR8888 => Some(4),
        _ => None,
    }
}

impl ShmHandler {
    pub(super) fn wl_shm_format_to_drm_format(format: u32) -> u32 {
        match format {
            wl_shm::FORMAT_ARGB8888 => 0x34325241, // DRM_FORMAT_ARGB8888
            wl_shm::FORMAT_XRGB8888 => 0x34325258, // DRM_FORMAT_XRGB8888
            _ => format,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        clear_host_shm_dmabuf_formats, clear_host_shm_wl_formats, guest_shm_format_available,
        record_host_shm_drm_format, record_host_shm_wl_format, register_guest_shm,
    };
    use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_v1;
    use crate::protocols::wayland::{wl_registry, wl_shm};
    use crate::protocols::{AnyEvent, AnyRequest};
    use crate::proxy::harness::Session;
    use crate::state::Context;
    use crate::test_support::decoded;
    use crate::wire::Action;

    #[test]
    fn synthetic_shm_advertises_mandatory_formats_until_host_capabilities_arrive() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        register_guest_shm(&mut ctx, 20, 10);

        assert_eq!(
            decoded(&ctx.host_to_client_queue, wl_shm::Event::from_wire),
            [
                (20, wl_shm::Event::Format { format: 0 }),
                (20, wl_shm::Event::Format { format: 1 })
            ]
        );
        assert!(guest_shm_format_available(&ctx, 0));
        assert!(guest_shm_format_available(&ctx, 1));
        assert!(!guest_shm_format_available(&ctx, 0x3631_4752));
    }

    #[test]
    fn host_optional_shm_format_is_sent_once_to_existing_guest_bindings() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        register_guest_shm(&mut ctx, 20, 10);
        ctx.host_to_client_queue.clear();

        record_host_shm_wl_format(&mut ctx, 0x3631_4752);
        record_host_shm_wl_format(&mut ctx, 0x3631_4752);

        assert_eq!(
            decoded(&ctx.host_to_client_queue, wl_shm::Event::from_wire),
            [(
                20,
                wl_shm::Event::Format {
                    format: 0x3631_4752
                }
            )]
        );
        assert!(guest_shm_format_available(&ctx, 0x3631_4752));
    }

    #[test]
    fn source_separated_host_formats_survive_single_source_removal() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_shm = 20;
        let format = 0x3631_4752;
        register_guest_shm(&mut ctx, guest_shm, 10);
        ctx.host_to_client_queue.clear();

        record_host_shm_wl_format(&mut ctx, format);
        record_host_shm_drm_format(&mut ctx, format);
        assert!(ctx.host_wl_shm_formats.contains(&format));
        assert!(ctx.host_dmabuf_shm_formats.contains(&format));
        assert!(ctx.host_shm_formats.contains(&format));
        assert_eq!(ctx.host_to_client_queue.len(), 1);

        ctx.host_to_client_queue.clear();
        clear_host_shm_dmabuf_formats(&mut ctx);
        assert!(
            ctx.host_shm_formats.contains(&format),
            "wl_shm remains a valid capability source"
        );
        assert!(ctx.host_to_client_queue.is_empty());

        clear_host_shm_wl_formats(&mut ctx);
        assert!(!ctx.host_shm_formats.contains(&format));

        record_host_shm_wl_format(&mut ctx, format);
        assert_eq!(
            ctx.host_to_client_queue.len(),
            0,
            "an existing object must not receive a duplicate format event"
        );
        register_guest_shm(&mut ctx, guest_shm + 1, 10);
        assert!(
            decoded(&ctx.host_to_client_queue, wl_shm::Event::from_wire)
                .contains(&(guest_shm + 1, wl_shm::Event::Format { format })),
            "a replacement synthetic object must receive the re-advertised format"
        );
    }

    #[test]
    fn drm_fourcc_capability_maps_to_wayland_shm_format() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        record_host_shm_drm_format(&mut ctx, 0x3432_5241); // DRM ARGB8888
        record_host_shm_drm_format(&mut ctx, 0x3432_5258); // DRM XRGB8888

        assert!(ctx.host_shm_formats.contains(&0));
        assert!(ctx.host_shm_formats.contains(&1));
        assert!(!ctx.host_shm_formats.contains(&0xdead_beef));
    }

    /// The host object the proxy binds for itself when the host advertises
    /// `interface`, learned from the bind among what the host received.
    fn internal_bind(received: &[(u32, AnyRequest)], interface: &str) -> u32 {
        received
            .iter()
            .find_map(|(_, request)| match request {
                AnyRequest::WlRegistry(wl_registry::Request::Bind { id, .. })
                    if id.0 == interface =>
                {
                    Some(id.2)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("the proxy did not bind {interface}: {received:?}"))
    }

    /// Without VirtWL every guest SHM buffer reaches the host's wl_shm, so
    /// the guest's wl_shm offers an optional format only once the host's
    /// wl_shm lists it, even if the host's linux-dmabuf takes it.
    #[tokio::test]
    async fn without_virtwl_guest_shm_offers_only_the_host_wl_shm_formats() {
        let mut s = Session::with_context(Context::new_for_test(true, false, vec![]));
        let registry = s.registry().await;
        let shm_name = s.advertise("wl_shm", wl_shm::VERSION).await;
        let host_shm = internal_bind(&s.host_received(), "wl_shm");
        let advertised = s.guest_received();
        let [(_, AnyEvent::WlRegistry(wl_registry::Event::Global { version, .. }))] =
            &advertised[..]
        else {
            panic!("the guest was not advertised wl_shm: {advertised:?}");
        };
        let shm = s.new_id();
        s.guest([wl_registry::req::bind(registry.id, shm_name, "wl_shm", *version, shm).unwrap()])
            .await;
        assert_eq!(s.host_received(), [], "the proxy implements wl_shm");
        assert_eq!(
            s.guest_received(),
            [
                (shm, wl_shm::Event::Format { format: 0 }.into()),
                (shm, wl_shm::Event::Format { format: 1 }.into()),
            ]
        );
        // With `--gpu-accel` the proxy binds the host linux-dmabuf too; the
        // guest gets that global as well, which is not this test's concern.
        s.advertise("zwp_linux_dmabuf_v1", 3).await;
        let host_dmabuf = internal_bind(&s.host_received(), "zwp_linux_dmabuf_v1");
        s.guest_received();

        s.host([
            zwp_linux_dmabuf_v1::evt::format(host_dmabuf, wl_shm::FORMAT_RGB565),
            zwp_linux_dmabuf_v1::evt::format(host_dmabuf, wl_shm::FORMAT_NV12),
        ])
        .await;
        assert_eq!(s.guest_received(), [], "dma-buf formats are not wl_shm's");

        s.host([
            wl_shm::evt::format(host_shm, wl_shm::FORMAT_RGB565),
            wl_shm::evt::format(host_shm, wl_shm::FORMAT_NV12),
        ])
        .await;
        assert_eq!(
            s.guest_received(),
            [
                (
                    shm,
                    wl_shm::Event::Format {
                        format: wl_shm::FORMAT_RGB565
                    }
                    .into()
                ),
                (
                    shm,
                    wl_shm::Event::Format {
                        format: wl_shm::FORMAT_NV12
                    }
                    .into()
                ),
            ]
        );
        assert_eq!(s.host_received(), []);
    }

    /// A format the host's wl_shm reports reaches every guest `wl_shm` in
    /// ascending ID order, whatever order the guest bound them in, so what
    /// the proxy sends never depends on how it hashes them. With eight, an
    /// order that depends on hashing is all but certain to differ.
    #[tokio::test]
    async fn a_host_format_reaches_every_guest_shm_in_id_order() {
        let mut s = Session::new();
        let registry = s.registry().await;
        let name = s.advertise("wl_shm", wl_shm::VERSION).await;
        let host_shm = internal_bind(&s.host_received(), "wl_shm");
        let advertised = s.guest_received();
        let [(_, AnyEvent::WlRegistry(wl_registry::Event::Global { version, .. }))] =
            &advertised[..]
        else {
            panic!("the guest was not advertised wl_shm: {advertised:?}");
        };
        let format =
            |shm, format| -> (u32, AnyEvent) { (shm, wl_shm::Event::Format { format }.into()) };
        let shms = (0..8).map(|_| s.new_id()).collect::<Vec<_>>();
        for &shm in shms.iter().rev() {
            let bind = wl_registry::req::bind(registry.id, name, "wl_shm", *version, shm);
            s.guest([bind.unwrap()]).await;
            assert_eq!(s.guest_received(), [format(shm, 0), format(shm, 1)]);
        }
        assert_eq!(s.host_received(), [], "the proxy implements wl_shm");

        s.host([wl_shm::evt::format(host_shm, wl_shm::FORMAT_RGB565)])
            .await;
        let expected = shms.iter().map(|&shm| format(shm, wl_shm::FORMAT_RGB565));
        assert_eq!(s.guest_received(), expected.collect::<Vec<_>>());
    }

    #[test]
    fn replacement_shm_capability_does_not_reach_stale_guest_object() {
        use crate::handler::registry::RegistryHandler;
        use crate::protocols::wayland::wl_registry::WlRegistryHandler;

        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = 1;
        ctx.shadow_table.insert_paired_for_test(1, 1, "wl_registry");
        let mut registry_handler = RegistryHandler;
        let shm = "wl_shm".to_string();

        assert_eq!(
            registry_handler.on_global(&mut ctx, 10, &shm, 1),
            Action::Drop
        );
        let old_guest_shm = 20;
        register_guest_shm(&mut ctx, old_guest_shm, 10);
        ctx.host_to_client_queue.clear();

        assert_eq!(
            registry_handler.on_global_remove(&mut ctx, 10),
            Action::Forward
        );
        assert!(
            ctx.stale_shm_guest_objects.contains(&old_guest_shm),
            "removed host SHM generation must mark existing guest objects stale"
        );
        assert_eq!(
            registry_handler.on_global(&mut ctx, 10, &shm, 1),
            Action::Drop
        );
        let new_guest_shm = 21;
        register_guest_shm(&mut ctx, new_guest_shm, 10);
        ctx.host_to_client_queue.clear();

        record_host_shm_wl_format(&mut ctx, 0x3631_4752);

        assert_eq!(
            decoded(&ctx.host_to_client_queue, wl_shm::Event::from_wire),
            [(
                new_guest_shm,
                wl_shm::Event::Format {
                    format: 0x3631_4752
                }
            )],
            "replacement capability must be sent only to the replacement guest object"
        );
    }
}
