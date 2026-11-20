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

use super::*;

/// The evdev codes of Tab and Enter.
const KEY_TAB: u32 = 15;
const KEY_ENTER: u32 = 28;

/// The guest reports the surrounding text "가", with the cursor at its
/// end, and commits: the host gets it, and answers the barrier the proxy
/// sends with it. Returns `ime` with the commit counted.
async fn report(kb: &mut Kb, mut ime: Ime) -> Ime {
    kb.s.guest([
        zwp_text_input_v3::req::set_surrounding_text(ime.id, "가", 3, 3).unwrap(),
        zwp_text_input_v3::req::commit(ime.id),
    ])
    .await;
    ime.serial += 1;
    assert_eq!(kb.s.guest_received(), [done(ime)]);
    let reported = zwp_text_input_v1::Request::SetSurroundingText {
        text: "가".into(),
        cursor: 3,
        anchor: 3,
    };
    let host = kb.s.host_received();
    assert!(host.contains(&(ime.host, reported.into())), "{host:?}");
    for (_, request) in host {
        if let AnyRequest::WlDisplay(wl_display::Request::Sync { callback }) = request {
            kb.s.host([
                wl_callback::evt::done(callback, 0),
                wl_display::evt::delete_id(WL_DISPLAY_ID, callback),
            ])
            .await;
        }
    }
    ime
}

/// An IME edit, whether or not it reaches the guest, ends the
/// held-Backspace fallback a confirmation leased: the repeat's
/// tombstone no longer takes Backspace's press.
#[tokio::test]
async fn ime_edits_end_a_stale_backspace_repeat() {
    type Event = fn(Ime) -> Outgoing;
    // Each edit, and whether the guest gets a `done` for it, after the
    // preedit it shows, if any.
    type Shows = Option<Option<&'static str>>;
    let cases: [(&str, Event, Shows); 3] = [
        (
            "an empty commit",
            |ime| zwp_text_input_v1::evt::commit_string(ime.host, 0, "").unwrap(),
            Some(None),
        ),
        (
            "a new preedit",
            |ime| zwp_text_input_v1::evt::preedit_string(ime.host, 0, "나", "나").unwrap(),
            Some(Some("나")),
        ),
        // Out of step with Exo's mirror, the core drops it.
        (
            "a dropped preedit region",
            |ime| zcr_extended_text_input_v1::evt::set_preedit_region(ime.ext, -3, 3),
            None,
        ),
    ];
    for (case, event, shows) in cases {
        let mut kb = Kb::new().await;
        let ime = kb.text_input().await;
        let ime = report(&mut kb, ime).await;
        kb.peek(1, 1, BACKSPACE, PRESSED).await;
        kb.lease(ime).await;

        kb.s.host([event(ime)]).await;
        let expected = match shows {
            Some(Some(preedit)) => vec![v3_preedit(ime, preedit), done(ime)],
            Some(None) => vec![done(ime)],
            None => vec![],
        };
        assert_eq!(kb.s.guest_received(), expected, "{case}");
        assert_eq!(kb.s.host_received(), [], "{case}");
        assert_eq!(kb.key(1, BACKSPACE, PRESSED).await, Route::Guest, "{case}");
    }
}

/// Exo consumes a held Backspace over committed text and confirms an
/// empty preedit for each repeat: each becomes a key pair with the
/// serial of the held key's peek, and the repeat stays leased, so the
/// key's delayed `wl_keyboard` press is the repeat's.
#[tokio::test]
async fn held_backspace_repeats_over_committed_korean_without_preedit() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let ime = report(&mut kb, ime).await;
    kb.peek(700, 100, BACKSPACE, PRESSED).await;

    for _ in 0..3 {
        kb.confirm(ime).await;
        assert_eq!(
            kb.s.guest_received(),
            recovered(ime, kb.k, 700, 100, BACKSPACE)
        );
    }
    assert_eq!(kb.key(700, BACKSPACE, PRESSED).await, Route::Host);
}

#[tokio::test]
async fn empty_confirmation_recovers_any_repeatable_ime_consumed_key() {
    for held in [BACKSPACE, KEY_TAB, KEY_ENTER, KEY_SPACE] {
        let mut kb = Kb::new().await;
        let ime = kb.text_input().await;
        kb.peek(700, 100, held, PRESSED).await;

        kb.confirm(ime).await;
        assert_eq!(
            kb.s.guest_received(),
            recovered(ime, kb.k, 700, 100, held),
            "key {held}"
        );
    }
}

/// An empty confirmation recovers only a key a keyboard of its text
/// input's seat holds: one held on another seat's keyboard is not its.
#[tokio::test]
async fn a_key_held_on_another_seats_keyboard_is_not_recovered() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let (_, other) = kb.s.keyboard().await;
    let surface = kb.surface;
    let other = kb.first_enter(unentered(other), surface).await;
    kb.peek_on(other, 700, 100, KEY_SPACE, PRESSED).await;
    kb.confirm(ime).await;
    assert_eq!(kb.s.guest_received(), []);
    // Held on the seat's own keyboard, the key is recovered.
    kb.peek(701, 101, KEY_SPACE, PRESSED).await;
    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        recovered(ime, kb.k, 701, 101, KEY_SPACE)
    );
}

#[tokio::test]
async fn empty_confirmation_does_not_fall_back_from_newest_ineligible_key() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    kb.peek(700, 100, KEY_SPACE, PRESSED).await;

    // Shift is newer but XKB does not mark it repeatable. The
    // confirmation belongs to that newest generation or to no
    // recoverable key; it must never synthesize the older held Space.
    kb.peek(701, 101, KEY_LEFTSHIFT, PRESSED).await;
    kb.confirm(ime).await;
    assert_eq!(kb.s.guest_received(), []);

    kb.peek(702, 102, KEY_LEFTSHIFT, RELEASED).await;
    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        [],
        "releasing the superseding key must not revive an older generation"
    );
}

/// Keys held since `wl_keyboard.enter` are ordered by their peeks, not
/// by the order the enter listed them in: an empty confirmation stands
/// for the key peeked last. The guest holds both from the enter, so it
/// gets no pair, only the `done` of a held key found.
#[tokio::test]
async fn keys_held_since_enter_are_ordered_by_their_peeks() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    // The enter of a second keyboard on the seat's surface moves no
    // focus. The default keymap repeats both keys.
    let other = unentered(kb.s.keyboard_of(kb.seat.id).await);
    let surface = kb.surface;
    let keys: Vec<u8> = [KEY_TAB, KEY_SPACE]
        .iter()
        .flat_map(|key| key.to_ne_bytes())
        .collect();
    let enter = wl_keyboard::evt::enter(other.host, 1, surface.host, &keys).unwrap();
    kb.s.host([enter]).await;
    let other = Keyboard {
        ext: extended_keyboard_of(other, &kb.s.host_received()),
        ..other
    };
    assert_eq!(
        kb.s.guest_received(),
        [
            entered(other, 1, surface.id, &[KEY_TAB, KEY_SPACE]),
            modifiers(other, 1, 0)
        ]
    );

    kb.peek_on(other, 700, 100, KEY_SPACE, PRESSED).await;
    kb.peek_on(other, 701, 101, KEY_TAB, PRESSED).await;
    kb.confirm(ime).await;
    assert_eq!(kb.s.guest_received(), [done(ime)]);
}

/// The seat's focus is on the surface through one keyboard; another of
/// the seat, which focused it once, holds a newer key: the confirmation
/// recovers the focused keyboard's.
#[tokio::test]
async fn empty_confirmation_prefers_the_keyboard_focused_on_the_current_surface() {
    let mut kb = Kb::unfocused(Context::new_for_test(false, false, Vec::new())).await;
    let surface = kb.surface;
    let stale = unentered(kb.s.keyboard_of(kb.seat.id).await);
    let stale = kb.first_enter(stale, surface).await;
    kb.s.host([wl_keyboard::evt::leave(stale.host, 2, surface.host)])
        .await;
    assert_eq!(kb.s.guest_received(), [left(stale, 2, surface.id)]);
    kb.k = kb.first_enter(kb.k, surface).await;
    let ime = kb.text_input().await;
    kb.peek(700, 100, KEY_SPACE, PRESSED).await;
    kb.peek_on(stale, 701, 101, KEY_ENTER, PRESSED).await;

    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        recovered(ime, kb.k, 700, 100, KEY_SPACE)
    );
}

#[tokio::test]
async fn empty_confirmation_never_uses_a_stale_unfocused_keyboard() {
    let mut kb = Kb::unfocused(Context::new_for_test(false, false, Vec::new())).await;
    let surface = kb.surface;
    // The keyboard focused the surface once; another of the seat now
    // does.
    kb.k = kb.first_enter(kb.k, surface).await;
    kb.s.host([wl_keyboard::evt::leave(kb.k.host, 2, surface.host)])
        .await;
    assert_eq!(kb.s.guest_received(), [left(kb.k, 2, surface.id)]);
    let other = unentered(kb.s.keyboard_of(kb.seat.id).await);
    kb.first_enter(other, surface).await;
    let ime = kb.text_input().await;
    kb.peek(700, 100, KEY_SPACE, PRESSED).await;

    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        [],
        "an active surface without a matching keyboard must not use stale focus"
    );
}

/// Exo empties the preedit as Backspace deletes it, then confirms each
/// repeat over the committed text: each becomes a Backspace pair, until
/// the fallback ends.
#[tokio::test]
async fn emptied_preedit_enables_held_backspace_fallback_for_committed_text() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let ime = report(&mut kb, ime).await;
    kb.peek(701, 101, BACKSPACE, PRESSED).await;
    kb.lease(ime).await;

    for _ in 0..2 {
        kb.confirm(ime).await;
        assert_eq!(
            kb.s.guest_received(),
            recovered(ime, kb.k, 701, 101, BACKSPACE)
        );
        // Reads state no peer can observe: the support the guest's
        // committed text gave the field, which the host has.
        let ti = crate::ime::TiId(ime.id);
        let state = kb.s.ctx().ime.get_mut(ti).expect("the text input");
        let committed = state.committed_mut_for_test().expect("a focused field");
        assert!(committed.surrounding_text_support);
    }

    // Another key's press ends the lease for the rest of the hold.
    assert_eq!(kb.key(702, KEY_TAB, PRESSED).await, Route::Guest);
    kb.confirm(ime).await;
    assert_eq!(kb.s.guest_received(), []);
}

/// A physical Backspace press the guest already got, then the host
/// clearing its preedit and confirming: the confirmation closes the
/// transaction without another pair, and ends the repeat.
#[tokio::test]
async fn empty_confirmation_does_not_duplicate_forwarded_backspace() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let ime = report(&mut kb, ime).await;
    kb.peek(1, 100, BACKSPACE, PRESSED).await;
    assert_eq!(kb.key(1, BACKSPACE, PRESSED).await, Route::Guest);
    kb.lease(ime).await;
    assert!(kb.leased(kb.k));

    kb.confirm(ime).await;
    assert_eq!(kb.s.guest_received(), [done(ime)]);
    assert!(!kb.leased(kb.k));
}
