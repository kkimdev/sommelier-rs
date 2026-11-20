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

use crate::protocols::wayland::wl_shm;
use crate::state::Context;

/// The one list of pixel formats the proxy converts and advertises: the
/// guest's `wl_shm` offers them, and the guest's linux-dmabuf shim passes
/// only their DRM codes (`is_supported_drm_format`). It is the same set in
/// two numberings: wl_shm and DRM share a value for every format except
/// ARGB8888 and XRGB8888, which `wl_shm_format_to_drm_format` maps. The
/// set follows ChromiumOS Sommelier's `sl_drm_format_is_supported()`.
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

/// The wl_shm format whose DRM code is `format`, if it is a supported one.
fn shm_format_from_drm(format: u32) -> Option<u32> {
    SUPPORTED_SHM_FORMATS
        .into_iter()
        .find(|shm| wl_shm_format_to_drm_format(*shm) == format)
}

/// Whether DRM code `format` is one of the supported formats.
pub(crate) fn is_supported_drm_format(format: u32) -> bool {
    shm_format_from_drm(format).is_some()
}

/// Whether the host reported supported format `format` through its wl_shm
/// or its linux-dmabuf. ARGB/XRGB are always available per the wl_shm
/// contract and need no report.
fn host_reported_shm_format(ctx: &Context, format: u32) -> bool {
    ctx.host_wl_shm_formats.contains(&format) || ctx.host_dmabuf_shm_formats.contains(&format)
}

pub(super) fn guest_shm_format_available(ctx: &Context, format: u32) -> bool {
    // Without VirtWL every guest SHM buffer reaches the host's wl_shm (see
    // `select_shm_backing`), so only the formats it lists count. VirtWL also
    // sends some buffers through the host's linux-dmabuf.
    MANDATORY_SHM_FORMATS.contains(&format)
        || if ctx.virtwayland_channel().is_some() {
            host_reported_shm_format(ctx, format)
        } else {
            ctx.host_wl_shm_formats.contains(&format)
        }
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
    ctx.host_wl_shm_formats.clear();
}

pub(crate) fn clear_host_shm_dmabuf_formats(ctx: &mut Context) {
    ctx.host_dmabuf_shm_formats.clear();
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

/// The DRM code of wl_shm `format`. Only the two mandatory formats differ;
/// every other format has the same FourCC in both.
pub(super) fn wl_shm_format_to_drm_format(format: u32) -> u32 {
    match format {
        wl_shm::FORMAT_ARGB8888 => 0x34325241, // DRM_FORMAT_ARGB8888
        wl_shm::FORMAT_XRGB8888 => 0x34325258, // DRM_FORMAT_XRGB8888
        _ => format,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        clear_host_shm_dmabuf_formats, clear_host_shm_wl_formats, format_bytes_per_pixel,
        host_reported_shm_format, is_supported_drm_format, record_host_shm_drm_format,
        record_host_shm_wl_format, register_guest_shm, shm_format_from_drm,
        wl_shm_format_to_drm_format, SUPPORTED_SHM_FORMATS,
    };
    use crate::handler::test_support::{bind_shm, shm_global, GUEST_SHM_VERSION};
    use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_v1;
    use crate::protocols::wayland::{wl_registry, wl_shm, wl_shm_pool};
    use crate::protocols::AnyEvent;
    use crate::proxy::harness::Session;
    use crate::state::Context;
    use crate::test_support::{decoded, shm_pool_file};

    const RGB565: u32 = wl_shm::FORMAT_RGB565;

    /// Every advertised format has a size, so a buffer in it is never refused
    /// for want of a bytes-per-pixel.
    #[test]
    fn every_supported_format_has_bytes_per_pixel() {
        for format in SUPPORTED_SHM_FORMATS {
            assert!(format_bytes_per_pixel(format).is_some(), "{format:#x}");
        }
    }

    /// The linux-dmabuf shim and the wl_shm shim advertise one set of
    /// formats, numbered two ways: `SUPPORTED_SHM_FORMATS` by wl_shm value,
    /// the shim's list by DRM FourCC.
    #[test]
    fn the_shm_and_drm_format_lists_are_one_set() {
        let drm = |format: u32| {
            SUPPORTED_SHM_FORMATS
                .map(wl_shm_format_to_drm_format)
                .contains(&format)
        };
        for shm in SUPPORTED_SHM_FORMATS {
            let drm = wl_shm_format_to_drm_format(shm);
            assert_eq!(shm_format_from_drm(drm), Some(shm), "{shm:#x}");
            assert!(is_supported_drm_format(drm), "{drm:#x}");
        }
        // The wl_shm enum values 0 and 1 are not DRM codes.
        for not_drm in [
            wl_shm::FORMAT_ARGB8888,
            wl_shm::FORMAT_XRGB8888,
            0xdead_beef,
        ] {
            assert!(!drm(not_drm));
            assert_eq!(shm_format_from_drm(not_drm), None, "{not_drm:#x}");
            assert!(!is_supported_drm_format(not_drm), "{not_drm:#x}");
        }
    }

    /// The guest creates a 1x1 `format` buffer from a new pool of `shm`;
    /// returns the pool and the buffer's request.
    async fn buffer_request(
        s: &mut Session,
        shm: u32,
        format: u32,
    ) -> (u32, crate::wire::Outgoing) {
        let pool = s.shm_pool(shm, shm_pool_file(&[0; 4096])).await;
        let buffer = s.new_id();
        let request = wl_shm_pool::req::create_buffer(pool, buffer, 0, 1, 1, 4, format);
        (pool, request)
    }

    /// A guest `wl_shm` offers the two mandatory formats at once, and no
    /// optional one before the host reports it: a buffer in one is an
    /// invalid format.
    #[tokio::test]
    async fn synthetic_shm_advertises_mandatory_formats_until_host_capabilities_arrive() {
        let mut s = Session::new();
        let registry = s.registry().await;
        let (name, _) = shm_global(&mut s).await;
        let shm = bind_shm(&mut s, registry.id, name).await;

        let (pool, request) = buffer_request(&mut s, shm, RGB565).await;
        s.guest_fatal([request]).await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (pool, wl_shm::ERROR_INVALID_FORMAT))
        );
        assert_eq!(s.host_received(), []);
    }

    /// An optional format the host reports reaches a guest `wl_shm` once,
    /// however often the host reports it, and the guest may then use it.
    #[tokio::test]
    async fn host_optional_shm_format_is_sent_once_to_existing_guest_bindings() {
        let mut s = Session::new();
        let registry = s.registry().await;
        let (name, host_shm) = shm_global(&mut s).await;
        let shm = bind_shm(&mut s, registry.id, name).await;

        s.host([
            wl_shm::evt::format(host_shm, RGB565),
            wl_shm::evt::format(host_shm, RGB565),
        ])
        .await;
        assert_eq!(
            s.guest_received(),
            [(shm, wl_shm::Event::Format { format: RGB565 }.into())]
        );
        let (_, request) = buffer_request(&mut s, shm, RGB565).await;
        s.guest([request]).await;
        assert_eq!(s.guest_received(), []);
        s.host_received();
    }

    /// Kept a unit test: the host's linux-dmabuf is a format source of the
    /// guest's `wl_shm` only with a VirtWL channel, which no session has.
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
        assert!(host_reported_shm_format(&ctx, format));
        assert_eq!(ctx.host_to_client_queue.len(), 1);

        ctx.host_to_client_queue.clear();
        clear_host_shm_dmabuf_formats(&mut ctx);
        assert!(
            host_reported_shm_format(&ctx, format),
            "wl_shm remains a valid capability source"
        );
        assert!(ctx.host_to_client_queue.is_empty());

        clear_host_shm_wl_formats(&mut ctx);
        assert!(!host_reported_shm_format(&ctx, format));

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

    /// Kept a unit test, as
    /// [`source_separated_host_formats_survive_single_source_removal`]: a
    /// DRM code the host's linux-dmabuf reports counts only with VirtWL.
    #[test]
    fn drm_fourcc_capability_maps_to_wayland_shm_format() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        record_host_shm_drm_format(&mut ctx, 0x3432_5241); // DRM ARGB8888
        record_host_shm_drm_format(&mut ctx, 0x3432_5258); // DRM XRGB8888

        assert!(host_reported_shm_format(&ctx, 0));
        assert!(host_reported_shm_format(&ctx, 1));
        assert!(!host_reported_shm_format(&ctx, 0xdead_beef));
    }

    /// Without VirtWL every guest SHM buffer reaches the host's wl_shm, so
    /// the guest's wl_shm offers an optional format only once the host's
    /// wl_shm lists it, even if the host's linux-dmabuf takes it.
    #[tokio::test]
    async fn without_virtwl_guest_shm_offers_only_the_host_wl_shm_formats() {
        let mut s = Session::with_context(Context::new_for_test(true, false, vec![]));
        let registry = s.registry().await;
        let (shm_name, host_shm) = shm_global(&mut s).await;
        let shm = bind_shm(&mut s, registry.id, shm_name).await;
        // With `--gpu-accel` the proxy binds the host linux-dmabuf too; the
        // guest gets that global as well, which is not this test's concern.
        let (_, host_dmabuf) = s.advertise_internal("zwp_linux_dmabuf_v1", 3).await;
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
        let (name, host_shm) = shm_global(&mut s).await;
        let format =
            |shm, format| -> (u32, AnyEvent) { (shm, wl_shm::Event::Format { format }.into()) };
        let shms = (0..8).map(|_| s.new_id()).collect::<Vec<_>>();
        for &shm in shms.iter().rev() {
            let bind = wl_registry::req::bind(registry.id, name, "wl_shm", GUEST_SHM_VERSION, shm);
            s.guest([bind.unwrap()]).await;
            assert_eq!(s.guest_received(), [format(shm, 0), format(shm, 1)]);
        }
        assert_eq!(s.host_received(), [], "the proxy implements wl_shm");

        s.host([wl_shm::evt::format(host_shm, wl_shm::FORMAT_RGB565)])
            .await;
        let expected = shms.iter().map(|&shm| format(shm, wl_shm::FORMAT_RGB565));
        assert_eq!(s.guest_received(), expected.collect::<Vec<_>>());
    }

    /// A guest `wl_shm` bound from a removed global gets no format the
    /// host reports for the global advertised anew under its name; the one
    /// bound from the new global does.
    #[tokio::test]
    async fn replacement_shm_capability_does_not_reach_stale_guest_object() {
        let mut s = Session::new();
        let registry = s.registry().await;
        let (name, _) = shm_global(&mut s).await;
        let old_guest_shm = bind_shm(&mut s, registry.id, name).await;

        s.host([wl_registry::evt::global_remove(registry.host, name)])
            .await;
        let removal = wl_registry::Event::GlobalRemove { name };
        assert_eq!(s.guest_received(), [(registry.id, removal.into())]);
        assert_eq!(s.host_received(), [], "a wl_shm v1 has no destructor");

        let global = wl_registry::evt::global(registry.host, name, wl_shm::NAME, wl_shm::VERSION);
        s.host([global.unwrap()]).await;
        let received = s.host_received();
        let [(
            _,
            crate::protocols::AnyRequest::WlRegistry(wl_registry::Request::Bind {
                id: (_, _, host_shm),
                ..
            }),
        )] = received[..]
        else {
            panic!("the proxy did not bind the new wl_shm: {received:?}");
        };
        s.guest_received();
        let new_guest_shm = bind_shm(&mut s, registry.id, name).await;

        s.host([wl_shm::evt::format(host_shm, RGB565)]).await;
        assert_eq!(
            s.guest_received(),
            [(
                new_guest_shm,
                wl_shm::Event::Format { format: RGB565 }.into()
            )],
            "replacement capability must be sent only to the replacement guest object, not \
             {old_guest_shm}"
        );
    }
}
