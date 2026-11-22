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

//! What one frame costs the proxy: its heap allocations, and the system
//! calls that copy pixels into, or map, a host copy
//! ([`Counts`]); and what forwarding one message costs, measured on the
//! commonest one, a pointer motion. Each test drives a steady stream of
//! frames or messages through a whole [`Session`], measures the proxy's
//! share alone, and holds it to a budget, so a change that makes frames or
//! messages dearer fails here. Run with `--nocapture` to see the numbers.

use super::harness::{Object, Session};
use crate::handler::shm::{copy_shm_damage, ShmCopyLayout};
use crate::protocols::wayland::{wl_buffer, wl_pointer, wl_seat, wl_shm, wl_surface};
use crate::state::{BufferRect, DamageRegion, PlaneFenceForTest};
use crate::test_support::counting::Counts;
use crate::test_support::TestConnection;
use crate::test_support::{host_mapping, shm_pool_file};

/// The frames each test measures, after the warm-up frames that copy each
/// buffer's host copy in full for the first time.
const FRAMES: u64 = 20;
const WARM_UP_FRAMES: u64 = 4;

/// The buffers' size: 256x128 ARGB8888 pixels, rows of 1024 bytes.
const WIDTH: i32 = 256;
const HEIGHT: i32 = 128;

/// A frame's buffer damage, as `wl_surface.damage_buffer` takes it.
type Damage = (i32, i32, i32, i32);

/// Damage of every row, but not of a column at either side: a scrolled
/// text pane beside a scroll bar.
const PARTIAL_WIDTH: Damage = (16, 0, WIDTH - 32, HEIGHT);
/// Damage of the whole buffer.
const FULL: Damage = (0, 0, WIDTH, HEIGHT);

/// A guest client that double-buffers, as on Exo: each frame attaches the
/// buffer the previous frame did not, damages `damage` and commits, and
/// the host releases the buffer the commit replaced.
async fn double_buffered_frames(
    s: &mut Session,
    surface: Object,
    buffers: [Object; 2],
    damage: Damage,
    frames: u64,
) {
    let (x, y, width, height) = damage;
    for frame in 0..frames {
        let shown = buffers[(frame % 2) as usize];
        let replaced = buffers[((frame + 1) % 2) as usize];
        s.guest([
            wl_surface::req::attach(surface.id, Some(shown.id), 0, 0),
            wl_surface::req::damage_buffer(surface.id, x, y, width, height),
            wl_surface::req::commit(surface.id),
        ])
        .await;
        let received = s.host_received();
        assert!(
            matches!(
                received.last(),
                Some((host, crate::protocols::AnyRequest::WlSurface(wl_surface::Request::Commit {})))
                    if *host == surface.host
            ),
            "frame {frame} reached the host: {received:?}"
        );
        if frame > 0 {
            s.host([wl_buffer::evt::release(replaced.host)]).await;
            assert_eq!(
                s.guest_received(),
                [(replaced.id, wl_buffer::Event::Release {}.into())]
            );
        }
    }
}

/// What one of `FRAMES` frames of `damage` costs the proxy, after the
/// warm-up frames.
async fn cost_per_frame(
    s: &mut Session,
    surface: Object,
    buffers: [Object; 2],
    damage: Damage,
) -> Counts {
    double_buffered_frames(s, surface, buffers, damage, WARM_UP_FRAMES).await;
    s.proxy_cost();
    double_buffered_frames(s, surface, buffers, damage, FRAMES).await;
    s.proxy_cost().per(FRAMES)
}

/// A surface and two SHM buffers of [`WIDTH`]x[`HEIGHT`], each in a pool
/// of its own, whose host copies are memfds laid out like the guest's
/// (no VirtWL).
async fn shm_surface() -> (Session, Object, [Object; 2]) {
    let mut s = Session::new();
    let surface = s.surface().await;
    let shm = s.bind("wl_shm", wl_shm::VERSION).await;
    let pixels = vec![0x5a; (4 * WIDTH * HEIGHT) as usize];
    let mut buffers = Vec::new();
    for _ in 0..2 {
        let pool = s.shm_pool(shm, shm_pool_file(&pixels)).await;
        buffers.push(s.shm_buffer(pool, WIDTH, HEIGHT).await);
    }
    let buffers = [buffers[0], buffers[1]];
    s.guest_received();
    (s, surface, buffers)
}

/// The cost of a SHM frame of `damage`, printed as `name`.
async fn shm_frame_cost(name: &str, damage: Damage) -> Counts {
    let (mut s, surface, buffers) = shm_surface().await;
    let cost = cost_per_frame(&mut s, surface, buffers, damage).await;
    println!("frame cost, {name}: {cost:?}");
    cost
}

/// A frame whose damage spans every row but not every column copies its
/// rows in one read, which skips the columns between them.
#[tokio::test]
async fn a_partial_width_shm_frame_costs_one_read() {
    let cost = shm_frame_cost("SHM, partial-width damage", PARTIAL_WIDTH).await;
    assert_eq!(cost.pool_reads, 1);
    assert_eq!(cost.host_maps, 0);
    assert!(cost.allocations <= 13, "{cost:?}");
}

/// A frame that damages the whole of a buffer laid out like its host copy
/// copies it in one read.
#[tokio::test]
async fn a_full_shm_frame_costs_one_read() {
    let cost = shm_frame_cost("SHM, full damage", FULL).await;
    assert_eq!(cost.pool_reads, 1);
    assert_eq!(cost.host_maps, 0);
    assert!(cost.allocations <= 13, "{cost:?}");
}

/// A host copy whose rows are longer than the guest's, as a VirtWL dma-buf
/// allocator pads them (`--gpu-accel`), takes a frame in one read too. A
/// session has no VirtWL, so this measures the copy alone.
#[test]
fn a_frame_into_padded_host_rows_costs_one_read() {
    let (width, height) = (WIDTH as usize, HEIGHT as usize);
    let src_stride = 4 * width;
    let dst_stride = src_stride + 256;
    let source = shm_pool_file(&vec![0x5a; src_stride * height]);
    let destination = host_mapping(&vec![0; dst_stride * height]);
    let layout = ShmCopyLayout {
        format: wl_shm::FORMAT_ARGB8888,
        offset: 0,
        width,
        src_stride,
        dst_stride,
        dst_plane1_offset: 0,
        dst_plane1_stride: 0,
        height,
    };
    for (name, damage) in [
        ("full damage", DamageRegion::Full),
        (
            "partial-width damage",
            DamageRegion::from(vec![BufferRect::new(
                PARTIAL_WIDTH.0,
                PARTIAL_WIDTH.1,
                PARTIAL_WIDTH.2,
                PARTIAL_WIDTH.3,
            )]),
        ),
    ] {
        let (cost, copied) = Counts::of(|| copy_shm_damage(&source, &destination, layout, &damage));
        copied.unwrap();
        println!("copy cost, padded host rows, {name}: {cost:?}");
        assert_eq!(cost.pool_reads, 1, "{name}");
    }
}

/// A frame of a native buffer copies nothing; it checks the buffer's
/// fence, which here has signalled.
#[tokio::test]
async fn a_native_frame_reads_no_pixels() {
    // Only with GPU access does the guest get the host's linux-dmabuf.
    let mut s = Session::with_context(TestConnection::default().gpu_accel().context());
    let surface = s.surface().await;
    let dmabuf = s.bind_paired("zwp_linux_dmabuf_v1", 3).await;
    s.guest_received();
    s.host_received();
    // Injects what no peer can provide: each plane stands in for its own
    // sync_file, which a readable pipe is once the guest GPU is done.
    s.ctx().plane_fences_for_test = Some(PlaneFenceForTest::SyncFile);
    let (plane, signal) = crate::test_support::pipe();
    assert_eq!(nix::unistd::write(&signal, &[1]), Ok(1));
    let buffers = [
        s.native_buffer(dmabuf.id, plane.try_clone().unwrap(), WIDTH, HEIGHT)
            .await,
        s.native_buffer(dmabuf.id, plane, WIDTH, HEIGHT).await,
    ];
    let cost = cost_per_frame(&mut s, surface, buffers, PARTIAL_WIDTH).await;
    println!("frame cost, native dma-buf: {cost:?}");
    assert_eq!(cost.pool_reads, 0);
    assert_eq!(cost.host_maps, 0);
    assert!(cost.allocations <= 9, "{cost:?}");
}

/// The motions the message test measures, after the warm-up ones that
/// grow the proxy's buffers to their working size.
const MOTIONS: u64 = 1_000;
const WARM_UP_MOTIONS: u64 = 10;

/// Each `wl_pointer.motion` the host sends in its own receive, as a moving
/// pointer does, reaches the guest. Forwarding one costs the proxy at most
/// its budget of allocations.
#[tokio::test]
async fn a_forwarded_pointer_motion_costs_at_most_its_budget() {
    let mut s = Session::new();
    let seat = s.bind_paired("wl_seat", wl_seat::VERSION).await;
    let surface = s.surface().await;
    let pointer = s.pointer_on(seat.id, surface).await;
    let motions = async |s: &mut Session, count: u64| {
        for time in 0..count as u32 {
            s.host([wl_pointer::evt::motion(pointer.host, time, 256, 512)])
                .await;
            let motion = wl_pointer::Event::Motion {
                time,
                surface_x: 256,
                surface_y: 512,
            };
            assert_eq!(s.guest_received(), [(pointer.id, motion.into())]);
        }
    };
    motions(&mut s, WARM_UP_MOTIONS).await;
    s.proxy_cost();
    let started = super::tests::thread_cpu_time();
    motions(&mut s, MOTIONS).await;
    let cpu = super::tests::thread_cpu_time() - started;
    let total = s.proxy_cost();
    println!(
        "message cost, pointer motion: {} allocations per motion; {:?} of CPU per motion, \
         the test's own work included",
        total.allocations as f64 / MOTIONS as f64,
        cpu / MOTIONS as u32
    );
    let cost = total.per(MOTIONS);
    assert!(cost.allocations <= 1, "{cost:?}");
}
