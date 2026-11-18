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

//! One test per row of section 2 of the design, named `row_<id>_<what>`,
//! against the core alone. Every step of every test goes through
//! [`Ime::step`], which checks the invariants after it.

// Test names keep the case of the row IDs they cite.
#![allow(non_snake_case)]

use std::collections::BTreeMap;

use super::guest::{GuestTxn, Preedit};
use super::serial::CommitCount;
use super::*;
use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1::{
    self as ext, Request as Ext,
};
use crate::protocols::text_input_extension_unstable_v1::zcr_text_input_extension_v1::Request as ExtManager;
use crate::protocols::text_input_unstable_v1::zwp_text_input_manager_v1::Request as V1Manager;
use crate::protocols::text_input_unstable_v1::zwp_text_input_v1::Request as V1;
use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::{self, Event as V3};
use crate::protocols::wayland::wl_pointer;
use crate::protocols::{AnyEvent, AnyRequest};
use crate::test_support::decoded;

const TI: TiId = TiId(10);
const TI2: TiId = TiId(11);
const SEAT: SeatId = SeatId(5);
const HOST_SEAT: u32 = 105;
const SURFACE: SurfaceId = SurfaceId(20);
const HOST_SURFACE: u32 = 120;
const OTHER: SurfaceId = SurfaceId(21);
const HOST_OTHER: u32 = 121;
/// The guest pointer whose press [`Ime::press`] re-sends.
const POINTER: u32 = 40;
const MANAGERS: HostManagers = HostManagers {
    v1: 2,
    ext: Some(3),
};

/// The host `zwp_text_input_v1` of `ti`.
fn v1(ti: TiId) -> u32 {
    200 + ti.0
}

/// The host `zcr_extended_text_input_v1` of `ti`.
fn ext_id(ti: TiId) -> u32 {
    300 + ti.0
}

fn hosts(ti: TiId) -> HostTi {
    HostTi {
        v1: v1(ti),
        ext: Some(HostExt {
            id: ext_id(ti),
            version: ext::VERSION,
        }),
    }
}

/// The interface of each object the tests name, by ID.
fn interface(id: u32) -> &'static str {
    match id {
        2 => "zwp_text_input_manager_v1",
        3 => "zcr_text_input_extension_v1",
        200..300 => "zwp_text_input_v1",
        300..400 => "zcr_extended_text_input_v1",
        POINTER => "wl_pointer",
        _ => "zwp_text_input_v3",
    }
}

/// The facts the core reads, as maps.
#[derive(Default)]
struct World {
    surfaces: BTreeMap<SurfaceId, u32>,
    seats: BTreeMap<SeatId, u32>,
    focus: BTreeMap<SeatId, SurfaceId>,
}

impl Env for World {
    fn host_surface(&self, surface: SurfaceId) -> Option<u32> {
        self.surfaces.get(&surface).copied()
    }

    fn host_seat(&self, seat: SeatId) -> Option<u32> {
        self.seats.get(&seat).copied()
    }

    fn keyboard_focus(&self, seat: SeatId) -> Option<SurfaceId> {
        self.focus.get(&seat).copied()
    }
}

/// A host output, decoded.
#[derive(Debug, PartialEq)]
enum Host {
    Req(u32, AnyRequest),
    Barrier(BarrierToken),
    Retire(u32),
}

/// A request to the keyboard concern, decoded.
#[derive(Debug, PartialEq)]
enum Key {
    Lease(TiId, LeaseOutcome),
    Recover(TiId, Vec<(u32, AnyEvent)>),
}

/// The effects of one step, decoded.
#[derive(Debug)]
struct Out {
    guest: Vec<(u32, AnyEvent)>,
    host: Vec<Host>,
    keys: Vec<Key>,
    forward: bool,
}

impl Out {
    /// The barriers among the host output.
    fn barriers(&self) -> Vec<BarrierToken> {
        self.host
            .iter()
            .filter_map(|out| match out {
                Host::Barrier(token) => Some(*token),
                _ => None,
            })
            .collect()
    }
}

fn guest_events(queue: &[Outgoing]) -> Vec<(u32, AnyEvent)> {
    decoded(queue, |m| AnyEvent::from_wire(interface(m.sender_id), m))
}

fn decode(fx: Effects) -> Out {
    let host = fx
        .to_host
        .into_iter()
        .map(|out| match out {
            HostOut::Request(request) => {
                let [(id, request)] = decoded(&[request], |m| {
                    AnyRequest::from_wire(interface(m.sender_id), m)
                })
                .try_into()
                .unwrap();
                Host::Req(id, request)
            }
            HostOut::Barrier(token) => Host::Barrier(token),
            HostOut::Retire(id) => Host::Retire(id),
        })
        .collect();
    let keys = fx
        .keys
        .into_iter()
        .map(|key| match key {
            KeyEffect::Lease { ti, seat, outcome } => {
                assert_eq!(seat, SEAT);
                Key::Lease(ti, outcome)
            }
            KeyEffect::RecoverHeldKey { ti, seat, done } => {
                assert_eq!(seat, SEAT);
                Key::Recover(ti, guest_events(&done))
            }
        })
        .collect();
    Out {
        guest: guest_events(&fx.to_guest),
        host,
        keys,
        forward: fx.forward,
    }
}

/// A core and the world it reads: one live seat whose keyboard focuses
/// [`SURFACE`], and two surfaces the host knows.
struct Ime {
    core: ImeCore,
    world: World,
}

impl Ime {
    fn new() -> Self {
        let world = World {
            surfaces: BTreeMap::from([(SURFACE, HOST_SURFACE), (OTHER, HOST_OTHER)]),
            seats: BTreeMap::from([(SEAT, HOST_SEAT)]),
            focus: BTreeMap::from([(SEAT, SURFACE)]),
        };
        Self {
            core: ImeCore::default(),
            world,
        }
    }

    /// Step, then check the invariants.
    fn step(&mut self, input: Input) -> Out {
        let fx = self.core.step(input, &self.world);
        self.core.assert_invariants(&self.world);
        decode(fx)
    }

    fn create(&mut self, ti: TiId) -> Out {
        self.step(Input::Create {
            ti,
            seat: SEAT,
            host: hosts(ti),
            managers: MANAGERS,
        })
    }

    /// `ti` enables and commits.
    fn enable(&mut self, ti: TiId) -> Out {
        assert!(self.step(Input::Enable(ti)).host.is_empty());
        self.step(Input::Commit(ti))
    }

    /// [`TI`], created and enabled with its first commit, attached.
    fn enabled() -> Self {
        let mut ime = Self::new();
        ime.create(TI);
        ime.enable(TI);
        ime
    }

    fn v1(&mut self, event: HostIme) -> Out {
        self.step(Input::V1(TI, event))
    }

    fn ext(&mut self, event: HostIme) -> Out {
        self.step(Input::Ext {
            ext: ext_id(TI),
            event,
        })
    }

    /// Exo composes `text`, which it also sends as the commit text.
    fn compose(&mut self, text: &str) -> Out {
        self.v1(HostIme::Preedit {
            serial: 0,
            text: text.into(),
            commit: text.to_string(),
        })
    }

    /// [`TI`]'s guest commits surrounding text `text` with the cursor at
    /// `cursor`.
    fn report(&mut self, text: &str, cursor: i32) -> Out {
        self.step(Input::SurroundingText(TI, text.into(), cursor, cursor));
        self.step(Input::Commit(TI))
    }

    /// A pointer press on `surface`.
    fn press(&mut self, surface: SurfaceId) -> Out {
        self.step(Input::Press {
            surface,
            resend: wl_pointer::evt::button(POINTER, 7, 8, 0x110, 1),
        })
    }
}

fn v3(ti: TiId, event: V3) -> (u32, AnyEvent) {
    (ti.0, event.into())
}

fn done(serial: u32) -> (u32, AnyEvent) {
    v3(TI, V3::Done { serial })
}

fn preedit_at(text: &str, cursor_begin: i32, cursor_end: i32) -> (u32, AnyEvent) {
    let text = Some(text.into());
    v3(
        TI,
        V3::PreeditString {
            text,
            cursor_begin,
            cursor_end,
        },
    )
}

/// The preedit `text` with the cursor at its end.
fn preedit(text: &str) -> (u32, AnyEvent) {
    preedit_at(text, text.len() as i32, text.len() as i32)
}

fn no_preedit() -> (u32, AnyEvent) {
    preedit_at("", 0, 0)
}

fn commit(text: &str) -> (u32, AnyEvent) {
    let text = Some(text.into());
    v3(TI, V3::CommitString { text })
}

fn delete(before_length: u32, after_length: u32) -> (u32, AnyEvent) {
    v3(
        TI,
        V3::DeleteSurroundingText {
            before_length,
            after_length,
        },
    )
}

fn req(id: u32, request: impl Into<AnyRequest>) -> Host {
    Host::Req(id, request.into())
}

fn surrounding(ti: TiId, text: &str, cursor: u32) -> Host {
    let text = text.into();
    req(
        v1(ti),
        V1::SetSurroundingText {
            text,
            cursor,
            anchor: cursor,
        },
    )
}

fn cursor_rect(ti: TiId, (x, y, width, height): (i32, i32, i32, i32)) -> Host {
    req(
        v1(ti),
        V1::SetCursorRectangle {
            x,
            y,
            width,
            height,
        },
    )
}

fn commit_state(ti: TiId, serial: u32) -> Host {
    req(v1(ti), V1::CommitState { serial })
}

/// The default content type, as the host's newest extension takes it.
fn content_type(ti: TiId, supported: bool) -> [Host; 2] {
    let support = match supported {
        true => ext::SURROUNDING_TEXT_SUPPORT_SUPPORTED,
        false => ext::SURROUNDING_TEXT_SUPPORT_UNSUPPORTED,
    };
    [
        req(ext_id(ti), Ext::SetSurroundingTextSupport { support }),
        req(
            ext_id(ti),
            Ext::SetInputType {
                input_type: ext::INPUT_TYPE_TEXT,
                input_mode: ext::INPUT_MODE_DEFAULT,
                input_flags: ext::INPUT_FLAGS_NONE,
                learning_mode: ext::LEARNING_MODE_ENABLED,
                inline_composition_support: ext::INLINE_COMPOSITION_SUPPORT_SUPPORTED,
            },
        ),
    ]
}

/// What an enable's commit, the guest's `serial`th, sends the host on
/// [`SURFACE`].
fn activation(ti: TiId, serial: u32) -> Vec<Host> {
    let mut host = vec![
        req(
            v1(ti),
            V1::Activate {
                seat: HOST_SEAT,
                surface: HOST_SURFACE,
            },
        ),
        surrounding(ti, "", 0),
    ];
    host.extend(content_type(ti, false));
    host.extend([cursor_rect(ti, (0, 0, 0, 0)), commit_state(ti, serial)]);
    host
}

fn lease(outcome: LeaseOutcome) -> Key {
    Key::Lease(TI, outcome)
}

/// Every combination of a transaction's parts, and whether the guest
/// shows a preedit, encodes in mutter's and KWin's order: the clear of a
/// shown preedit, the delete, the commit, the new preedit, then `done`.
/// An empty preedit goes out only to clear a shown one, and a delete only
/// when it deletes something.
#[test]
fn guest_txn_encodes_in_canonical_order() {
    // Per combination of (shown before, delete, commit, preedit), the
    // events before `done`: `c` the clear, `d` the delete, `m` the commit
    // and `p` the preedit.
    let expected = [
        ((false, false, false, false), ""),
        ((false, false, false, true), "p"),
        ((false, false, true, false), "m"),
        ((false, false, true, true), "mp"),
        ((false, true, false, false), "d"),
        ((false, true, false, true), "dp"),
        ((false, true, true, false), "dm"),
        ((false, true, true, true), "dmp"),
        ((true, false, false, false), "c"),
        ((true, false, false, true), "p"),
        ((true, false, true, false), "cm"),
        ((true, false, true, true), "cmp"),
        ((true, true, false, false), "cd"),
        ((true, true, false, true), "cdp"),
        ((true, true, true, false), "cdm"),
        ((true, true, true, true), "cdmp"),
    ];
    let serial = CommitCount::default().serial();
    for ((shown, deletes, commits, shows), parts) in expected {
        let event = |part| match part {
            'c' => no_preedit(),
            'd' => delete(1, 2),
            'm' => commit("가"),
            'p' => preedit_at("나", 0, 3),
            _ => unreachable!(),
        };
        let mut events: Vec<_> = parts.chars().map(event).collect();
        events.push(done(0));
        let txn = |delete| GuestTxn {
            delete,
            commit: commits.then(|| "가".into()),
            preedit: shows.then(|| Preedit {
                text: "나".into(),
                cursor: (0, 3),
            }),
        };
        let case = (shown, deletes, commits, shows);
        let encoded = txn(deletes.then_some((1, 2))).encode(TI, shown, serial);
        assert_eq!(guest_events(&encoded.unwrap()), events, "{case:?}");
        // A zero-length delete is no delete.
        if !deletes {
            let encoded = txn(Some((0, 0))).encode(TI, shown, serial);
            assert_eq!(guest_events(&encoded.unwrap()), events, "{case:?}");
        }
    }
}

#[test]
fn count_bumps_only_in_the_commit_arm() {
    let sources = [
        ("mod.rs", include_str!("mod.rs")),
        ("guest.rs", include_str!("guest.rs")),
        ("host.rs", include_str!("host.rs")),
        ("serial.rs", include_str!("serial.rs")),
        ("state.rs", include_str!("state.rs")),
        ("step.rs", include_str!("step.rs")),
    ];
    let calls = |pattern: &str| {
        sources
            .iter()
            .flat_map(|(file, source)| source.matches(pattern).map(move |_| *file))
            .collect::<Vec<_>>()
    };
    assert_eq!(calls(".bump()"), ["state.rs"]);
    assert_eq!(calls(".count_commit()"), ["step.rs"]);
    // B5: one builder of the v3 transaction events.
    for event in [
        "evt::preedit_string(",
        "evt::delete_surrounding_text(",
        "evt::commit_string(",
        "evt::done(",
    ] {
        let files = calls(event);
        assert!(
            !files.is_empty() && files.iter().all(|&file| file == "guest.rs"),
            "{event}"
        );
    }
}

// 2.1 Guest requests.

#[test]
fn row_G1_create_makes_the_host_objects_and_enters_the_focus() {
    let mut ime = Ime::new();
    let out = ime.create(TI);
    assert_eq!(
        out.host,
        [
            req(2, V1Manager::CreateTextInput { id: v1(TI) }),
            req(
                3,
                ExtManager::GetExtendedTextInput {
                    id: ext_id(TI),
                    text_input: v1(TI),
                }
            ),
        ]
    );
    assert_eq!(out.guest, [v3(TI, V3::Enter { surface: SURFACE.0 })]);

    // Without the extension or a keyboard focus, there is neither.
    ime.world.focus.clear();
    let out = ime.step(Input::Create {
        ti: TI2,
        seat: SEAT,
        host: HostTi {
            v1: v1(TI2),
            ext: None,
        },
        managers: HostManagers { v1: 2, ext: None },
    });
    assert_eq!(
        out.host,
        [req(2, V1Manager::CreateTextInput { id: v1(TI2) })]
    );
    assert_eq!(out.guest, []);
}

#[test]
fn row_G3_an_enable_waits_for_its_commit() {
    let mut ime = Ime::new();
    ime.create(TI);
    let out = ime.step(Input::Enable(TI));
    assert!(out.host.is_empty() && out.guest.is_empty());
    let out = ime.step(Input::Commit(TI));
    assert_eq!(out.host, activation(TI, 1));
    assert_eq!(out.guest, [done(1)]);
}

#[test]
fn row_G4_a_disable_deactivates_behind_a_barrier() {
    let mut ime = Ime::enabled();
    ime.compose("가");
    ime.step(Input::Disable(TI));
    let out = ime.step(Input::Commit(TI));
    let [barrier] = out.barriers()[..] else {
        panic!("one barrier in {out:?}");
    };
    assert_eq!(
        out.host,
        [
            req(v1(TI), V1::Deactivate { seat: HOST_SEAT }),
            Host::Barrier(barrier),
            surrounding(TI, "", 0),
            cursor_rect(TI, (0, 0, 0, 0)),
            commit_state(TI, 2),
        ]
    );
    // A new field ends the composition with no guest output, since the
    // guest reset itself, and a disabled field gets no done.
    assert_eq!(out.guest, []);
    assert_eq!(out.keys, [lease(LeaseOutcome::End)]);
}

#[test]
fn row_G5_surrounding_text_is_checked_then_sent_with_its_commit() {
    let mut ime = Ime::enabled();
    ime.report("a가b", 4);
    // An offset inside a character is ignored.
    ime.step(Input::SurroundingText(TI, "a가b".into(), 2, 2));
    let out = ime.step(Input::Commit(TI));
    assert_eq!(out.host, [commit_state(TI, 3)]);
    // Text that is not UTF-8 empties the field's text.
    let not_utf8 = IndexedString::NotUtf8(vec![0xff]);
    ime.step(Input::SurroundingText(TI, not_utf8, 1, 1));
    let out = ime.step(Input::Commit(TI));
    assert_eq!(out.host, [surrounding(TI, "", 0), commit_state(TI, 4)]);
    assert_eq!(out.guest, [done(4)]);
}

#[test]
fn row_G7_a_content_type_reaches_the_attached_input_method_once() {
    let mut ime = Ime::enabled();
    let password = (0, zwp_text_input_v3::CONTENT_PURPOSE_PASSWORD);
    ime.step(Input::ContentType(TI, password));
    let out = ime.step(Input::Commit(TI));
    let [support, _] = content_type(TI, false);
    let input_type = Ext::SetInputType {
        input_type: ext::INPUT_TYPE_PASSWORD,
        input_mode: ext::INPUT_MODE_DEFAULT,
        input_flags: ext::INPUT_FLAGS_HAS_BEEN_PASSWORD,
        learning_mode: ext::LEARNING_MODE_DISABLED,
        inline_composition_support: ext::INLINE_COMPOSITION_SUPPORT_SUPPORTED,
    };
    assert_eq!(
        out.host,
        [support, req(ext_id(TI), input_type), commit_state(TI, 2)]
    );
    ime.step(Input::ContentType(TI, password));
    let out = ime.step(Input::Commit(TI));
    assert_eq!(out.host, [commit_state(TI, 3)]);
}

#[test]
fn row_G8_a_cursor_rectangle_goes_with_its_commit() {
    let mut ime = Ime::enabled();
    ime.step(Input::CursorRect(TI, (1, 2, 3, 4)));
    let out = ime.step(Input::Commit(TI));
    assert_eq!(
        out.host,
        [cursor_rect(TI, (1, 2, 3, 4)), commit_state(TI, 2)]
    );
}

#[test]
fn row_G9a_an_unfocused_commit_only_counts() {
    let mut ime = Ime::new();
    ime.world.focus.clear();
    ime.create(TI);
    for _ in 0..2 {
        let out = ime.step(Input::Commit(TI));
        assert!(out.host.is_empty() && out.guest.is_empty());
    }
    ime.world.focus.insert(SEAT, SURFACE);
    let out = ime.step(Input::FocusChanged {
        seat: SEAT,
        surface: Some(SURFACE),
    });
    assert_eq!(out.guest, [v3(TI, V3::Enter { surface: SURFACE.0 })]);
    let out = ime.enable(TI);
    assert_eq!(out.host, activation(TI, 3));
    assert_eq!(out.guest, [done(3)]);
}

#[test]
fn row_G9b_a_commit_whose_text_does_not_fit_only_counts() {
    let mut ime = Ime::enabled();
    ime.compose("가");
    let long = "x".repeat(4_084);
    ime.step(Input::SurroundingText(
        TI,
        long.as_str().into(),
        4_084,
        4_084,
    ));
    let password = (0, zwp_text_input_v3::CONTENT_PURPOSE_PASSWORD);
    ime.step(Input::ContentType(TI, password));
    let out = ime.step(Input::Commit(TI));
    assert_eq!(out.host, []);
    assert_eq!(out.guest, [preedit("가"), done(2)]);
    assert_eq!(out.keys, []);
    // The commit spent its pending state.
    let out = ime.step(Input::Commit(TI));
    assert_eq!(out.host, [commit_state(TI, 3)]);
    assert_eq!(out.guest, [preedit("가"), done(3)]);
}

#[test]
fn row_G9c_a_commit_of_an_enabled_field_repeats_the_shown_preedit() {
    let mut ime = Ime::enabled();
    ime.v1(HostIme::PreeditCursor(0));
    ime.compose("가");
    let out = ime.step(Input::Commit(TI));
    assert_eq!(out.host, [commit_state(TI, 2)]);
    assert_eq!(out.guest, [preedit_at("가", 0, 0), done(2)]);
}

#[test]
fn row_G9c_reenabling_an_enabled_field_keeps_its_composition() {
    let mut ime = Ime::enabled();
    ime.compose("가");
    let out = ime.enable(TI);
    // The host keeps its content type.
    assert_eq!(
        out.host,
        [
            surrounding(TI, "", 0),
            cursor_rect(TI, (0, 0, 0, 0)),
            commit_state(TI, 2),
        ]
    );
    assert_eq!(out.guest, [preedit("가"), done(2)]);
    assert_eq!(out.keys, []);
}

#[test]
fn row_G9c_an_enable_while_another_is_enabled_applies_the_rest() {
    let mut ime = Ime::enabled();
    ime.create(TI2);
    ime.step(Input::Enable(TI2));
    ime.step(Input::SurroundingText(TI2, "a".into(), 1, 1));
    let out = ime.step(Input::Commit(TI2));
    // No activation and no done: the field stays disabled, and counts as
    // a new one.
    assert_eq!(
        out.host,
        [
            surrounding(TI2, "a", 1),
            cursor_rect(TI2, (0, 0, 0, 0)),
            commit_state(TI2, 1),
        ]
    );
    assert_eq!(out.guest, []);
    assert_eq!(out.keys, [Key::Lease(TI2, LeaseOutcome::End)]);
}

#[test]
fn row_G9c_surrounding_text_support_follows_the_field() {
    let mut ime = Ime::enabled();
    let out = ime.report("a", 1);
    let mut host = vec![surrounding(TI, "a", 1)];
    host.extend(content_type(TI, true));
    host.push(commit_state(TI, 2));
    assert_eq!(out.host, host);
    let out = ime.report("b", 1);
    assert_eq!(out.host, [surrounding(TI, "b", 1), commit_state(TI, 3)]);
}

#[test]
fn row_G10_destroy_deactivates_and_destroys_the_extension() {
    let mut ime = Ime::enabled();
    ime.compose("가");
    let out = ime.step(Input::Destroy {
        ti: TI,
        host_deleted: false,
    });
    let [barrier] = out.barriers()[..] else {
        panic!("one barrier in {out:?}");
    };
    assert_eq!(
        out.host,
        [
            req(v1(TI), V1::Deactivate { seat: HOST_SEAT }),
            Host::Barrier(barrier),
            req(ext_id(TI), Ext::Destroy {}),
            Host::Retire(ext_id(TI)),
        ]
    );
    assert_eq!(out.guest, []);
    assert_eq!(out.keys, [lease(LeaseOutcome::End)]);
    // Its barrier's done, and its extension's events, find nothing.
    let out = ime.step(Input::BarrierDone(barrier));
    assert!(out.host.is_empty() && out.guest.is_empty());
    let out = ime.ext(HostIme::Confirm);
    assert!(out.guest.is_empty() && out.keys.is_empty());
}

#[test]
fn row_G10_destroy_of_the_enabled_input_lets_another_enable() {
    let mut ime = Ime::enabled();
    ime.create(TI2);
    ime.step(Input::Destroy {
        ti: TI,
        host_deleted: false,
    });
    let out = ime.enable(TI2);
    assert_eq!(out.host, activation(TI2, 1));
    assert_eq!(out.guest, [v3(TI2, V3::Done { serial: 1 })]);
}
