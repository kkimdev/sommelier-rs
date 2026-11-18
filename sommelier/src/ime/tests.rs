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
use crate::protocols::text_input_unstable_v1::zwp_text_input_v1::{self, Request as V1};
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
/// The host `zwp_text_input_manager_v1`; the extension's manager is 3.
const MANAGER: u32 = 2;

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
            manager: 3,
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
                Key::Recover(ti, guest_events(&[done]))
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
            manager: MANAGER,
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
/// An empty preedit goes out only to clear a shown one, a delete only when
/// it deletes something, and a commit only when it commits something.
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
        let txn = |delete, commit: Option<&str>| GuestTxn {
            delete,
            commit: commit.map(str::to_string),
            preedit: shows.then(|| Preedit::new("나".into(), (0, 3)).unwrap()),
        };
        let case = (shown, deletes, commits, shows);
        let commit = commits.then_some("가");
        let encoded = txn(deletes.then_some((1, 2)), commit).encode(TI, shown, serial);
        assert_eq!(guest_events(&encoded.unwrap()), events, "{case:?}");
        // A zero-length delete is no delete, and an empty commit no
        // commit.
        if !deletes {
            let encoded = txn(Some((0, 0)), commit).encode(TI, shown, serial);
            assert_eq!(guest_events(&encoded.unwrap()), events, "{case:?}");
        }
        if !commits {
            let encoded = txn(deletes.then_some((1, 2)), Some("")).encode(TI, shown, serial);
            assert_eq!(guest_events(&encoded.unwrap()), events, "{case:?}");
        }
    }
    // An empty preedit is none, and its cursor is (0, 0).
    assert_eq!(Preedit::new(String::new(), (3, 3)), None);
    assert_eq!(guest::resolve_preedit_cursor("", None, Some(3)), (0, 0));
}

/// The production sources of the core.
const SOURCES: [(&str, &str); 6] = [
    ("mod.rs", include_str!("mod.rs")),
    ("guest.rs", include_str!("guest.rs")),
    ("host.rs", include_str!("host.rs")),
    ("serial.rs", include_str!("serial.rs")),
    ("state.rs", include_str!("state.rs")),
    ("step.rs", include_str!("step.rs")),
];

/// The file of each occurrence of `pattern` in [`SOURCES`].
fn occurrences(pattern: &str) -> Vec<&'static str> {
    SOURCES
        .iter()
        .flat_map(|(file, source)| source.matches(pattern).map(move |_| *file))
        .collect()
}

/// B4: the count has one writer, called from the Commit arm alone.
#[test]
fn count_bumps_only_in_the_commit_arm() {
    assert_eq!(occurrences(".bump()"), ["state.rs"]);
    assert_eq!(occurrences(".count_commit()"), ["step.rs"]);
}

/// B5 and the core's isolation: one builder of the v3 transaction
/// events, and no import from the rest of the proxy but the wire, the
/// generated protocols and the three state types the old code shares.
#[test]
fn ime_is_isolated() {
    for event in [
        "evt::preedit_string(",
        "evt::delete_surrounding_text(",
        "evt::commit_string(",
        "evt::done(",
    ] {
        let files = occurrences(event);
        assert!(
            !files.is_empty() && files.iter().all(|&file| file == "guest.rs"),
            "{event}"
        );
    }
    assert_eq!(occurrences("crate::handler"), [] as [&str; 0]);
    let state_types = |names: &str| {
        names
            .trim_matches(['{', '}'])
            .split(',')
            .all(|name| ["HostContentType", "V3Committed", "V3Pending"].contains(&name))
    };
    for (file, source) in SOURCES {
        let mut source = source;
        while let Some(start) = source.find("\nuse crate::") {
            let rest = &source[start + "\nuse crate::".len()..];
            let end = rest.find(';').expect("a use ends");
            let path: String = rest[..end].split_whitespace().collect();
            let allowed = path.starts_with("protocols::")
                || path.starts_with("wire::")
                || path.strip_prefix("state::").is_some_and(state_types);
            assert!(allowed, "{file} uses crate::{path}");
            source = rest;
        }
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
        manager: MANAGER,
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

// 2.2 Host events.

#[test]
fn row_H2_an_exo_detach_clears_the_preedit_and_activates_again() {
    let mut ime = Ime::enabled();
    ime.compose("가");
    let out = ime.step(Input::HostLeave(TI));
    assert_eq!(out.guest, [no_preedit(), done(1)]);
    let mut host = vec![req(
        v1(TI),
        V1::Activate {
            seat: HOST_SEAT,
            surface: HOST_SURFACE,
        },
    )];
    host.extend(content_type(TI, false));
    host.push(commit_state(TI, 1));
    assert_eq!(out.host, host);
    assert_eq!(ime.compose("나").guest, [preedit("나"), done(1)]);
}

#[test]
fn row_H2prime_a_leave_that_answers_a_deactivate_is_ignored() {
    let mut ime = Ime::new();
    ime.create(TI);
    let out = ime.step(Input::HostLeave(TI));
    assert!(out.host.is_empty() && out.guest.is_empty());
    ime.enable(TI);
    ime.step(Input::Disable(TI));
    ime.step(Input::Commit(TI));
    let out = ime.step(Input::HostLeave(TI));
    assert!(out.host.is_empty() && out.guest.is_empty());
}

#[test]
fn row_H4_a_selection_style_becomes_the_preedit_cursor() {
    let mut ime = Ime::enabled();
    let style = |style| HostIme::PreeditStyling {
        index: 3,
        length: 3,
        style,
    };
    ime.v1(style(zwp_text_input_v1::PREEDIT_STYLE_UNDERLINE));
    assert_eq!(ime.compose("가나").guest, [preedit("가나"), done(1)]);
    ime.v1(style(zwp_text_input_v1::PREEDIT_STYLE_SELECTION));
    assert_eq!(
        ime.compose("가나").guest,
        [preedit_at("가나", 3, 6), done(1)]
    );

    // Only an attached text input records one.
    ime.step(Input::Disable(TI));
    let barrier = ime.step(Input::Commit(TI)).barriers()[0];
    ime.v1(style(zwp_text_input_v1::PREEDIT_STYLE_SELECTION));
    ime.step(Input::BarrierDone(barrier));
    ime.enable(TI);
    assert_eq!(ime.compose("가나").guest, [preedit("가나"), done(3)]);
}

#[test]
fn row_H5_a_preedit_cursor_places_the_cursor() {
    let mut ime = Ime::enabled();
    for (cursor, expected) in [(3, (3, 3)), (1, (6, 6)), (-1, (-1, -1))] {
        ime.v1(HostIme::PreeditCursor(cursor));
        let out = ime.compose("가나");
        assert_eq!(
            out.guest,
            [preedit_at("가나", expected.0, expected.1), done(1)]
        );
    }
}

#[test]
fn row_H6_a_cursor_position_is_only_logged() {
    let mut ime = Ime::enabled();
    let out = ime.v1(HostIme::CursorPosition {
        index: 0,
        anchor: 0,
    });
    assert!(out.guest.is_empty() && out.host.is_empty() && out.keys.is_empty());
    let out = ime.v1(HostIme::Commit {
        serial: 0,
        text: "a".into(),
    });
    assert_eq!(out.guest, [commit("a"), done(1)]);
}

#[test]
fn row_H7_a_preedit_shows_and_its_clear_arms_the_lease() {
    let mut ime = Ime::enabled();
    let out = ime.compose("가");
    assert_eq!(out.guest, [preedit("가"), done(1)]);
    assert_eq!(out.keys, [lease(LeaseOutcome::End)]);
    let out = ime.compose("");
    assert_eq!(out.guest, [no_preedit(), done(1)]);
    assert_eq!(out.keys, [lease(LeaseOutcome::ArmIfBackspaceHeld)]);
    // An empty preedit while none shows sends its done alone.
    let out = ime.compose("");
    assert_eq!(out.guest, [done(1)]);
    assert_eq!(out.keys, [lease(LeaseOutcome::End)]);
}

#[test]
fn row_H7_a_cleared_preedit_with_commit_text_commits_it() {
    let mut ime = Ime::enabled();
    ime.report("ab", 2);
    ime.compose("가");
    let out = ime.v1(HostIme::Preedit {
        serial: 0,
        text: "".into(),
        commit: "가".into(),
    });
    assert_eq!(out.guest, [no_preedit(), commit("가"), done(2)]);
    assert_eq!(out.keys, [lease(LeaseOutcome::End)]);
    // The guest has yet to report that commit, against which Exo measures
    // its next absolute delete.
    let out = ime.v1(HostIme::Delete {
        index: 0,
        length: 2,
    });
    assert_eq!(out.guest, []);
    ime.report("ab가", 5);
    let out = ime.v1(HostIme::Delete {
        index: 2,
        length: 3,
    });
    assert_eq!(out.guest, [delete(3, 0), done(3)]);
}

#[test]
fn row_H7_a_preedit_that_does_not_fit_is_dropped() {
    let mut ime = Ime::enabled();
    ime.compose("가");
    ime.v1(HostIme::PreeditCursor(0));
    let out = ime.compose(&"x".repeat(4_084));
    assert!(out.guest.is_empty() && out.keys.is_empty());
    // The guest still shows its preedit, and the next one is placed by
    // its own cursor.
    let out = ime.step(Input::Commit(TI));
    assert_eq!(out.guest, [preedit("가"), done(2)]);
    assert_eq!(ime.compose("나").guest, [preedit("나"), done(2)]);
}

#[test]
fn row_H7prime_a_preedit_that_is_not_utf8_is_dropped() {
    let mut ime = Ime::enabled();
    ime.v1(HostIme::PreeditCursor(0));
    let out = ime.v1(HostIme::Preedit {
        serial: 0,
        text: IndexedString::NotUtf8(vec![0xff]),
        commit: String::new(),
    });
    assert!(out.guest.is_empty() && out.keys.is_empty());
    assert_eq!(ime.compose("나").guest, [preedit("나"), done(1)]);
}

#[test]
fn row_H8_a_commit_ends_the_preedit() {
    let mut ime = Ime::enabled();
    ime.compose("가");
    let out = ime.v1(HostIme::Commit {
        serial: 0,
        text: "각".into(),
    });
    assert_eq!(out.guest, [no_preedit(), commit("각"), done(1)]);
    assert_eq!(out.keys, [lease(LeaseOutcome::End)]);
    // One that does not fit is dropped.
    ime.compose("가");
    let out = ime.v1(HostIme::Commit {
        serial: 0,
        text: "x".repeat(4_090),
    });
    assert!(out.guest.is_empty() && out.keys.is_empty());
    assert_eq!(ime.step(Input::Commit(TI)).guest, [preedit("가"), done(2)]);
}

#[test]
fn row_H9_an_absolute_delete_becomes_lengths_around_the_cursor() {
    let mut ime = Ime::enabled();
    ime.report("abcd", 2);
    let out = ime.v1(HostIme::Delete {
        index: 1,
        length: 2,
    });
    assert_eq!(out.guest, [delete(1, 1), done(2)]);
    assert_eq!(out.keys, [lease(LeaseOutcome::End)]);
    // The guest has yet to report it: an absolute delete waits, and a
    // relative one counts from the cursor. A shown preedit is cleared.
    let absolute = HostIme::Delete {
        index: 0,
        length: 1,
    };
    assert_eq!(ime.v1(absolute).guest, []);
    ime.compose("가");
    let relative = HostIme::Delete {
        index: -1,
        length: 1,
    };
    assert_eq!(
        ime.v1(relative).guest,
        [no_preedit(), delete(1, 0), done(2)]
    );
}

#[test]
fn row_H9_a_delete_that_cannot_be_measured_is_dropped() {
    type Case = (&'static str, Option<(&'static str, i32, i32)>, i32, u32);
    let cases: [Case; 5] = [
        ("no surrounding text", None, 0, 1),
        ("a selection", Some(("abcd", 2, 0)), 0, 1),
        ("not around the cursor", Some(("abcd", 2, 2)), 3, 1),
        ("off a character boundary", Some(("가", 3, 3)), 1, 2),
        ("empty", Some(("ab", 1, 1)), 1, 0),
    ];
    for (case, text, index, length) in cases {
        let mut ime = Ime::enabled();
        if let Some((text, cursor, anchor)) = text {
            ime.step(Input::SurroundingText(TI, text.into(), cursor, anchor));
            ime.step(Input::Commit(TI));
        }
        let out = ime.v1(HostIme::Delete { index, length });
        assert!(out.guest.is_empty() && out.keys.is_empty(), "{case}");
    }
}

#[test]
fn row_H10_keysyms_go_to_an_attached_input_whatever_its_fence() {
    let mut ime = Ime::new();
    ime.create(TI);
    assert_eq!(ime.core.keysym_target(TI), None);
    ime.enable(TI);
    assert_eq!(ime.core.keysym_target(TI), Some(SEAT));
    ime.compose("가");
    ime.press(SURFACE);
    assert_eq!(ime.core.keysym_target(TI), Some(SEAT));
    ime.step(Input::Disable(TI));
    ime.step(Input::Commit(TI));
    assert_eq!(ime.core.keysym_target(TI), None);
}

#[test]
fn row_X1_a_preedit_region_turns_text_into_the_preedit() {
    let mut ime = Ime::enabled();
    ime.report("a가b", 4);
    let region = |index, length| HostIme::PreeditRegion { index, length };
    let out = ime.ext(region(-3, 3));
    assert_eq!(out.guest, [delete(3, 0), preedit_at("가", 3, 3), done(2)]);
    assert_eq!(out.keys, [lease(LeaseOutcome::End)]);
    // The assembly places its cursor, and the text being ahead does not
    // stop it.
    ime.v1(HostIme::PreeditCursor(0));
    let out = ime.ext(region(-3, 4));
    assert_eq!(
        out.guest,
        [no_preedit(), delete(3, 1), preedit_at("가b", 0, 0), done(2)]
    );
    // An empty one deletes nothing and clears the shown preedit.
    let out = ime.ext(region(0, 0));
    assert_eq!(out.guest, [no_preedit(), done(2)]);
}

#[test]
fn row_X1prime_a_region_the_text_cannot_hold_is_dropped() {
    let mut ime = Ime::enabled();
    let region = |index, length| HostIme::PreeditRegion { index, length };
    ime.v1(HostIme::PreeditCursor(0));
    let out = ime.ext(region(-1, 1));
    assert!(out.guest.is_empty() && out.keys.is_empty());
    assert_eq!(ime.compose("나").guest, [preedit("나"), done(1)]);
    ime.report("ab", 1);
    assert_eq!(ime.ext(region(1, 1)).guest, []);
}

#[test]
fn row_X2_a_confirmation_commits_the_shown_preedit() {
    let mut ime = Ime::enabled();
    ime.compose("가");
    let out = ime.ext(HostIme::Confirm);
    assert_eq!(out.guest, [no_preedit(), commit("가"), done(1)]);
    assert_eq!(out.keys, [lease(LeaseOutcome::End)]);
}

#[test]
fn row_X2prime_an_empty_confirmation_recovers_the_held_key() {
    let mut ime = Ime::enabled();
    let out = ime.ext(HostIme::Confirm);
    assert_eq!(out.guest, []);
    assert_eq!(out.keys, [Key::Recover(TI, vec![done(1)])]);
}

#[test]
fn row_W1_a_drained_link_activates_again() {
    let mut ime = Ime::enabled();
    ime.step(Input::Disable(TI));
    let barrier = ime.step(Input::Commit(TI)).barriers()[0];
    // An enable before the drain ends waits for it (R2).
    let out = ime.enable(TI);
    assert_eq!(
        out.host,
        [
            surrounding(TI, "", 0),
            cursor_rect(TI, (0, 0, 0, 0)),
            commit_state(TI, 3),
        ]
    );
    assert_eq!(out.guest, [done(3)]);
    let out = ime.step(Input::BarrierDone(barrier));
    let mut host = vec![req(
        v1(TI),
        V1::Activate {
            seat: HOST_SEAT,
            surface: HOST_SURFACE,
        },
    )];
    host.extend(content_type(TI, false));
    host.push(commit_state(TI, 3));
    assert_eq!(out.host, host);
}

#[test]
fn row_W2_a_fence_ends_only_with_its_own_barrier() {
    let mut ime = Ime::enabled();
    ime.compose("가");
    let fence = ime.press(SURFACE).barriers()[0];
    // A drain, its barrier and a new activation leave the fence pending.
    ime.step(Input::Disable(TI));
    let barrier = ime.step(Input::Commit(TI)).barriers()[0];
    ime.step(Input::BarrierDone(barrier));
    ime.enable(TI);
    assert_eq!(ime.compose("나").guest, []);
    ime.step(Input::BarrierDone(fence));
    assert_eq!(ime.compose("다").guest, [preedit("다"), done(3)]);
}

// 2.3 Cross-concern inputs and reconcile outcomes.

#[test]
fn row_F1_a_focus_change_resets_the_field() {
    let mut ime = Ime::enabled();
    ime.compose("가");
    let out = ime.step(Input::FocusChanged {
        seat: SEAT,
        surface: Some(OTHER),
    });
    assert_eq!(
        out.guest,
        [
            v3(TI, V3::Leave { surface: SURFACE.0 }),
            v3(TI, V3::Enter { surface: OTHER.0 }),
        ]
    );
    let [barrier] = out.barriers()[..] else {
        panic!("one barrier in {out:?}");
    };
    assert_eq!(
        out.host,
        [
            req(v1(TI), V1::Deactivate { seat: HOST_SEAT }),
            Host::Barrier(barrier),
        ]
    );
    // The keyboard concern ends the seat's leases.
    assert_eq!(out.keys, []);
    // The new field is disabled.
    assert_eq!(ime.step(Input::Commit(TI)).guest, []);
    let out = ime.step(Input::FocusChanged {
        seat: SEAT,
        surface: None,
    });
    assert_eq!(out.guest, [v3(TI, V3::Leave { surface: OTHER.0 })]);
    let out = ime.step(Input::FocusChanged {
        seat: SeatId(6),
        surface: Some(SURFACE),
    });
    assert_eq!(out.guest, []);
}

#[test]
fn row_F2_a_destroyed_surface_is_left_while_it_is_valid() {
    let mut ime = Ime::enabled();
    ime.world.surfaces.remove(&SURFACE);
    ime.world.focus.insert(SEAT, OTHER);
    let out = ime.step(Input::SurfaceDestroyed(SURFACE));
    assert_eq!(
        out.guest,
        [
            v3(TI, V3::Leave { surface: SURFACE.0 }),
            v3(TI, V3::Enter { surface: OTHER.0 }),
        ]
    );
    assert_eq!(out.host[0], req(v1(TI), V1::Deactivate { seat: HOST_SEAT }));

    // A repair that stays on the surface tells the guest nothing, and the
    // field counts as disabled until enabled again.
    let mut ime = Ime::enabled();
    let out = ime.step(Input::SurfaceDestroyed(SURFACE));
    assert_eq!(out.guest, []);
    assert_eq!(out.host[0], req(v1(TI), V1::Deactivate { seat: HOST_SEAT }));
    assert_eq!(ime.step(Input::Commit(TI)).guest, []);
}

#[test]
fn row_F3_a_bound_seat_carries_the_deferred_activation() {
    let mut ime = Ime::new();
    ime.world.seats.clear();
    ime.create(TI);
    let out = ime.enable(TI);
    assert_eq!(
        out.host,
        [
            surrounding(TI, "", 0),
            cursor_rect(TI, (0, 0, 0, 0)),
            commit_state(TI, 1),
        ]
    );
    ime.world.seats.insert(SEAT, HOST_SEAT);
    let out = ime.step(Input::SeatBound(SEAT));
    let mut host = vec![req(
        v1(TI),
        V1::Activate {
            seat: HOST_SEAT,
            surface: HOST_SURFACE,
        },
    )];
    host.extend(content_type(TI, false));
    host.push(commit_state(TI, 1));
    assert_eq!(out.host, host);
}

#[test]
fn row_F4_a_press_commits_the_preedit_then_resends_the_press() {
    let mut ime = Ime::enabled();
    ime.compose("가");
    ime.v1(HostIme::PreeditCursor(0));
    let out = ime.press(SURFACE);
    let button = wl_pointer::Event::Button {
        serial: 7,
        time: 8,
        button: 0x110,
        state: 1,
    };
    assert_eq!(
        out.guest,
        [
            no_preedit(),
            commit("가"),
            done(1),
            (POINTER, button.into())
        ]
    );
    let [fence] = out.barriers()[..] else {
        panic!("one barrier in {out:?}");
    };
    assert_eq!(out.host, [req(v1(TI), V1::Reset {}), Host::Barrier(fence)]);
    assert_eq!(out.keys, [lease(LeaseOutcome::End)]);
    assert!(!out.forward);
    // The press took the assembly.
    ime.step(Input::BarrierDone(fence));
    assert_eq!(ime.compose("나").guest, [preedit("나"), done(1)]);
}

#[test]
fn row_F4_a_press_elsewhere_or_without_a_composition_is_forwarded() {
    let mut ime = Ime::enabled();
    let out = ime.press(SURFACE);
    assert!(out.forward && out.guest.is_empty() && out.host.is_empty());
    ime.compose("가");
    let out = ime.press(OTHER);
    assert!(out.forward && out.guest.is_empty() && out.host.is_empty());
}

#[test]
fn row_F4_events_in_flight_at_a_press_are_dropped_and_take_the_assembly() {
    type Event = (&'static str, fn() -> Input);
    let events: [Event; 5] = [
        ("preedit", || {
            Input::V1(
                TI,
                HostIme::Preedit {
                    serial: 0,
                    text: "각".into(),
                    commit: "각".into(),
                },
            )
        }),
        ("commit", || {
            Input::V1(
                TI,
                HostIme::Commit {
                    serial: 0,
                    text: "가".into(),
                },
            )
        }),
        ("delete", || {
            Input::V1(
                TI,
                HostIme::Delete {
                    index: -1,
                    length: 1,
                },
            )
        }),
        ("region", || Input::Ext {
            ext: ext_id(TI),
            event: HostIme::PreeditRegion {
                index: -1,
                length: 1,
            },
        }),
        ("confirm", || Input::Ext {
            ext: ext_id(TI),
            event: HostIme::Confirm,
        }),
    ];
    for (event, input) in events {
        let mut ime = Ime::enabled();
        ime.report("ab", 2);
        ime.compose("가");
        let fence = ime.press(SURFACE).barriers()[0];
        let selection = zwp_text_input_v1::PREEDIT_STYLE_SELECTION;
        ime.v1(HostIme::PreeditStyling {
            index: 0,
            length: 3,
            style: selection,
        });
        ime.v1(HostIme::PreeditCursor(0));
        let out = ime.step(input());
        assert!(out.guest.is_empty() && out.keys.is_empty(), "{event}");
        ime.step(Input::BarrierDone(fence));
        assert_eq!(ime.compose("나").guest, [preedit("나"), done(2)], "{event}");
    }
}

#[test]
fn row_F6_a_host_deleted_text_input_sends_nothing_to_its_v1_object() {
    let mut ime = Ime::enabled();
    let out = ime.step(Input::Destroy {
        ti: TI,
        host_deleted: true,
    });
    assert_eq!(
        out.host,
        [req(ext_id(TI), Ext::Destroy {}), Host::Retire(ext_id(TI))]
    );
}

#[test]
fn row_R1_a_dead_seat_abandons_the_link_and_keeps_the_fence() {
    let mut ime = Ime::enabled();
    ime.compose("가");
    // The focus moves while the seat is dead: no deactivate can go, and the
    // link ends locally with its composition (I2).
    ime.world.seats.clear();
    let out = ime.step(Input::FocusChanged {
        seat: SEAT,
        surface: Some(OTHER),
    });
    assert_eq!(out.host, []);
    // The link is detached, so the field activates again once the seat is
    // back.
    ime.world.seats.insert(SEAT, HOST_SEAT);
    let mut host = activation(TI, 2);
    host[0] = req(
        v1(TI),
        V1::Activate {
            seat: HOST_SEAT,
            surface: HOST_OTHER,
        },
    );
    assert_eq!(ime.enable(TI).host, host);

    let mut ime = Ime::enabled();
    ime.compose("가");
    let fence = ime.press(SURFACE).barriers()[0];
    ime.world.seats.clear();
    ime.step(Input::Disable(TI));
    let out = ime.step(Input::Commit(TI));
    assert_eq!(
        out.host,
        [
            surrounding(TI, "", 0),
            cursor_rect(TI, (0, 0, 0, 0)),
            commit_state(TI, 2),
        ]
    );
    ime.world.seats.insert(SEAT, HOST_SEAT);
    assert_eq!(ime.enable(TI).host, activation(TI, 3));
    assert_eq!(ime.compose("나").guest, []);
    ime.step(Input::BarrierDone(fence));
    assert_eq!(ime.compose("다").guest, [preedit("다"), done(3)]);
}

#[test]
fn row_R2_an_activation_waits_for_a_live_seat() {
    let mut ime = Ime::new();
    ime.world.seats.clear();
    ime.create(TI);
    ime.enable(TI);
    for _ in 0..2 {
        let out = ime.step(Input::SeatBound(SeatId(6)));
        assert!(out.host.is_empty());
    }
    assert_eq!(ime.compose("가").guest, []);
}
