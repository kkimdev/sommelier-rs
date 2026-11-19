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
    GluedBarrier(BarrierToken),
    Retire(u32),
}

/// A request to the keyboard concern, decoded.
#[derive(Debug, PartialEq)]
enum Key {
    Lease(TiId, LeaseOutcome),
    Recover(TiId),
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
    /// The barriers glued to a report among the host output.
    fn glued(&self) -> Vec<BarrierToken> {
        self.host
            .windows(2)
            .filter_map(|pair| match pair {
                [Host::Req(_, AnyRequest::ZwpTextInputV1(V1::SetSurroundingText { .. })), Host::GluedBarrier(token)] => {
                    Some(*token)
                }
                _ => None,
            })
            .collect()
    }

    /// The barriers among the host output, glued or not.
    fn barriers(&self) -> Vec<BarrierToken> {
        self.host
            .iter()
            .filter_map(|out| match out {
                Host::Barrier(token) | Host::GluedBarrier(token) => Some(*token),
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
            HostOut::GluedBarrier(token) => Host::GluedBarrier(token),
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
            KeyEffect::RecoverHeldKey { ti, seat } => {
                assert_eq!(seat, SEAT);
                Key::Recover(ti)
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

    /// [`Self::step_unanswered`], then Exo processes each report it sent:
    /// the barrier glued to it, which the output leaves out, is done.
    fn step(&mut self, input: Input) -> Out {
        let mut out = self.step_unanswered(input);
        let glued = out.glued();
        for &token in &glued {
            self.step_unanswered(Input::BarrierDone(token));
        }
        out.host
            .retain(|out| !matches!(out, Host::GluedBarrier(token) if glued.contains(token)));
        out
    }

    /// Step, then check the invariants.
    fn step_unanswered(&mut self, input: Input) -> Out {
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

    /// [`Self::report`] that Exo has yet to process: returns the barrier
    /// whose `done` says it has.
    fn report_unanswered(&mut self, text: &str, cursor: i32) -> BarrierToken {
        self.step(Input::SurroundingText(TI, text.into(), cursor, cursor));
        let [barrier] = self.step_unanswered(Input::Commit(TI)).glued()[..] else {
            panic!("a report glued to its barrier");
        };
        barrier
    }

    /// Exo commits `text`.
    fn host_commit(&mut self, text: &str) -> Out {
        let text = text.to_owned();
        self.v1(HostIme::Commit { serial: 0, text })
    }

    fn region(&mut self, index: i32, length: u32) -> Out {
        self.ext(HostIme::PreeditRegion { index, length })
    }

    fn delete(&mut self, index: i32, length: u32) -> Out {
        self.v1(HostIme::Delete { index, length })
    }

    /// Whether [`TI`]'s next delete or region is measured in the mirror.
    fn in_step(&self) -> bool {
        self.core.get(TI).expect("a text input").exo().in_step()
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
const SOURCES: [(&str, &str); 8] = [
    ("mod.rs", include_str!("mod.rs")),
    ("exo_view.rs", include_str!("exo_view.rs")),
    ("guest.rs", include_str!("guest.rs")),
    ("host.rs", include_str!("host.rs")),
    ("mirror.rs", include_str!("mirror.rs")),
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

/// B5 and the core's isolation: one builder of the v3 transaction events
/// in the crate, and no import from the rest of the proxy but the wire, the
/// generated protocols, and the size of a host submission, which tells
/// whether a report and its barrier reach Exo in one read.
#[test]
fn ime_is_isolated() {
    const BUILDERS: [&str; 4] = [
        "preedit_string",
        "delete_surrounding_text",
        "commit_string",
        "done",
    ];
    for builder in BUILDERS {
        let files = occurrences(&format!("evt::{builder}("));
        assert!(
            !files.is_empty() && files.iter().all(|&file| file == "guest.rs"),
            "{builder}"
        );
    }
    let builders = crate_sources()
        .into_iter()
        .filter(|(_, source)| builds_v3_transactions(source))
        .map(|(file, _)| file)
        .collect::<Vec<_>>();
    assert_eq!(builders, ["ime/guest.rs"]);
    assert_eq!(occurrences("crate::handler"), [] as [&str; 0]);
    for (file, source) in SOURCES {
        let mut source = source;
        while let Some(start) = source.find("use crate::") {
            let rest = &source[start + "use crate::".len()..];
            let end = rest.find(';').expect("a use ends");
            let path: String = rest[..end].split_whitespace().collect();
            let allowed = path.starts_with("protocols::")
                || path.starts_with("wire::")
                || path == "connection::MAX_BYTES_PER_SEND";
            assert!(allowed, "{file} uses crate::{path}");
            source = rest;
        }
    }
}

/// Every Rust source of the crate, by its path below `src/`, in path
/// order.
fn crate_sources() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("a source directory") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                walk(&path, files);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let name = path.strip_prefix(&root).expect("below src");
            let source = std::fs::read_to_string(&path).expect("a readable source");
            (name.to_string_lossy().into_owned(), source)
        })
        .collect()
}

/// Whether `source` can build a `zwp_text_input_v3` `preedit_string`,
/// `delete_surrounding_text`, `commit_string` or `done`: it names the
/// interface's event encoders for anything but `enter` and `leave`, or
/// imports them.
fn builds_v3_transactions(source: &str) -> bool {
    let source: String = source.split_whitespace().collect();
    // Split, so that this file does not name them.
    let encoders = ["zwp_text_input_v3", "::evt"].concat();
    let named = source.match_indices(&encoders).any(|(at, _)| {
        let rest = &source[at + encoders.len()..];
        !(rest.starts_with("::enter(") || rest.starts_with("::leave("))
    });
    let imported = source.match_indices("zwp_text_input_v3::{").any(|(at, _)| {
        let group = &source[at..];
        let group = &group[..group.find('}').unwrap_or(group.len())];
        group
            .split([',', '{'])
            .any(|name| name == "evt" || name.starts_with("evtas") || name.starts_with("evt::"))
    });
    named || imported
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
        keyboard: Vec::new(),
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

/// The old code's `an_answer_that_cannot_repeat_the_preedit_forgets_it`:
/// an answer that cannot repeat the shown preedit sends its `done` alone,
/// which clears it, so the core forgets it, as the old code's
/// `forget_shown_preedit` does. The guest then shows nothing of the
/// composition Exo holds, so a region over it is dropped. No input reaches
/// this, since every preedit a plan shows fit a message once, so the test
/// replaces the shown preedit.
#[test]
fn row_G9c_an_answer_that_cannot_repeat_the_preedit_forgets_it() {
    let mut ime = Ime::enabled();
    ime.report("abcdef", 4);
    let region = |index, length| HostIme::PreeditRegion { index, length };
    ime.ext(region(-1, 1));
    let long = Preedit::new("x".repeat(4_084), (1, 1)).unwrap();
    ime.core.inputs.get_mut(&TI).unwrap().replace_shown(long);
    let out = ime.step(Input::Commit(TI));
    assert_eq!(out.guest, [done(3)]);
    assert_eq!(ime.ext(region(-1, 1)).guest, []);
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

/// INVARIANTS "Every commit that leaves a field enabled gets a `done`":
/// an enable of an enabled field keeps its composition and the content
/// type the host applied, while a commit that disables it starts a new
/// field, whose reconcile ends the composition with the link, so the field
/// enabled again shows none of it. The old code's test of
/// `TextInputState::commit`, against the core.
#[test]
fn only_an_enable_that_changes_the_field_closes_its_composition() {
    let mut ime = Ime::enabled();
    ime.compose("가");
    let select = |ime: &mut Ime| {
        ime.v1(HostIme::PreeditStyling {
            index: 0,
            length: 3,
            style: zwp_text_input_v1::PREEDIT_STYLE_SELECTION,
        });
        ime.v1(HostIme::PreeditCursor(0));
    };
    select(&mut ime);
    let out = ime.enable(TI);
    let kept = [
        surrounding(TI, "", 0),
        cursor_rect(TI, (0, 0, 0, 0)),
        commit_state(TI, 2),
    ];
    assert_eq!(out.host, kept);
    assert_eq!(out.guest, [preedit("가"), done(2)]);
    assert_eq!(out.keys, []);
    // The preedit cursor and styling the host sent before it survive too.
    let out = ime.compose("나");
    assert_eq!(out.guest, [preedit_at("나", 0, 3), done(2)]);

    select(&mut ime);
    ime.step(Input::Disable(TI));
    let out = ime.step(Input::Commit(TI));
    let [barrier] = out.barriers()[..] else {
        panic!("one drain: {out:?}");
    };
    let mut deactivate = vec![
        req(v1(TI), V1::Deactivate { seat: HOST_SEAT }),
        Host::Barrier(barrier),
    ];
    deactivate.extend([
        surrounding(TI, "", 0),
        cursor_rect(TI, (0, 0, 0, 0)),
        commit_state(TI, 3),
    ]);
    assert_eq!(out.host, deactivate);
    assert_eq!(out.guest, []);
    assert_eq!(out.keys, [lease(LeaseOutcome::End)]);

    ime.step(Input::BarrierDone(barrier));
    let out = ime.enable(TI);
    assert_eq!(out.host, activation(TI, 4));
    assert_eq!(out.guest, [done(4)]);
    assert_eq!(ime.compose("다").guest, [preedit("다"), done(4)]);
}

/// INVARIANTS "The host gets each content type once it can apply it, and
/// only then": from every link, with or without a target surface and a
/// live seat, one reconcile reaches the target with only the requests it
/// needs, and the next has nothing left to send. The old code's test of
/// its `reconcile` over `HostApplied`, against the core's.
#[test]
fn reconcile_reaches_every_target_with_only_needed_requests() {
    use super::host::HostContentType;
    use super::state::LinkState;
    use host::{ActivationStep, HostSteps, HostTarget};
    let content_types: Vec<_> = [(1, 2), (3, 4)]
        .into_iter()
        .flat_map(|content_type| {
            [None, Some(false), Some(true)].map(|surrounding_text_support| HostContentType {
                content_type,
                surrounding_text_support,
            })
        })
        .collect();
    let draining = LinkState::Draining {
        barrier: BarrierToken(100),
    };
    let links: Vec<_> = [LinkState::Detached, draining]
        .into_iter()
        .chain(
            [None]
                .into_iter()
                .chain(content_types.iter().copied().map(Some))
                .map(|content_type| LinkState::Attached { content_type }),
        )
        .collect();
    // Apply `steps` to `link` as the host does.
    let apply = |link: LinkState, steps: HostSteps| {
        let mut next = match (steps.activation, link) {
            (Some(ActivationStep::Activate { .. }), LinkState::Detached) => {
                LinkState::Attached { content_type: None }
            }
            (Some(ActivationStep::Deactivate { .. }), LinkState::Attached { .. }) => {
                LinkState::Draining {
                    barrier: BarrierToken(101),
                }
            }
            (Some(ActivationStep::Abandon), LinkState::Attached { .. }) => LinkState::Detached,
            (None, link) => link,
            (step, link) => panic!("{step:?} is no transition from {link:?}"),
        };
        if let Some(content_type) = steps.content_type {
            let LinkState::Attached {
                content_type: applied,
            } = next
            else {
                panic!("{steps:?} sends a content type no input method applies");
            };
            assert_ne!(applied, Some(content_type), "{steps:?} resends {applied:?}");
            next = LinkState::Attached {
                content_type: Some(content_type),
            };
        }
        next
    };
    for surface in [None, Some(HOST_SURFACE)] {
        for seat in [None, Some(HOST_SEAT)] {
            for &content_type in &content_types {
                let target = HostTarget {
                    surface,
                    seat,
                    content_type,
                };
                for &link in &links {
                    let reached = apply(link, host::reconcile(&target, link));
                    let attached = LinkState::Attached {
                        content_type: Some(content_type),
                    };
                    let expected = match (link, surface, seat) {
                        // The barrier's completion reconciles again.
                        (LinkState::Draining { .. }, ..) => link,
                        (_, Some(_), Some(_)) => attached,
                        (LinkState::Attached { .. }, None, Some(_)) => LinkState::Draining {
                            barrier: BarrierToken(101),
                        },
                        (_, None, _) => LinkState::Detached,
                        // A seat that cannot carry an activate keeps the
                        // link as it is, with the content type it can still
                        // take.
                        (LinkState::Detached, Some(_), None) => link,
                        (LinkState::Attached { .. }, Some(_), None) => attached,
                    };
                    assert_eq!(reached, expected, "{target:?} from {link:?}");
                    assert_eq!(
                        host::reconcile(&target, reached),
                        HostSteps::default(),
                        "{target:?} is reached from {link:?} in one reconcile"
                    );
                }
            }
        }
    }
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
    let out = ime.ext(HostIme::Confirm {
        keep_selection: true,
    });
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

/// G7 for each extension version: one request per way of sending a
/// content type (INVARIANTS "`set_input_type` replaces v1
/// `set_content_type`").
#[test]
fn row_G7_a_content_type_takes_one_host_request_per_extension_version() {
    use zwp_text_input_v3::CONTENT_PURPOSE_PIN as PIN;
    let (hint, purpose, input_type, input_mode, input_flags, learning_mode) =
        map_v3_content_type(0, PIN);
    for version in [None, Some(1), Some(7), Some(8), Some(9)] {
        let mut ime = Ime::new();
        let ext = version.map(|version| HostExt {
            id: ext_id(TI),
            version,
            manager: 3,
        });
        ime.step(Input::Create {
            ti: TI,
            seat: SEAT,
            host: HostTi { v1: v1(TI), ext },
            manager: MANAGER,
        });
        ime.step(Input::Enable(TI));
        ime.step(Input::ContentType(TI, (0, PIN)));
        let out = ime.step(Input::Commit(TI));
        let set_content_type = req(v1(TI), V1::SetContentType { hint, purpose });
        let deprecated = req(
            ext_id(TI),
            Ext::DeprecatedSetInputType {
                input_type,
                input_mode,
                input_flags,
                learning_mode,
            },
        );
        let support = req(
            ext_id(TI),
            Ext::SetSurroundingTextSupport {
                support: ext::SURROUNDING_TEXT_SUPPORT_UNSUPPORTED,
            },
        );
        let set_input_type = || {
            req(
                ext_id(TI),
                Ext::SetInputType {
                    input_type,
                    input_mode,
                    input_flags,
                    learning_mode,
                    inline_composition_support: ext::INLINE_COMPOSITION_SUPPORT_SUPPORTED,
                },
            )
        };
        let content_type = match version {
            None | Some(1) => vec![set_content_type],
            Some(7) => vec![set_content_type, deprecated],
            Some(8) => vec![set_input_type()],
            _ => vec![support, set_input_type()],
        };
        // The activation's other requests surround the content type's.
        let mut around = activation(TI, 1);
        let tail = around.split_off(around.len() - 2);
        around.truncate(2);
        around.extend(content_type);
        let expected: Vec<Host> = around.into_iter().chain(tail).collect();
        assert_eq!(out.host, expected, "{version:?}");
    }
}

/// A commit's pending state belongs to its field: a refocus drops it.
#[test]
fn row_G9c_a_refocus_drops_the_pending_enable() {
    let mut ime = Ime::new();
    ime.create(TI);
    ime.step(Input::Enable(TI));
    ime.step(Input::FocusChanged {
        seat: SEAT,
        surface: Some(OTHER),
        keyboard: Vec::new(),
    });
    let out = ime.step(Input::Commit(TI));
    assert_eq!(out.host, [commit_state(TI, 1)]);
    assert_eq!(out.guest, []);
}

// 2.2 Host events.

/// Exo's detach confirms only a composition it tracks below v11 of the
/// extension (E6), and a report it could not match drops that (E4): the
/// guest commits the preedit nothing confirmed.
#[test]
fn row_H2_an_exo_detach_commits_the_preedit_and_activates_again() {
    let mut ime = Ime::enabled();
    ime.compose("가");
    let out = ime.step(Input::HostLeave(TI));
    assert_eq!(out.guest, [no_preedit(), commit("가"), done(1)]);
    assert_eq!(out.keys, [lease(LeaseOutcome::End)]);
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

/// The confirmation Exo's detach sends before its `leave` (E6) commits
/// the preedit, so the `leave` commits it no more.
#[test]
fn row_H2_a_confirmed_composition_is_committed_once() {
    let mut ime = Ime::enabled();
    ime.compose("가");
    let out = ime.ext(HostIme::Confirm {
        keep_selection: true,
    });
    assert_eq!(out.guest, [no_preedit(), commit("가"), done(1)]);
    let out = ime.step(Input::HostLeave(TI));
    assert!(out.guest.is_empty() && out.keys.is_empty());
    // Below v11, the confirmation is a commit of the composition.
    let mut ime = Ime::enabled();
    ime.compose("가");
    ime.v1(HostIme::Commit {
        serial: 0,
        text: "가".into(),
    });
    let out = ime.step(Input::HostLeave(TI));
    assert!(out.guest.is_empty() && out.keys.is_empty());
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

/// An empty commit with no preedit shown edits nothing, so an absolute
/// delete after it still applies.
#[test]
fn row_H8_an_empty_commit_is_no_edit() {
    let mut ime = Ime::enabled();
    ime.report("ab", 2);
    let out = ime.v1(HostIme::Commit {
        serial: 0,
        text: String::new(),
    });
    assert_eq!(out.guest, [done(2)]);
    let out = ime.v1(HostIme::Delete {
        index: 0,
        length: 2,
    });
    assert_eq!(out.guest, [delete(2, 0), done(2)]);
}

/// An empty commit over a shown preedit edits nothing either (the old
/// code's `edited = !text.is_empty()`), so an absolute delete after it
/// still applies.
#[test]
fn row_H8_an_empty_commit_over_a_preedit_is_no_edit() {
    let mut ime = Ime::enabled();
    ime.report("ab", 2);
    ime.compose("가");
    ime.v1(HostIme::Commit {
        serial: 0,
        text: String::new(),
    });
    let out = ime.v1(HostIme::Delete {
        index: 0,
        length: 2,
    });
    assert!(!out.guest.is_empty());
}

/// Exo's delete is absolute, in its text, which the guest's is while in
/// step. A negative index, which only the v1 XML's cursor-relative
/// reading could mean, is not Exo's: the mirror diverges, and the delete
/// is dropped.
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
    // Exo's "a|d", which the guest has yet to report.
    let absolute = HostIme::Delete {
        index: 0,
        length: 1,
    };
    assert_eq!(ime.v1(absolute).guest, [delete(1, 0), done(2)]);
    ime.report("d", 0);
    let relative = HostIme::Delete {
        index: -1,
        length: 1,
    };
    assert_eq!(ime.v1(relative).guest, []);
}

#[test]
fn row_H9_a_delete_that_cannot_be_measured_is_dropped() {
    type Case = (&'static str, Option<(&'static str, i32, i32)>, i32, u32);
    let cases: [Case; 5] = [
        ("no surrounding text", None, 0, 1),
        ("a selection", Some(("abcd", 2, 0)), -1, 2),
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
        assert_eq!(out.guest, [], "{case}");
        // Exo edited its text, unless the range was empty.
        let ended = (length > 0).then_some(lease(LeaseOutcome::End));
        assert_eq!(out.keys, Vec::from_iter(ended), "{case}");
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
    // The assembly places its cursor, and the edit the guest has yet to
    // report does not stop it. The guest no longer has the "가" the first
    // region took.
    ime.v1(HostIme::PreeditCursor(0));
    let out = ime.ext(region(-3, 4));
    assert_eq!(
        out.guest,
        [no_preedit(), delete(0, 1), preedit_at("가b", 0, 0), done(2)]
    );
}

/// With no assembly, the preedit cursor stays where the text cursor was,
/// inside the region; and an empty region edits nothing.
#[test]
fn row_X1_a_region_keeps_the_cursor_and_an_empty_one_edits_nothing() {
    let mut ime = Ime::enabled();
    ime.report("ab", 1);
    let region = |index, length| HostIme::PreeditRegion { index, length };
    let out = ime.ext(region(-1, 2));
    assert_eq!(out.guest, [delete(1, 1), preedit_at("ab", 1, 1), done(2)]);

    let mut ime = Ime::enabled();
    ime.report("ab", 2);
    assert_eq!(ime.ext(region(0, 0)).guest, [done(2)]);
    let out = ime.v1(HostIme::Delete {
        index: 0,
        length: 2,
    });
    assert_eq!(out.guest, [delete(2, 0), done(2)]);
}

/// The old code's
/// `a_region_after_a_host_preedit_deletes_only_what_the_guest_has`: a host
/// preedit replaces the region's preedit, but the guest still lacks the
/// bytes the region took. Exo holds the new preedit in place of those
/// bytes, so the region takes it.
#[test]
fn row_X1_a_region_after_a_host_preedit_deletes_only_what_the_guest_has() {
    let mut ime = Ime::enabled();
    ime.report("abcdef", 4);
    let region = |index, length| HostIme::PreeditRegion { index, length };
    let out = ime.ext(region(-1, 1));
    assert_eq!(out.guest, [delete(1, 0), preedit_at("d", 1, 1), done(2)]);

    assert_eq!(ime.compose("x").guest, [preedit("x"), done(2)]);

    // The guest's text is "abc|ef", and Exo's "abcx|ef": of "cx", only
    // "c" is in the guest's.
    let out = ime.ext(region(-2, 2));
    let expected = [no_preedit(), delete(1, 0), preedit_at("cx", 2, 2), done(2)];
    assert_eq!(out.guest, expected);
}

/// The old code's
/// `a_region_over_a_host_preedit_is_measured_from_exos_cursor`: Exo holds
/// the preedit the guest shows as text at the guest's cursor, with its own
/// cursor where the preedit's was, and measures a region from there. With
/// "a1b|" and "b" shown, region -2+2 is "bb", not "1b".
#[test]
fn row_X1_a_region_over_a_host_preedit_is_measured_from_exos_cursor() {
    let mut ime = Ime::enabled();
    ime.report("a1b", 3);
    ime.v1(HostIme::PreeditCursor(1));
    let out = ime.compose("b");
    assert_eq!(out.guest, [preedit_at("b", 1, 1), done(2)]);

    let out = ime.ext(HostIme::PreeditRegion {
        index: -2,
        length: 2,
    });
    let expected = [no_preedit(), delete(1, 0), preedit_at("bb", 2, 2), done(2)];
    assert_eq!(out.guest, expected);
}

/// The old code's `a_region_that_does_not_cover_a_host_preedit_is_dropped`:
/// a non-empty region that does not cover the host preedit the guest
/// shows is dropped, since Exo would leave the rest of that preedit as
/// text. One that covers it on both sides deletes the bytes beyond it on
/// each.
#[test]
fn row_X1_a_region_that_does_not_cover_a_host_preedit_is_dropped() {
    let region = |index, length| HostIme::PreeditRegion { index, length };
    // Exo holds "ax|bc".
    let composing = || {
        let mut ime = Ime::enabled();
        ime.report("abc", 1);
        ime.v1(HostIme::PreeditCursor(1));
        let out = ime.compose("x");
        assert_eq!(out.guest, [preedit_at("x", 1, 1), done(2)]);
        ime
    };
    for (index, length) in [(-2, 1), (0, 1)] {
        let out = composing().ext(region(index, length));
        let dropped = out.guest.is_empty() && out.keys == [lease(LeaseOutcome::End)];
        assert!(dropped, "region {index}+{length}");
    }
    let out = composing().ext(region(-2, 3));
    let expected = [no_preedit(), delete(1, 1), preedit_at("axb", 2, 2), done(2)];
    assert_eq!(out.guest, expected);
}

/// The old code's `an_empty_region_over_a_held_preedit_commits_it`: an
/// empty region at Exo's cursor ends Exo's composition and leaves the
/// preedit it held as text, so the guest commits it, as at a confirmation.
#[test]
fn row_X1_an_empty_region_over_a_held_preedit_commits_it() {
    let mut ime = Ime::enabled();
    ime.report("abcdef", 4);
    let region = |index, length| HostIme::PreeditRegion { index, length };
    assert_eq!(ime.compose("x").guest, [preedit("x"), done(2)]);
    let out = ime.ext(region(0, 0));
    assert_eq!(out.guest, [no_preedit(), commit("x"), done(2)]);
    assert_eq!(out.keys, [lease(LeaseOutcome::End)]);
}

/// The old code's `a_region_equal_to_the_shown_one_sends_only_the_preedit`.
#[test]
fn row_X1_a_region_equal_to_the_shown_one_sends_only_the_preedit() {
    let mut ime = Ime::enabled();
    ime.report("abcdef", 4);
    let region = |index, length| HostIme::PreeditRegion { index, length };
    let out = ime.ext(region(-1, 1));
    assert_eq!(out.guest, [delete(1, 0), preedit_at("d", 1, 1), done(2)]);

    let out = ime.ext(region(-1, 1));
    assert_eq!(out.guest, [preedit_at("d", 1, 1), done(2)]);
}

/// The old code's `a_region_that_does_not_cover_the_shown_one_is_dropped`:
/// one short of the shown preedit's bytes on either side.
#[test]
fn row_X1_a_region_that_does_not_cover_the_shown_one_is_dropped() {
    let region = |index, length| HostIme::PreeditRegion { index, length };
    for (index, length) in [(-2, 2), (0, 2), (-1, 1)] {
        let mut ime = Ime::enabled();
        ime.report("abcdef", 3);
        // "c" before the cursor and "d" after it.
        let out = ime.ext(region(-1, 2));
        assert_eq!(out.guest, [delete(1, 1), preedit_at("cd", 1, 1), done(2)]);
        let out = ime.ext(region(index, length));
        let dropped = out.guest.is_empty() && out.keys == [lease(LeaseOutcome::End)];
        assert!(dropped, "region {index}+{length}");
    }
}

/// The old code's `a_region_over_a_region_deletes_the_rest_on_both_sides`.
#[test]
fn row_X1_a_region_over_a_region_deletes_the_rest_on_both_sides() {
    let mut ime = Ime::enabled();
    ime.report("abcdef", 3);
    let region = |index, length| HostIme::PreeditRegion { index, length };
    let out = ime.ext(region(-1, 2));
    assert_eq!(out.guest, [delete(1, 1), preedit_at("cd", 1, 1), done(2)]);

    let out = ime.ext(region(-2, 4));
    let expected = [
        no_preedit(),
        delete(1, 1),
        preedit_at("bcde", 2, 2),
        done(2),
    ];
    assert_eq!(out.guest, expected);
}

#[test]
fn row_X1prime_a_region_the_text_cannot_hold_is_dropped() {
    let mut ime = Ime::enabled();
    let region = |index, length| HostIme::PreeditRegion { index, length };
    ime.v1(HostIme::PreeditCursor(0));
    let out = ime.ext(region(-1, 1));
    assert_eq!(out.guest, []);
    assert_eq!(ime.compose("나").guest, [preedit("나"), done(1)]);
    ime.report("ab", 1);
    assert_eq!(ime.ext(region(1, 1)).guest, []);
}

#[test]
fn row_X2_a_confirmation_commits_the_shown_preedit() {
    let mut ime = Ime::enabled();
    ime.compose("가");
    let out = ime.ext(HostIme::Confirm {
        keep_selection: true,
    });
    assert_eq!(out.guest, [no_preedit(), commit("가"), done(1)]);
    assert_eq!(out.keys, [lease(LeaseOutcome::End)]);
}

#[test]
fn row_X2prime_an_empty_confirmation_recovers_the_held_key() {
    let mut ime = Ime::enabled();
    let out = ime.ext(HostIme::Confirm {
        keep_selection: true,
    });
    assert_eq!(out.guest, []);
    assert_eq!(out.keys, [Key::Recover(TI)]);
}

/// X2': the keyboard concern recovers the held key at the host's next
/// event, after guest requests the core has handled since the
/// confirmation. The transaction that follows the key is the guest's state
/// under the serial of then; with the gate closed there is none.
#[test]
fn row_X2prime_a_recovered_key_is_closed_under_the_serial_of_its_recovery() {
    let mut ime = Ime::enabled();
    ime.ext(HostIme::Confirm {
        keep_selection: true,
    });
    let done_now = |ime: &mut Ime| {
        let done = ime.core.recovery_done(TI);
        done.map(|done| guest_events(&done))
    };
    assert_eq!(done_now(&mut ime), Some(vec![done(1)]));
    ime.report("ab", 1);
    assert_eq!(done_now(&mut ime), Some(vec![done(2)]));
    ime.step(Input::Disable(TI));
    ime.step(Input::Commit(TI));
    assert_eq!(done_now(&mut ime), None);
    assert!(ime.core.recovery_done(TiId(TI.0 + 1)).is_none());
}

/// B3, as the old handlers did it: a commit or a delete that reaches no
/// guest, and an empty confirmation, leave the preedit cursor and styling
/// to the next preedit; a confirmation of a preedit takes them.
#[test]
fn row_H8_H9_X2_only_an_edit_that_reaches_the_guest_takes_the_assembly() {
    type Case = (&'static str, bool, fn() -> HostIme, bool);
    let cases: [Case; 4] = [
        (
            "a commit that does not fit",
            false,
            || HostIme::Commit {
                serial: 0,
                text: "x".repeat(4_090),
            },
            false,
        ),
        (
            "a delete with no surrounding text",
            false,
            || HostIme::Delete {
                index: -1,
                length: 1,
            },
            false,
        ),
        (
            "an empty confirmation",
            false,
            || HostIme::Confirm {
                keep_selection: true,
            },
            false,
        ),
        (
            "a confirmation",
            true,
            || HostIme::Confirm {
                keep_selection: true,
            },
            true,
        ),
    ];
    for (case, composing, event, takes) in cases {
        let mut ime = Ime::enabled();
        if composing {
            ime.compose("가");
        }
        ime.v1(HostIme::PreeditStyling {
            index: 0,
            length: 3,
            style: zwp_text_input_v1::PREEDIT_STYLE_SELECTION,
        });
        ime.v1(HostIme::PreeditCursor(0));
        let out = match event() {
            HostIme::Confirm { .. } => ime.ext(HostIme::Confirm {
                keep_selection: true,
            }),
            event => ime.v1(event),
        };
        assert_eq!(out.guest.is_empty(), !takes, "{case}");
        let next = match takes {
            true => preedit("나"),
            false => preedit_at("나", 0, 3),
        };
        assert_eq!(ime.compose("나").guest, [next, done(1)], "{case}");
    }
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

/// A drain ends only with its own barrier, not with a press's fence.
#[test]
fn row_W1_only_its_own_barrier_ends_a_drain() {
    let mut ime = Ime::enabled();
    ime.compose("가");
    let fence = ime.press(SURFACE).barriers()[0];
    ime.step(Input::Disable(TI));
    let drain = ime.step(Input::Commit(TI)).barriers()[0];
    ime.step(Input::BarrierDone(fence));
    let out = ime.enable(TI);
    assert_eq!(
        out.host,
        [
            surrounding(TI, "", 0),
            cursor_rect(TI, (0, 0, 0, 0)),
            commit_state(TI, 3),
        ]
    );
    let out = ime.step(Input::BarrierDone(drain));
    assert_eq!(
        out.host[0],
        req(
            v1(TI),
            V1::Activate {
                seat: HOST_SEAT,
                surface: HOST_SURFACE,
            },
        )
    );
}

// 2.3 Cross-concern inputs and reconcile outcomes.

#[test]
fn row_F1_a_focus_change_resets_the_field() {
    let mut ime = Ime::enabled();
    let out = ime.step(Input::FocusChanged {
        seat: SEAT,
        surface: Some(OTHER),
        keyboard: Vec::new(),
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
        keyboard: Vec::new(),
    });
    assert_eq!(out.guest, [v3(TI, V3::Leave { surface: OTHER.0 })]);
    let out = ime.step(Input::FocusChanged {
        seat: SeatId(6),
        surface: Some(SURFACE),
        keyboard: Vec::new(),
    });
    assert_eq!(out.guest, []);
}

/// C5: the field the focus leaves commits the preedit the guest shows
/// before its `leave`, as mutter and ibus do at a focus-out. Exo's detach
/// confirms the composition too (E6), but after the keyboard's `leave`
/// (E12), while the link drains, so the proxy drops it.
#[test]
fn row_F1_a_focus_change_commits_the_shown_preedit_before_leave() {
    let mut ime = Ime::enabled();
    ime.compose("가");
    let out = ime.step(Input::FocusChanged {
        seat: SEAT,
        surface: Some(OTHER),
        keyboard: Vec::new(),
    });
    assert_eq!(
        out.guest,
        [
            no_preedit(),
            commit("가"),
            done(1),
            v3(TI, V3::Leave { surface: SURFACE.0 }),
            v3(TI, V3::Enter { surface: OTHER.0 }),
        ]
    );
    assert_eq!(out.keys, [lease(LeaseOutcome::End)]);
    let confirm = ime.ext(HostIme::Confirm {
        keep_selection: true,
    });
    assert!(confirm.guest.is_empty() && confirm.keys.is_empty());
    // The commit ended the composition: a focus that comes back shows
    // nothing.
    let out = ime.step(Input::FocusChanged {
        seat: SEAT,
        surface: Some(SURFACE),
        keyboard: Vec::new(),
    });
    assert_eq!(
        out.guest,
        [
            v3(TI, V3::Leave { surface: OTHER.0 }),
            v3(TI, V3::Enter { surface: SURFACE.0 }),
        ]
    );
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

    // A preedit the guest shows is not committed into the destroyed
    // surface, unlike at F1.
    let mut ime = Ime::enabled();
    ime.compose("가");
    ime.world.surfaces.remove(&SURFACE);
    ime.world.focus.remove(&SEAT);
    let out = ime.step(Input::SurfaceDestroyed(SURFACE));
    assert_eq!(out.guest, [v3(TI, V3::Leave { surface: SURFACE.0 })]);
    assert_eq!(out.keys, []);
    // The seat's focus change that follows finds the field reset.
    let out = ime.step(Input::FocusChanged {
        seat: SEAT,
        surface: None,
        keyboard: Vec::new(),
    });
    assert!(out.guest.is_empty() && out.host.is_empty() && out.keys.is_empty());

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
    assert_eq!(
        out.host,
        [req(v1(TI), V1::Reset {}), Host::GluedBarrier(fence)]
    );
    assert_eq!(out.keys, [lease(LeaseOutcome::End)]);
    assert!(!out.forward);
    // The press took the assembly.
    ime.step(Input::BarrierDone(fence));
    assert_eq!(ime.compose("나").guest, [preedit("나"), done(1)]);
}

/// A press whose commit does not fit is forwarded, and leaves the preedit
/// cursor and styling to the next preedit, as the old code's
/// `end_composition_on_press` did: it took them only once the commit
/// reached the guest.
#[test]
fn row_F4_a_press_whose_commit_does_not_fit_keeps_the_assembly() {
    let mut ime = Ime::enabled();
    ime.compose("가");
    let long = Preedit::new("x".repeat(4_090), (0, 0)).unwrap();
    ime.core.inputs.get_mut(&TI).unwrap().replace_shown(long);
    ime.v1(HostIme::PreeditStyling {
        index: 0,
        length: 3,
        style: zwp_text_input_v1::PREEDIT_STYLE_SELECTION,
    });
    ime.v1(HostIme::PreeditCursor(0));
    let out = ime.press(SURFACE);
    assert!(out.forward && out.guest.is_empty() && out.host.is_empty());
    assert_eq!(out.keys, []);
    assert_eq!(ime.compose("나").guest, [preedit_at("나", 0, 3), done(1)]);
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

/// Events in flight at a press are dropped. A preedit and a region take
/// the preedit cursor and styling before them; a commit, a delete and a
/// confirmation leave them, as the old handlers did (B3).
#[test]
fn row_F4_events_in_flight_at_a_press_are_dropped() {
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
            event: HostIme::Confirm {
                keep_selection: true,
            },
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
        let next = match event {
            "preedit" | "region" => preedit("나"),
            _ => preedit_at("나", 0, 3),
        };
        assert_eq!(ime.compose("나").guest, [next, done(2)], "{event}");
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
        keyboard: Vec::new(),
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

// The Exo mirror (design exo-mirror 3): from v11 of the extension, H9 and
// X1 are measured in the Exo mirror while the guest is in step with it, and
// dropped otherwise (design single-exo-model 3.4); below v11 they are
// dropped.

impl Ime {
    /// [`Self::enabled`] with a host whose text-input extension is
    /// `version`, if any.
    fn enabled_at(version: Option<u32>) -> Self {
        let mut ime = Self::new();
        let ext = version.map(|version| HostExt {
            id: ext_id(TI),
            version,
            manager: 3,
        });
        let host = HostTi { v1: v1(TI), ext };
        ime.step(Input::Create {
            ti: TI,
            seat: SEAT,
            host,
            manager: MANAGER,
        });
        ime.enable(TI);
        ime
    }

    /// [`TI`]'s Exo mirror.
    fn mirror(&self) -> &mirror::ExoMirror {
        let exo = self.core.get(TI).expect("a text input").exo();
        exo.mirror().expect("a mirror")
    }
}

#[test]
fn row_G1_a_text_input_has_a_mirror_from_v11() {
    let mirrored = |version| {
        let ime = Ime::enabled_at(version);
        let exo = ime.core.get(TI).expect("a text input").exo();
        exo.mirror().is_some()
    };
    assert!(mirrored(Some(11)));
    assert!(!mirrored(Some(10)));
    assert!(!mirrored(None));
}

/// Below v11 of the extension the text input has no mirror: Exo's deletes
/// and regions are dropped, which warns once, and every report goes out;
/// typing does not need the mirror.
#[test]
fn row_H9_X1_below_v11_deletes_and_regions_are_dropped() {
    let mut ime = Ime::enabled_at(Some(10));
    let out = ime.report("ab", 2);
    assert_eq!(out.host[0], surrounding(TI, "ab", 2));
    let warned = |ime: &Ime| ime.core.get(TI).expect("a text input").warned_for_test().1;
    assert!(!warned(&ime));
    assert_eq!(ime.delete(1, 1).guest, []);
    assert!(warned(&ime));
    assert_eq!(ime.region(-1, 1).guest, []);
    assert_eq!(ime.compose("가").guest, [preedit("가"), done(2)]);
    let out = ime.report("ab", 2);
    assert_eq!(out.host, [surrounding(TI, "ab", 2), commit_state(TI, 3)]);
    let out = ime.ext(HostIme::Confirm {
        keep_selection: true,
    });
    assert_eq!(out.guest, [no_preedit(), commit("가"), done(3)]);
    assert_eq!(ime.host_commit("나").guest, [commit("나"), done(3)]);
}

/// The mirror takes a report at its barrier, which reaches Exo with it.
#[test]
fn row_G9c_a_forwarded_report_is_glued_to_a_barrier() {
    let mut ime = Ime::enabled();
    ime.step(Input::SurroundingText(TI, "ab".into(), 2, 2));
    let out = ime.step_unanswered(Input::Commit(TI));
    let [barrier] = out.glued()[..] else {
        panic!("one glued barrier in {out:?}");
    };
    // The content type and `commit_state` follow.
    assert_eq!(
        out.host[..2],
        [surrounding(TI, "ab", 2), Host::GluedBarrier(barrier)]
    );
    let text = |ime: &Ime| ime.mirror().predicted().text.clone();
    assert_eq!(text(&ime), "");
    ime.step(Input::BarrierDone(barrier));
    assert_eq!(text(&ime), "ab");
}

/// A report that does not fit a message sends nothing, a barrier
/// included (G9b). One that fits but may reach Exo in two reads leaves
/// the mirror exact, unless an event of Exo's comes while it is on its
/// way, which Exo may have processed before the report or after it: the
/// mirror is then unsure, and the guest out of step.
#[test]
fn row_G9b_a_report_that_does_not_fit_queues_no_barrier() {
    let mut ime = Ime::enabled();
    ime.report("", 0);
    let long = "x".repeat(4_084);
    ime.step(Input::SurroundingText(TI, long.as_str().into(), 0, 0));
    assert_eq!(ime.step_unanswered(Input::Commit(TI)).host, []);
    assert!(ime.in_step());

    let health = |ime: &Ime| ime.mirror().health();
    let long = "x".repeat(4_000);
    let barrier = ime.report_unanswered(&long, 0);
    ime.step(Input::BarrierDone(barrier));
    assert_eq!(health(&ime), mirror::Health::Exact);
    assert!(ime.in_step());

    let barrier = ime.report_unanswered(&long, 1);
    ime.host_commit("y");
    ime.step(Input::BarrierDone(barrier));
    assert_eq!(health(&ime), mirror::Health::Unknown);
    assert!(!ime.in_step());
}

/// An event of Exo's that does not agree with the mirror is a misreading
/// of Chromium's, which warns once per text input. The mirror no longer
/// knows Exo's text, nor which state Exo's history holds in its place, so
/// it measures nothing until a press's `reset` empties that history and a
/// report resets Exo.
#[test]
fn row_X1_a_mirror_that_diverges_warns_once() {
    let mut ime = Ime::enabled();
    ime.report("ab", 2);
    let exo = |ime: &Ime| {
        let exo = ime.core.get(TI).expect("a text input").exo();
        (exo.warned(), ime.mirror().health())
    };
    assert_eq!(exo(&ime), (false, mirror::Health::Exact));
    // A region before the text's start.
    ime.region(-3, 1);
    assert_eq!(exo(&ime), (true, mirror::Health::Unknown));
    assert!(!ime.in_step());
    ime.report("abc", 3);
    assert_eq!(exo(&ime), (true, mirror::Health::Unknown));
    ime.compose("가");
    let fence = ime.press(SURFACE).barriers()[0];
    ime.step(Input::BarrierDone(fence));
    ime.report("abc가", 6);
    assert_eq!(exo(&ime), (true, mirror::Health::Exact));
}

/// A report without the preedit the guest shows names no state of Exo's,
/// which holds it inline, so Exo would only drop its composition (E4):
/// with the composition at the guest's cursor it names one, and the core
/// withholds it, with no barrier (design exo-mirror 5). The rest of the
/// commit goes out, and the guest is in step with Exo's composition.
#[test]
fn row_G9c_a_report_without_the_shown_preedit_is_withheld() {
    let mut ime = Ime::enabled();
    ime.report("ab", 2);
    ime.region(-1, 1);
    ime.step(Input::SurroundingText(TI, "a".into(), 1, 1));
    let out = ime.step_unanswered(Input::Commit(TI));
    assert_eq!(out.host, [commit_state(TI, 3)]);
    assert_eq!(out.guest, [preedit_at("b", 1, 1), done(3)]);
    assert_eq!(ime.mirror().predicted().composition, 1..2);
    assert!(ime.in_step());
    // So Exo's region over its composition is measured.
    let out = ime.region(-2, 2);
    let expected = [no_preedit(), delete(1, 0), preedit_at("ab", 2, 2), done(3)];
    assert_eq!(out.guest, expected);
}

/// A report on its way to Exo when the guest's next is withheld can only
/// take the guest out of step by resetting Exo: matching an older state,
/// here one with the composition inline, changes nothing of Exo's.
#[test]
fn row_G9c_an_older_report_matched_after_a_withheld_one_keeps_it_in_step() {
    let mut ime = Ime::enabled();
    ime.report("ab", 2);
    ime.compose("가");
    let barrier = ime.report_unanswered("ab가", 5);
    let out = ime.report("ab", 2);
    assert_eq!(out.host, [commit_state(TI, 4)]);
    assert!(ime.in_step());
    ime.step(Input::BarrierDone(barrier));
    assert!(ime.in_step());
}

/// A report that names a state of Exo's goes out: Exo matches it and
/// keeps its composition. That one would also be the composition's state
/// without it does not matter.
#[test]
fn row_G9c_an_exact_report_goes_out_unchanged() {
    let mut ime = Ime::enabled();
    ime.report("ab", 2);
    ime.compose("가");
    let out = ime.report("ab", 2);
    assert_eq!(out.host, [surrounding(TI, "ab", 2), commit_state(TI, 3)]);
    assert_eq!(ime.mirror().predicted().composition, 2..5);
}

/// A report that names no state, even with the composition the guest
/// shows inline, goes out, and Exo resets to the guest's text; so does
/// every report below v11 of the extension, which has no mirror.
#[test]
fn row_G9c_an_unmatched_report_goes_out_unchanged() {
    let mut ime = Ime::enabled();
    ime.report("ab", 2);
    ime.region(-1, 1);
    let out = ime.report("a", 0);
    assert_eq!(out.host, [surrounding(TI, "a", 0), commit_state(TI, 3)]);
    assert!(ime.mirror().predicted().composition.is_empty());

    let mut ime = Ime::enabled_at(Some(10));
    ime.report("ab", 2);
    ime.compose("b");
    let out = ime.report("a", 1);
    assert_eq!(out.host, [surrounding(TI, "a", 1), commit_state(TI, 3)]);
}

#[test]
fn row_X1_a_region_after_an_unreported_commit_is_measured_in_exos_text() {
    let mut ime = Ime::enabled();
    ime.report("ab", 2);
    ime.host_commit("c");
    // Exo's "abc|": "bc" around the guest's cursor.
    let out = ime.region(-2, 2);
    assert_eq!(out.guest, [delete(2, 0), preedit_at("bc", 2, 2), done(2)]);
}

#[test]
fn row_H9_a_delete_after_an_unreported_edit_lands_in_exos_text() {
    let mut ime = Ime::enabled();
    ime.report("abc", 3);
    ime.host_commit("X");
    // Exo deletes "cX" of its "abcX|", which the guest has before its
    // cursor.
    assert_eq!(ime.delete(2, 2).guest, [delete(2, 0), done(2)]);
}

/// A report Exo cannot match drops its composition (E4): the preedit the
/// guest shows is no one's text, and a region is measured around the
/// guest's cursor.
#[test]
fn row_X1_a_region_after_an_e4_drop_holds_no_preedit() {
    let mut ime = Ime::enabled();
    ime.report("ab", 2);
    ime.compose("가");
    ime.report("ab", 1);
    let out = ime.region(-1, 1);
    let expected = [no_preedit(), delete(1, 0), preedit_at("a", 1, 1), done(3)];
    assert_eq!(out.guest, expected);
}

/// Exo made the region before it got the guest's report, which the
/// region's state names: the guest caught up with Exo.
#[test]
fn row_X1_a_region_made_before_a_report_is_measured_before_it() {
    let mut ime = Ime::enabled();
    ime.report("ab", 2);
    ime.host_commit("c");
    let barrier = ime.report_unanswered("abc", 3);
    let out = ime.region(-1, 1);
    assert_eq!(out.guest, [delete(1, 0), preedit_at("c", 1, 1), done(3)]);
    ime.step(Input::BarrierDone(barrier));
    assert!(ime.in_step());
}

/// The last of `steps` is Exo's delete or region, which the core cannot
/// measure in the mirror: it drops it, with no guest transaction, and the
/// guest is out of step with Exo until its next report (design
/// single-exo-model 3.4).
fn drops(steps: impl Fn(&mut Ime) -> Out) {
    let mut ime = Ime::enabled();
    let out = steps(&mut ime);
    assert!(!ime.in_step(), "the guest is in step");
    assert_eq!(out.guest, [], "a guest transaction");
    // Exo edited all the same.
    assert_eq!(out.keys, [lease(LeaseOutcome::End)]);
}

/// A report on its way to Exo that names no state of Exo's holds an
/// edit of the guest's that Exo made the region without.
#[test]
fn row_X1_a_region_crossing_a_guest_edit_drops() {
    drops(|ime| {
        ime.report("ab", 2);
        ime.report_unanswered("abX", 3);
        ime.region(-1, 1)
    });
}

/// An event the mirror finds Exo could not have sent leaves it diverged
/// until it heals.
#[test]
fn row_X1_a_mirror_that_is_not_exact_drops() {
    drops(|ime| {
        ime.report("ab", 2);
        ime.region(-5, 1);
        ime.region(-1, 1)
    });
}

/// A v11 confirmation keeps Exo's selection, inside the composition, while
/// the guest's cursor goes after the preedit it commits.
#[test]
fn row_X1_after_a_confirm_inside_the_composition_drops() {
    drops(|ime| {
        ime.report("ab", 2);
        ime.v1(HostIme::PreeditCursor(3));
        ime.compose("가나");
        let confirmed = ime.ext(HostIme::Confirm {
            keep_selection: true,
        });
        assert_eq!(confirmed.guest, [no_preedit(), commit("가나"), done(2)]);
        ime.region(-2, 2)
    });
}

/// v11's `SetEditableSelectionRange` moves Exo's selection and keeps its
/// composition, while the guest commits it.
#[test]
fn row_H9_after_set_editable_selection_range_drops() {
    drops(|ime| {
        ime.report("ab", 2);
        ime.compose("가");
        ime.v1(HostIme::CursorPosition {
            index: 0,
            anchor: 0,
        });
        let out = ime.host_commit("가");
        assert_eq!(out.guest, [no_preedit(), commit("가"), done(2)]);
        ime.delete(1, 1)
    });
}

/// An empty region ends Exo's composition as text, which the guest
/// commits, but leaves Exo's cursor inside it.
#[test]
fn row_H9_after_an_empty_region_with_exos_cursor_inside_drops() {
    drops(|ime| {
        ime.report("ab", 2);
        ime.v1(HostIme::PreeditCursor(3));
        ime.compose("가나");
        let out = ime.region(0, 0);
        assert_eq!(out.guest, [no_preedit(), commit("가나"), done(2)]);
        ime.delete(2, 3)
    });
}

/// A region dropped for not covering the preedit Exo holds leaves Exo
/// composing what the guest does not show.
#[test]
fn row_H9_after_a_dropped_region_drops() {
    drops(|ime| {
        ime.report("abc", 3);
        ime.compose("가");
        assert_eq!(ime.region(-4, 1).guest, []);
        ime.ext(HostIme::Confirm {
            keep_selection: true,
        });
        ime.delete(3, 3)
    });
}

/// Chromium applies a delete before it clears the preedit it shows, and
/// drops it (C3).
#[test]
fn row_H9_after_a_delete_over_a_shown_preedit_drops() {
    drops(|ime| {
        ime.report("ab", 2);
        ime.compose("가");
        // Exo drops its composition (E4); the guest still shows it.
        ime.report("ab", 1);
        let out = ime.delete(0, 1);
        assert_eq!(out.guest, [no_preedit(), delete(1, 0), done(3)]);
        ime.delete(0, 1)
    });
}

/// A field that states no surrounding text, as a terminal's, gets the
/// proxy's empty stand-in, which tells nothing of the guest's text.
#[test]
fn row_H9_a_field_that_states_no_text_drops() {
    drops(|ime| {
        ime.host_commit("X");
        ime.delete(0, 1)
    });
}

/// A key the guest gets may edit its text where Exo does not see, and so
/// may a press: Exo's delete of "cX" after a commit the guest has yet to
/// report, which the mirror would measure, waits for the report.
#[test]
fn row_H9_after_a_forwarded_key_drops() {
    for press in [false, true] {
        drops(|ime| {
            ime.report("abc", 3);
            ime.host_commit("X");
            match press {
                false => ime.step(Input::GuestKey(SEAT)),
                true => ime.press(SURFACE),
            };
            ime.delete(2, 2)
        });
    }
}

/// Exo ends its composition at an empty region, and leaves it as text,
/// whether or not the mirror can measure the region. So an empty region
/// the core drops, below v11 or after a key the guest has not stated,
/// commits the preedit the guest shows: if Exo had kept it as well, the
/// text doubles, where sending nothing would lose it to the next preedit,
/// which replaces it (design single-exo-model 3.4).
#[test]
fn row_X1_an_unmeasured_empty_region_commits_the_shown_preedit() {
    for version in [Some(10), Some(11)] {
        let mut ime = Ime::enabled_at(version);
        ime.report("ab", 2);
        ime.compose("가");
        ime.step(Input::GuestKey(SEAT));
        let out = ime.region(0, 0);
        assert_eq!(
            out.guest,
            [no_preedit(), commit("가"), done(2)],
            "{version:?}"
        );
        assert_eq!(out.keys, [lease(LeaseOutcome::End)], "{version:?}");
        assert_eq!(ime.compose("나").guest, [preedit("나"), done(2)]);
    }
}

/// Exo edited its text at a delete or a region the core drops, so either
/// ends the Backspace repeat leased to the text input, as one that reaches
/// the guest does; an empty delete edits nothing, and leaves it.
#[test]
fn row_H9_X1_a_dropped_delete_or_region_ends_the_lease() {
    let mut ime = Ime::enabled();
    ime.report("abc", 3);
    ime.step(Input::GuestKey(SEAT));
    let out = ime.region(-1, 1);
    assert_eq!(out.guest, []);
    assert_eq!(out.keys, [lease(LeaseOutcome::End)]);
    let out = ime.delete(0, 1);
    assert_eq!(out.guest, []);
    assert_eq!(out.keys, [lease(LeaseOutcome::End)]);
    let out = ime.delete(1, 0);
    assert_eq!((out.guest, out.keys), (vec![], vec![]));
}

/// An event that heals a mirror that was not exact, as a delete over a
/// vague selection does, is dropped as the mirror was unsure, not as the
/// guest out of step.
#[test]
fn row_H9_a_delete_that_heals_an_unsure_mirror_drops_as_unsure() {
    let mut ime = Ime::enabled();
    ime.report("가나다", 9);
    ime.v1(HostIme::PreeditStyling {
        index: 0,
        length: 3,
        style: zwp_text_input_v1::PREEDIT_STYLE_HIGHLIGHT,
    });
    ime.v1(HostIme::PreeditCursor(3));
    ime.compose("漢字");
    ime.ext(HostIme::Confirm {
        keep_selection: true,
    });
    assert_eq!(ime.mirror().health(), mirror::Health::Vague);
    take_drops_for_test();
    assert_eq!(ime.delete(9, 6).guest, []);
    assert_eq!(ime.mirror().health(), mirror::Health::Exact);
    let unsure = Drops {
        unsure: 1,
        ..Drops::default()
    };
    assert_eq!(take_drops_for_test(), unsure);
}

/// A report after the key states what it did, and a commit without one
/// that it changed nothing.
#[test]
fn row_H9_in_step_again_after_a_matched_report() {
    let mut ime = Ime::enabled();
    ime.report("abc", 3);
    ime.step(Input::GuestKey(SEAT));
    ime.report("abc", 3);
    assert_eq!(ime.delete(2, 1).guest, [delete(1, 0), done(3)]);

    ime.step(Input::GuestKey(SEAT));
    let out = ime.step(Input::Commit(TI));
    assert_eq!(out.guest, [done(4)]);
    assert_eq!(ime.delete(1, 1).guest, [delete(1, 0), done(4)]);
}
