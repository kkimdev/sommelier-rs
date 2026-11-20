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

//! The recovery of an empty IME confirmation's held key, through the
//! adapters (`crate::keys::recovery`).

use crate::handler::text_input::tests::*;
use crate::keys::KeyInput;
use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::Event as V3;
use crate::state::{HostId, KeyboardFocusUpdate};

/// An IME edit, whether or not it reaches the guest, ends the
/// held-Backspace fallback a confirmation leased.
#[test]
fn ime_edits_end_a_stale_backspace_repeat() {
    type Event = fn(&mut Ime);
    let cases: [(&str, Event); 3] = [
        ("an empty commit", |ime| ime.commit("")),
        ("a new preedit", |ime| ime.preedit("나", "나")),
        // Out of step with Exo's mirror, the core drops it.
        ("a dropped preedit region", |ime| ime.preedit_region(-3, 3)),
    ];
    for (case, event) in cases {
        let mut ime = Ime::with_keyboard();
        ime.committed().surrounding_text_support = true;
        ime.state().show_for_test("가");
        ime.lease_backspace_repeat(TI);

        event(&mut ime);
        assert!(!ime.repeat_active(), "{case}");
    }
}

/// Exo consumes a held Backspace over committed text and confirms an
/// empty preedit for each repeat: each becomes a key pair with the
/// serial of the held key's peek, and the repeat stays leased.
#[test]
fn held_backspace_repeats_over_committed_korean_without_preedit() {
    let mut ime = Ime::with_keyboard();
    ime.hold_peeked(HOST_KEYBOARD, BACKSPACE, 700, 100);
    ime.committed().surrounding_text_support = true;

    for _ in 0..3 {
        ime.confirm();
        assert_eq!(
            ime.events(),
            [
                key(KEYBOARD, BACKSPACE, PRESSED, 700, 100),
                key(KEYBOARD, BACKSPACE, RELEASED, 700, 100),
                uncommitted_done(),
            ]
        );
    }
    assert!(ime.repeat_active());
}

#[test]
fn empty_confirmation_recovers_any_repeatable_ime_consumed_key() {
    const TAB: u32 = 15;
    const ENTER: u32 = 28;
    const SPACE: u32 = 57;
    for held in [BACKSPACE, TAB, ENTER, SPACE] {
        let mut ime = Ime::with_keyboard();
        ime.hold_peeked(HOST_KEYBOARD, held, 700, 100);

        ime.confirm();
        assert_eq!(
            ime.events(),
            [
                key(KEYBOARD, held, PRESSED, 700, 100),
                key(KEYBOARD, held, RELEASED, 700, 100),
                uncommitted_done(),
            ]
        );
    }
}

/// An empty confirmation recovers only a key a keyboard of its text
/// input's seat holds: one held on another seat's keyboard is not its.
#[test]
fn a_key_held_on_another_seats_keyboard_is_not_recovered() {
    const OTHER_GUEST: u32 = 70;
    let other = HostId(71);
    let mut ime = Ime::with_keyboard();
    ime.ctx
        .shadow_table
        .insert_paired_for_test(OTHER_GUEST, other.0, "wl_keyboard");
    crate::handler::keyboard::for_test::created(&mut ime.ctx, OTHER_GUEST, other.0, 9);
    ime.hold_peeked(other, 57, 700, 100);
    ime.confirm();
    assert_eq!(ime.events(), []);
    // Held on the seat's own keyboard, the key is recovered.
    ime.hold_peeked(HOST_KEYBOARD, 57, 701, 101);
    ime.confirm();
    assert_ne!(ime.events(), []);
}

#[test]
fn empty_confirmation_does_not_fall_back_from_newest_ineligible_key() {
    const SHIFT: u32 = 42;
    let mut ime = Ime::with_keyboard();
    ime.hold_peeked(HOST_KEYBOARD, 57, 700, 100);

    // Shift is newer but XKB does not mark it repeatable. The
    // confirmation belongs to that newest generation or to no
    // recoverable key; it must never synthesize the older held Space.
    ime.hold_peeked(HOST_KEYBOARD, SHIFT, 701, 101);
    ime.confirm();
    assert_eq!(ime.events(), []);

    ime.peek(SHIFT, RELEASED, 702, 102);
    ime.confirm();
    assert_eq!(
        ime.events(),
        [],
        "releasing the superseding key must not revive an older generation"
    );
}

/// Keys held since `wl_keyboard.enter` are ordered by their peeks, not
/// by the order the enter listed them in: an empty confirmation stands
/// for the key peeked last. The guest holds both from the enter, so it
/// gets no pair, only the `done` of a held key found.
#[test]
fn keys_held_since_enter_are_ordered_by_their_peeks() {
    const TAB: u32 = 15;
    const SPACE: u32 = 57;
    let mut ime = Ime::with_extended_keyboard();
    // The enter of a second keyboard on the seat's surface moves no
    // focus.
    let enter = KeyInput::Focus {
        update: KeyboardFocusUpdate::default(),
        enter: Some((HOST_KEYBOARD, vec![TAB, SPACE])),
        keyboard: Vec::new(),
    };
    crate::handler::keyboard::keys(&mut ime.ctx, enter);
    // The default keymap repeats both.
    crate::handler::keyboard::for_test::default_keymap(&mut ime.ctx, HOST_KEYBOARD.0);

    ime.peek(SPACE, PRESSED, 700, 100);
    ime.peek(TAB, PRESSED, 701, 101);
    ime.confirm();
    assert_eq!(ime.events(), [uncommitted_done()]);
}

#[test]
fn empty_confirmation_prefers_the_keyboard_focused_on_the_current_surface() {
    let mut ime = Ime::with_keyboard();
    let stale = HostId(51);
    ime.keyboard(50, stale);
    ime.focus(HOST_KEYBOARD);
    ime.hold_peeked(HOST_KEYBOARD, 57, 700, 100);
    ime.hold_peeked(stale, 28, 701, 101);

    ime.confirm();
    assert_eq!(
        ime.events(),
        [
            key(KEYBOARD, 57, PRESSED, 700, 100),
            key(KEYBOARD, 57, RELEASED, 700, 100),
            uncommitted_done(),
        ]
    );
}

#[test]
fn empty_confirmation_never_uses_a_stale_unfocused_keyboard() {
    let mut ime = Ime::with_keyboard();
    ime.focus(HostId(99));
    ime.hold_peeked(HOST_KEYBOARD, 57, 700, 100);

    ime.confirm();
    assert_eq!(
        ime.events(),
        [],
        "an active surface without a matching keyboard must not use stale focus"
    );
}

/// Exo empties the preedit as Backspace deletes it, then confirms each
/// repeat over the committed text: each becomes a Backspace pair, until
/// the fallback ends.
#[test]
fn emptied_preedit_enables_held_backspace_fallback_for_committed_text() {
    let mut ime = Ime::with_keyboard();
    ime.hold_peeked(HOST_KEYBOARD, BACKSPACE, 701, 101);
    ime.state().set_commits_for_test(2);
    ime.committed().surrounding_text_support = true;
    ime.preedit("가", "");
    ime.preedit("", "");
    ime.events();

    for _ in 0..2 {
        ime.confirm();
        assert_eq!(
            ime.events(),
            [
                key(KEYBOARD, BACKSPACE, PRESSED, 701, 101),
                key(KEYBOARD, BACKSPACE, RELEASED, 701, 101),
                (TI, V3::Done { serial: 2 }.into()),
            ]
        );
        assert!(ime.committed().surrounding_text_support);
    }

    // Another key's press ends the lease for the rest of the hold.
    assert_eq!(ime.key(15, PRESSED, 702), crate::wire::Action::Forward);
    ime.confirm();
    assert_eq!(ime.events(), []);
}

/// A physical Backspace press the guest already got, then the host
/// clearing its preedit and confirming: the confirmation closes the
/// transaction without another pair.
#[test]
fn empty_confirmation_does_not_duplicate_forwarded_backspace() {
    let mut ime = Ime::with_keyboard();
    ime.hold_peeked(HOST_KEYBOARD, BACKSPACE, 1, 100);
    let press = ime.key(BACKSPACE, PRESSED, 1);
    assert_eq!(press, crate::wire::Action::Forward);
    ime.state().show_for_test("가");
    ime.committed().surrounding_text_support = true;
    ime.preedit("", "");
    assert!(ime.repeat_active());
    ime.events();

    ime.confirm();
    assert_eq!(ime.events(), [uncommitted_done()]);
    assert!(!ime.repeat_active());
}
