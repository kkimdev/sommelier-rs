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

use super::*;

/// No pieces before an edit.
fn none() -> V1Pieces {
    V1Pieces::default()
}

/// A `preedit_cursor(index)` before a `preedit_string`.
fn cursor(index: i32) -> V1Pieces {
    V1Pieces {
        cursor: Some(index),
        ..V1Pieces::default()
    }
}

/// A `preedit_styling(index, length, style)` and a
/// `preedit_cursor(cursor)` before a `preedit_string`.
fn styled(cursor: i32, index: u32, length: u32, style: u32) -> V1Pieces {
    V1Pieces {
        cursor: Some(cursor),
        styling: vec![(index, length, style)],
        cursor_position: None,
    }
}

/// A `cursor_position(index, anchor)` before a `commit_string`.
fn position(index: i32, anchor: i32) -> V1Pieces {
    V1Pieces {
        cursor_position: Some((index, anchor)),
        ..V1Pieces::default()
    }
}

/// Something that happens to a mirror.
type Event = fn(&mut ExoMirror);

/// Three syllables of three bytes each: 가 0..3, 나 3..6, 다 6..9.
const GANADA: &str = "가나다";

fn state(text: &str, selection: (usize, usize), composition: Range<usize>) -> ExoState {
    ExoState {
        text: text.to_owned(),
        selection: Span::new(selection.0, selection.1),
        composition,
    }
}

/// A mirror at extension v11 whose prediction and only history state
/// is `predicted`.
fn at(predicted: ExoState) -> ExoMirror {
    let mut mirror = ExoMirror::new();
    let print = Fingerprint::of(&predicted, None);
    mirror.predicted = predicted;
    mirror.expected = VecDeque::from([Entry::Known {
        print,
        in_step: false,
    }]);
    mirror
}

/// A history entry is a state's if it is its exact fingerprint.
impl PartialEq<ExoState> for Entry {
    fn eq(&self, state: &ExoState) -> bool {
        matches!(self, Entry::Known { print, .. } if *print == Fingerprint::of(state, None))
    }
}

/// The history, without the marks.
fn history(mirror: &ExoMirror) -> Vec<Entry> {
    let unmarked = |entry: &Entry| match entry {
        Entry::Known { print, .. } => Entry::Known {
            print: print.clone(),
            in_step: false,
        },
        Entry::Unknown => Entry::Unknown,
    };
    mirror.expected().iter().map(unmarked).collect()
}

/// The prediction is the last history state, as every STT function
/// but `CancelComposition` leaves it.
#[track_caller]
fn assert_pushed(mirror: &ExoMirror, predicted: &ExoState) {
    assert_eq!(mirror.predicted(), predicted);
    let back = mirror.expected().back();
    assert!(back.is_some_and(|entry| entry == predicted), "{back:?}");
    assert!(predicted.fits(), "{predicted:?}");
}

/// `Update` of a report of `text` with `selection`.
fn update(mirror: &mut ExoMirror, text: &str, selection: Span) -> UpdateResult {
    mirror.update(text.to_owned(), Key::of(text), selection)
}

fn sent_report(text: &str, cursor: u32, anchor: u32) -> Sent {
    Sent::report(text.to_owned(), cursor, anchor, false)
}

/// Sends a report and its barrier, and lets Exo process both.
fn report(mirror: &mut ExoMirror, text: &str, cursor: u32, anchor: u32) {
    let token = BarrierToken(1);
    mirror.sent(token, sent_report(text, cursor, anchor));
    mirror.barrier_done(token);
}

fn cancel(mirror: &mut ExoMirror) {
    let token = BarrierToken(2);
    mirror.sent(token, Sent::Cancel);
    mirror.barrier_done(token);
}

/// A fingerprint's keys are those of whole texts: of the text, and with
/// a composition, of the text without it and of its text, however the
/// composition splits the text's bytes.
#[test]
fn a_fingerprint_keys_whole_texts() {
    let mut rng = crate::test_support::Rng::new(1);
    for _ in 0..2_000 {
        let len = rng.below(40);
        let text: String = (0..len)
            .map(|_| *rng.pick(&['a', 'é', '가', '😀']))
            .collect();
        let mut boundaries: Vec<usize> = (0..=text.len())
            .filter(|&at| text.is_char_boundary(at))
            .collect();
        let start = *rng.pick(&boundaries);
        boundaries.retain(|&at| at >= start);
        let composition = composition(start..*rng.pick(&boundaries));
        let state = ExoState {
            text: text.clone(),
            selection: Span::at(composition.end),
            composition: composition.clone(),
        };
        let print = Fingerprint::of(&state, None);
        assert_eq!(print.text, Key::of(&text), "{state:?}");
        let split = (!composition.is_empty()).then(|| {
            let mut rest = text[..composition.start].to_owned();
            rest.push_str(&text[composition.end..]);
            (Key::of(&rest), Key::of(&text[composition.clone()]))
        });
        assert_eq!(print.split, split, "{state:?}");
    }
}

/// The constructor's state, from `ResetInternal(u"", 0, Range(0))`
/// (STT 114-116).
#[test]
fn a_new_mirror_holds_the_empty_text() {
    let mirror = ExoMirror::new();
    let empty = state("", (0, 0), NO_COMPOSITION);
    assert_eq!(history(&mirror), std::slice::from_ref(&empty));
    assert_pushed(&mirror, &empty);
    assert_eq!(mirror.health(), Health::Exact);
}

/// `Update` (STT 131-191), exact match: the first state from the
/// oldest with an equal text and selection is matched, the states
/// before it go, and the prediction stays.
#[test]
fn update_matches_an_exact_state_and_erases_older_ones() {
    let mut mirror = at(state("가", (3, 3), NO_COMPOSITION));
    mirror.on_set_composition_text("ㄴ", Span::at(3));
    mirror.on_set_composition_text("나", Span::at(3));
    mirror.on_confirm_composition_text(false);
    let states = history(&mirror);
    assert_eq!(states.len(), 4);
    let predicted = mirror.predicted().clone();
    assert_eq!(predicted, state("가나", (6, 6), NO_COMPOSITION));

    // `가ㄴ` with its cursor at 6 is the second state; the composition
    // is not compared ("intentionally ignored composition text").
    assert_eq!(
        update(&mut mirror, "가ㄴ", Span::at(6)),
        UpdateResult::Updated
    );
    assert_eq!(history(&mirror), states[1..]);
    assert_eq!(mirror.predicted(), &predicted);
    // An older state is gone, so it no longer matches.
    let mut reset = mirror.clone();
    assert_eq!(update(&mut reset, "가", Span::at(3)), UpdateResult::Reset);

    // The oldest of two equal states matches, which erases nothing.
    let mut mirror = at(state("가", (3, 3), NO_COMPOSITION));
    mirror.on_insert_text("나", CursorBehavior::MoveCursorAfterText);
    mirror.on_extend_selection_and_delete(3, 0);
    let states = history(&mirror);
    assert_eq!(states[0], states[2]);
    assert_eq!(
        update(&mut mirror, "가", Span::at(3)),
        UpdateResult::Updated
    );
    assert_eq!(history(&mirror), states);
}

/// `Update` compares selections with the directed
/// `gfx::Range::operator==` (range.h 99), while a report's selection
/// is `minmax(cursor, anchor)` (ZTIM 506-545): a reversed selection
/// never matches a report.
#[test]
fn update_compares_a_directed_selection() {
    let mut mirror = at(state(GANADA, (0, 0), NO_COMPOSITION));
    mirror.on_set_editable_selection_range(Span::new(6, 3));
    let states = history(&mirror);
    assert_eq!(
        update(&mut mirror.clone(), GANADA, Span::new(6, 3)),
        UpdateResult::Updated
    );
    assert_eq!(
        update(&mut mirror.clone(), GANADA, Span::new(3, 6)),
        UpdateResult::Reset
    );

    report(&mut mirror, GANADA, 6, 3);
    assert_eq!(history(&mirror), [state(GANADA, (3, 6), NO_COMPOSITION)]);
    assert_ne!(history(&mirror), states[1..]);

    // A forward selection matches the report either way round.
    let mut mirror = at(state(GANADA, (0, 0), NO_COMPOSITION));
    mirror.on_set_editable_selection_range(Span::new(3, 6));
    let states = history(&mirror);
    report(&mut mirror, GANADA, 6, 3);
    assert_eq!(history(&mirror), states[1..]);
    assert_eq!(mirror.health(), Health::Exact);
}

/// `Update` with no match: `ResetInternal` (STT 374-381) replaces the
/// prediction, composition included, and the history is the report
/// alone, so a report that resets a composition drops it (E4).
#[test]
fn update_with_no_match_resets_and_drops_the_composition() {
    let mut mirror = at(state("가", (3, 3), NO_COMPOSITION));
    mirror.on_set_composition_text("나", Span::at(3));
    assert_eq!(update(&mut mirror, "가", Span::at(0)), UpdateResult::Reset);
    let reported = state("가", (0, 0), NO_COMPOSITION);
    assert_eq!(history(&mirror), std::slice::from_ref(&reported));
    assert_pushed(&mirror, &reported);

    // On the wire.
    let mut mirror = at(state("가", (3, 3), NO_COMPOSITION));
    mirror.preedit_string("나", "나", &cursor(3));
    // `가` with the cursor at 3 is still in the history; at 0 it is not.
    report(&mut mirror, "가", 3, 3);
    let composition = |mirror: &ExoMirror| mirror.predicted().composition.clone();
    assert_eq!(composition(&mirror), 3..6, "a matched report keeps `나`");
    report(&mut mirror, "가", 0, 0);
    assert_eq!(history(&mirror), [state("가", (0, 0), NO_COMPOSITION)]);
    mirror.set_preedit_region(0, 3);
    assert_eq!(composition(&mirror), 0..3);
    report(&mut mirror, "가나", 6, 6);
    assert_eq!(composition(&mirror), NO_COMPOSITION);
    assert_eq!(mirror.health(), Health::Exact);
}

/// By induction, from states that fit, every STT function leaves a
/// state that fits the text, on char boundaries, so no range ever
/// leaves the text and the offset stays 0. The echo checks keep the
/// wire's ranges in the text.
#[test]
fn offsets_stay_zero() {
    let mut mirror = at(state(GANADA, (9, 9), NO_COMPOSITION));
    let steps: [(&str, Event); 14] = [
        ("compose at the end", |m| {
            m.on_set_composition_text("ㄹ", Span::at(3))
        }),
        ("insert over it", |m| {
            m.on_insert_text("라", CursorBehavior::MoveCursorAfterText)
        }),
        ("select all, reversed", |m| {
            m.on_set_editable_selection_range(Span::new(12, 0))
        }),
        ("compose over it", |m| {
            m.on_set_composition_text("마", Span::new(3, 0))
        }),
        ("confirm after", |m| m.on_confirm_composition_text(false)),
        ("delete past both ends", |m| {
            m.on_extend_selection_and_delete(30, 30)
        }),
        ("insert into nothing", |m| {
            m.on_insert_text(GANADA, CursorBehavior::MoveCursorBeforeText)
        }),
        ("compose the end", |m| {
            m.on_set_composition_from_existing_text(6..9)
        }),
        ("delete after a disjoint composition", |m| {
            m.on_extend_selection_and_delete(0, 3)
        }),
        ("compose the start", |m| {
            m.on_set_composition_from_existing_text(0..3)
        }),
        ("cursor at the end", |m| {
            m.on_set_editable_selection_range(Span::at(3))
        }),
        ("insert after a disjoint composition", |m| {
            m.on_insert_text("바", CursorBehavior::MoveCursorAfterText)
        }),
        ("compose and clear", |m| {
            m.on_set_composition_text("사", Span::at(3));
            m.on_clear_composition_text();
        }),
        ("confirm with nothing composed", |m| {
            m.on_confirm_composition_text(true)
        }),
    ];
    for (name, step) in steps {
        step(&mut mirror);
        let predicted = mirror.predicted().clone();
        assert_pushed(&mirror, &predicted);
        assert_eq!(mirror.health(), Health::Exact, "{name}");
    }
    assert_eq!(mirror.predicted(), &state("바", (3, 3), NO_COMPOSITION));
}

/// `OnSetCompositionText` (STT 202-229): the composition replaces the
/// old composition, or the selection if there is none, from its min;
/// the selection is the composition's own, directed, at its start.
#[test]
fn set_composition_replaces_the_composition_or_the_selection() {
    // At a cursor, it inserts.
    let mut mirror = at(state(GANADA, (3, 3), NO_COMPOSITION));
    mirror.on_set_composition_text("ㅎ", Span::at(3));
    assert_pushed(&mirror, &state("가ㅎ나다", (6, 6), 3..6));
    // Over a composition, it replaces it wherever the selection is.
    mirror.on_set_editable_selection_range(Span::at(0));
    mirror.on_set_composition_text("한", Span::new(3, 0));
    assert_pushed(&mirror, &state("가한나다", (6, 3), 3..6));
    assert_eq!(mirror.predicted().composition_text(), "한");
    // Over a reversed selection, it replaces the selection from its
    // min.
    let mut mirror = at(state(GANADA, (9, 3), NO_COMPOSITION));
    mirror.on_set_composition_text("글", Span::at(0));
    assert_pushed(&mirror, &state("가글", (3, 3), 3..6));
    // On the wire, the cursor is `preedit_cursor`, or else the end.
    let mut mirror = at(state(GANADA, (3, 3), NO_COMPOSITION));
    mirror.preedit_string("한", "한", &cursor(0));
    assert_pushed(&mirror, &state("가한나다", (3, 3), 3..6));
    mirror.preedit_string("한글", "한글", &none());
    assert_pushed(&mirror, &state("가한글나다", (9, 9), 3..9));
    assert_eq!(mirror.health(), Health::Exact);
}

/// An empty `SetCompositionText` with no composition, which IMA's
/// empty `UpdateCompositionText` sends, replaces the selection with
/// nothing; with a composition it ends as `OnClearCompositionText`
/// (STT 255-270) does, which Exo sends the same way (TI 265-272).
#[test]
fn an_empty_preedit_with_no_composition_deletes_the_selection() {
    let mut mirror = at(state(GANADA, (6, 3), NO_COMPOSITION));
    mirror.preedit_string("", "", &none());
    assert_pushed(&mirror, &state("가다", (3, 3), NO_COMPOSITION));

    let composing = state(GANADA, (6, 6), 3..6);
    let mut cleared = at(composing.clone());
    cleared.on_clear_composition_text();
    assert_pushed(&cleared, &state("가다", (3, 3), NO_COMPOSITION));
    let mut mirror = at(composing);
    mirror.preedit_string("", "", &none());
    assert_pushed(&mirror, cleared.predicted());
}

/// `OnClearCompositionText` (STT 255-270): the composition goes, with
/// its text, and the cursor goes to where it was; with none, the state
/// is pushed unchanged.
#[test]
fn clear_composition_erases_it_and_puts_the_cursor_at_its_start() {
    let mut mirror = at(state(GANADA, (0, 9), 3..6));
    mirror.on_clear_composition_text();
    assert_pushed(&mirror, &state("가다", (3, 3), NO_COMPOSITION));
    mirror.on_clear_composition_text();
    assert_pushed(&mirror, &state("가다", (3, 3), NO_COMPOSITION));
    assert_eq!(mirror.expected().len(), 3);
}

/// `OnSetCompositionFromExistingText` (STT 231-239): only the
/// composition changes, to the range as given.
#[test]
fn compose_existing_text_sets_only_the_composition() {
    let mut mirror = at(state(GANADA, (9, 0), NO_COMPOSITION));
    mirror.on_set_composition_from_existing_text(3..6);
    assert_pushed(&mirror, &state(GANADA, (9, 0), 3..6));
    mirror.on_set_composition_from_existing_text(0..9);
    assert_pushed(&mirror, &state(GANADA, (9, 0), 0..9));
}

/// X1: ZTIM sends the region's start relative to the selection's
/// directed end, `cursor.end()` (ZTIM 248-268).
#[test]
fn region_is_measured_from_the_selection_end() {
    let mut mirror = at(state(GANADA, (9, 9), NO_COMPOSITION));
    mirror.set_preedit_region(-6, 3);
    assert_pushed(&mirror, &state(GANADA, (9, 9), 3..6));

    let mut forward = at(state(GANADA, (0, 6), NO_COMPOSITION));
    forward.set_preedit_region(0, 3);
    assert_eq!(forward.predicted().composition_text(), "다");
    let mut reversed = at(state(GANADA, (6, 0), NO_COMPOSITION));
    reversed.set_preedit_region(0, 3);
    assert_eq!(reversed.predicted().composition_text(), "가");
    assert_eq!(reversed.health(), Health::Exact);
}

/// X1 with an empty region: the composition ends, its text stays, and
/// the state is pushed.
#[test]
fn empty_region_ends_the_composition() {
    let mut mirror = at(state(GANADA, (6, 6), 3..6));
    mirror.set_preedit_region(0, 0);
    assert_pushed(&mirror, &state(GANADA, (6, 6), NO_COMPOSITION));
    assert_eq!(mirror.expected().len(), 2);
    assert_eq!(mirror.health(), Health::Exact);
}

/// `OnConfirmCompositionText` (STT 241-253): the composition stays as
/// text; the selection stays, or moves after it. With no composition,
/// the state is still pushed. X2's `unchanged` keeps the selection.
#[test]
fn confirm_keep_or_after() {
    let composing = state(GANADA, (3, 3), 3..6);
    let mut kept = at(composing.clone());
    kept.confirm_preedit(true);
    assert_pushed(&kept, &state(GANADA, (3, 3), NO_COMPOSITION));
    let mut after = at(composing);
    after.confirm_preedit(false);
    assert_pushed(&after, &state(GANADA, (6, 6), NO_COMPOSITION));
    after.confirm_preedit(false);
    assert_pushed(&after, &state(GANADA, (6, 6), NO_COMPOSITION));
    assert_eq!(after.expected().len(), 3);
}

/// `OnInsertText` (STT 272-326) with no composition: `text` replaces
/// the selection, and the cursor goes after it, or before it.
#[test]
fn insert_replaces_the_selection() {
    let mut mirror = at(state(GANADA, (3, 3), NO_COMPOSITION));
    mirror.commit_string("한", &none());
    assert_pushed(&mirror, &state("가한나다", (6, 6), NO_COMPOSITION));
    let mut mirror = at(state(GANADA, (9, 3), NO_COMPOSITION));
    mirror.on_insert_text("글", CursorBehavior::MoveCursorBeforeText);
    assert_pushed(&mirror, &state("가글", (3, 3), NO_COMPOSITION));
    let mut mirror = at(state(GANADA, (9, 9), NO_COMPOSITION));
    mirror.on_insert_text("", CursorBehavior::MoveCursorAfterText);
    assert_pushed(&mirror, &state(GANADA, (9, 9), NO_COMPOSITION));
}

/// `OnInsertText` over a composition the selection intersects (by
/// `gfx::Range::Intersects`, range.h 106-131): `text` replaces their
/// union. An empty selection inside the composition intersects it,
/// as does one at its start; one at its end does not.
#[test]
fn insert_over_an_intersecting_composition_replaces_the_union() {
    for (selection, expected) in [
        ((3, 3), state("가한다", (6, 6), NO_COMPOSITION)),
        ((6, 0), state("한다", (3, 3), NO_COMPOSITION)),
        ((9, 3), state("가한", (6, 6), NO_COMPOSITION)),
    ] {
        let mut mirror = at(state(GANADA, selection, 3..6));
        mirror.commit_string("한", &none());
        assert_pushed(&mirror, &expected);
    }
}

/// `OnInsertText` over a disjoint composition: the composition goes
/// first, and the selection shifts back if it came after it. Then the
/// selection is replaced.
#[test]
fn insert_over_a_disjoint_composition_erases_it() {
    // The composition before the selection, the cursor at its end.
    let mut mirror = at(state(GANADA, (6, 6), 3..6));
    mirror.commit_string("한", &none());
    assert_pushed(&mirror, &state("가한다", (6, 6), NO_COMPOSITION));
    // Before a reversed selection, which keeps its length.
    let mut mirror = at(state(GANADA, (9, 6), 0..3));
    mirror.commit_string("한", &none());
    assert_pushed(&mirror, &state("나한", (6, 6), NO_COMPOSITION));
    // After the selection: nothing shifts.
    let mut mirror = at(state(GANADA, (0, 0), 6..9));
    mirror.commit_string("한", &none());
    assert_pushed(&mirror, &state("한가나", (3, 3), NO_COMPOSITION));
}

/// `OnSetEditableSelectionRange` (STT 193-200): the range, as given,
/// becomes the selection, and the composition stays.
#[test]
fn set_editable_selection_range_keeps_direction_and_composition() {
    let mut mirror = at(state(GANADA, (6, 6), 3..6));
    mirror.on_set_editable_selection_range(Span::new(9, 0));
    assert_pushed(&mirror, &state(GANADA, (9, 0), 3..6));
    mirror.on_set_editable_selection_range(Span::at(3));
    assert_pushed(&mirror, &state(GANADA, (3, 3), 3..6));
}

/// `OnExtendSelectionAndDelete` (STT 328-372) with no composition:
/// `before` and `after` extend the selection, clamped to the text
/// (`std::min` and `EraseString16WithOffset`); the range goes and the
/// cursor goes to its start. Nothing to delete still pushes.
#[test]
fn extend_selection_and_delete_erases_around_the_selection() {
    for (selection, before, after, expected) in [
        ((3, 3), 3, 0, state("나다", (0, 0), NO_COMPOSITION)),
        ((3, 3), 0, 3, state("가다", (3, 3), NO_COMPOSITION)),
        ((6, 3), 3, 3, state("", (0, 0), NO_COMPOSITION)),
        ((3, 6), 0, 0, state("가다", (3, 3), NO_COMPOSITION)),
        ((3, 3), 30, 30, state("", (0, 0), NO_COMPOSITION)),
        ((6, 6), 0, 0, state(GANADA, (6, 6), NO_COMPOSITION)),
    ] {
        let mut mirror = at(state(GANADA, selection, NO_COMPOSITION));
        mirror.on_extend_selection_and_delete(before, after);
        assert_pushed(&mirror, &expected);
        assert_eq!(mirror.expected().len(), 2);
    }
    // On the wire, the absolute range gives `before` and `after`.
    let mut mirror = at(state(GANADA, (6, 3), NO_COMPOSITION));
    mirror.delete_surrounding_text(0, 9);
    assert_pushed(&mirror, &state("", (0, 0), NO_COMPOSITION));
    let mut mirror = at(state(GANADA, (3, 3), NO_COMPOSITION));
    mirror.delete_surrounding_text(3, 0);
    assert_pushed(&mirror, &state(GANADA, (3, 3), NO_COMPOSITION));
    assert_eq!(mirror.health(), Health::Exact);
}

/// `OnExtendSelectionAndDelete` with a composition, which IMA never
/// causes (the wire's echo check diverges instead): an intersecting
/// composition widens the range, and a disjoint one goes first, which
/// shifts the range if it came before it.
#[test]
fn extend_selection_and_delete_over_a_composition() {
    for (composition, selection, before, after, expected) in [
        // Intersecting: the union goes.
        (3..6, (6, 6), 3, 0, state("가다", (3, 3), NO_COMPOSITION)),
        (3..6, (9, 9), 6, 0, state("가", (3, 3), NO_COMPOSITION)),
        (3..9, (6, 6), 6, 0, state("", (0, 0), NO_COMPOSITION)),
        // An empty range inside the composition.
        (0..6, (3, 3), 0, 0, state("다", (0, 0), NO_COMPOSITION)),
        // Disjoint, before the range: both go, and the range shifts.
        // A range that starts at the composition's end is disjoint.
        (0..3, (6, 6), 3, 0, state("다", (0, 0), NO_COMPOSITION)),
        (3..6, (6, 6), 0, 3, state("가", (3, 3), NO_COMPOSITION)),
        (0..3, (9, 9), 3, 0, state("나", (3, 3), NO_COMPOSITION)),
        (0..3, (6, 6), 0, 0, state("나다", (3, 3), NO_COMPOSITION)),
        // Disjoint, after the range: both go, nothing shifts.
        (6..9, (3, 3), 3, 0, state("나", (0, 0), NO_COMPOSITION)),
    ] {
        let mut mirror = at(state(GANADA, selection, composition.clone()));
        mirror.on_extend_selection_and_delete(before, after);
        assert_pushed(&mirror, &expected);
    }
}

/// `CancelComposition` (STT 124-129): the composition ends, its text
/// stays, and the history empties without the prediction, so the next
/// report resets even when it names the prediction.
#[test]
fn cancel_empties_the_history() {
    let mut mirror = at(state(GANADA, (9, 9), NO_COMPOSITION));
    mirror.preedit_string("라", "라", &none());
    cancel(&mut mirror);
    assert_eq!(
        mirror.predicted(),
        &state("가나다라", (12, 12), NO_COMPOSITION)
    );
    assert!(history(&mirror).is_empty());
    assert_eq!(
        update(&mut mirror, "가나다라", Span::at(12)),
        UpdateResult::Reset
    );
    // An event after the cancel leaves a history of its own state.
    let mut mirror = at(state(GANADA, (9, 9), NO_COMPOSITION));
    cancel(&mut mirror);
    mirror.commit_string("라", &none());
    assert_eq!(
        history(&mirror),
        [state("가나다라", (12, 12), NO_COMPOSITION)]
    );
    assert_eq!(mirror.health(), Health::Exact);
}

/// `SetSurroundingTextImpl` (ZTIM 506-545): the selection is
/// `minmax(cursor, anchor)`, and a request whose offsets do not
/// convert to UTF-16 (past the end, or inside a character) is ignored
/// whole.
#[test]
fn a_report_selection_is_minmax_and_unconvertible_offsets_are_ignored() {
    let mut mirror = at(state("가", (3, 3), NO_COMPOSITION));
    report(&mut mirror, GANADA, 9, 3);
    assert_eq!(history(&mirror), [state(GANADA, (3, 9), NO_COMPOSITION)]);
    for (cursor, anchor) in [(1, 1), (3, 4), (10, 0)] {
        let mut ignored = mirror.clone();
        ignored.preedit_string("라", "라", &none());
        let before = (ignored.predicted().clone(), history(&ignored));
        report(&mut ignored, GANADA, cursor, anchor);
        assert_eq!((ignored.predicted().clone(), history(&ignored)), before);
    }
}

/// A request takes effect when its barrier is done, after the events
/// that came before that `done`, not when it is sent; a barrier the
/// mirror did not queue changes nothing.
#[test]
fn a_request_applies_at_its_barrier_not_when_sent() {
    let mut mirror = at(state("가", (3, 3), NO_COMPOSITION));
    mirror.sent(BarrierToken(7), sent_report("가", 0, 0));
    mirror.sent(BarrierToken(8), Sent::Cancel);
    // Exo composed before it processed the report.
    mirror.preedit_string("나", "나", &none());
    let composing = state("가나", (6, 6), 3..6);
    assert_pushed(&mirror, &composing);
    mirror.barrier_done(BarrierToken(8));
    mirror.barrier_done(BarrierToken(9));
    assert_eq!(history(&mirror).len(), 2, "nothing applied out of order");
    // The report then matches nothing and drops the composition.
    mirror.barrier_done(BarrierToken(7));
    assert_eq!(history(&mirror), [state("가", (0, 0), NO_COMPOSITION)]);
    mirror.barrier_done(BarrierToken(8));
    assert!(history(&mirror).is_empty());
    assert_eq!(mirror.predicted(), &state("가", (0, 0), NO_COMPOSITION));

    // Applied when sent, the report would have come before the
    // composition, which would then have stayed.
    let mut early = at(state("가", (3, 3), NO_COMPOSITION));
    report(&mut early, "가", 0, 0);
    early.preedit_string("나", "나", &none());
    assert_eq!(early.predicted(), &state("나가", (3, 3), 0..3));
}

/// `cursor_position(range)` + `commit_string(composition)` is
/// `SetEditableSelectionRange` (TI 386-403), which keeps the
/// composition, whatever the range: from v11, a confirm is
/// `confirm_preedit`.
#[test]
fn a_cursor_position_pair_sets_the_selection_and_keeps_the_composition() {
    let composing = state(GANADA, (6, 6), 3..6);
    for ((index, anchor), expected) in [
        ((6, 6), composing.clone()),
        ((9, 0), state(GANADA, (0, 9), 3..6)),
    ] {
        let mut mirror = at(composing.clone());
        mirror.commit_string("나", &position(index, anchor));
        assert_pushed(&mirror, &expected);
        // The pair is spent: the next commit is an insert.
        mirror.commit_string("라", &none());
        assert!(mirror.predicted().text.contains('라'));
    }
    // With no composition, the pair commits "".
    let mut mirror = at(state(GANADA, (3, 3), NO_COMPOSITION));
    mirror.commit_string("", &position(6, 6));
    assert_pushed(&mirror, &state(GANADA, (6, 6), NO_COMPOSITION));
}

/// Every echo check: an event that does not agree with the mirror
/// diverges, which makes the prediction and the state the event pushed
/// unknown.
#[test]
fn each_echo_check_diverges() {
    let composing = state(GANADA, (6, 6), 3..6);
    let idle = state(GANADA, (6, 3), NO_COMPOSITION);
    let cases: [(&str, &ExoState, Event); 15] = [
        ("a preedit whose commit differs", &idle, |m| {
            m.preedit_string("라", "마", &none())
        }),
        ("a preedit cursor inside a character", &idle, |m| {
            m.preedit_string("라", "라", &cursor(1));
        }),
        ("a preedit cursor past its end", &idle, |m| {
            m.preedit_string("라", "라", &cursor(4));
        }),
        ("a negative preedit cursor", &idle, |m| {
            m.preedit_string("라", "라", &cursor(-1));
        }),
        ("a pair that commits other text", &composing, |m| {
            m.commit_string("다", &position(6, 6));
        }),
        ("a pair that commits text with no composition", &idle, |m| {
            m.commit_string("다", &position(6, 6));
        }),
        ("a pair out of the text", &composing, |m| {
            m.commit_string("나", &position(10, 6));
        }),
        ("a pair inside a character", &composing, |m| {
            m.commit_string("나", &position(4, 4));
        }),
        ("a delete while composing", &composing, |m| {
            m.delete_surrounding_text(3, 3)
        }),
        ("a delete inside a character", &idle, |m| {
            m.delete_surrounding_text(2, 4)
        }),
        ("a delete past the text", &idle, |m| {
            m.delete_surrounding_text(3, 9)
        }),
        ("a delete beside the selection", &idle, |m| {
            m.delete_surrounding_text(0, 3)
        }),
        ("a negative delete", &idle, |m| {
            m.delete_surrounding_text(-3, 9)
        }),
        ("a region inside a character", &idle, |m| {
            m.set_preedit_region(-1, 1)
        }),
        ("a region out of the text", &idle, |m| {
            m.set_preedit_region(-6, 3)
        }),
    ];
    for (name, start, event) in cases {
        let mut mirror = at(start.clone());
        event(&mut mirror);
        assert_eq!(mirror.health(), Health::Unknown, "{name}");
        assert!(mirror.take_diverged(), "{name}");
        let history = history(&mirror);
        assert!(history[0] == *start, "{name}");
        assert_eq!(history[1..], [Entry::Unknown], "{name}");
        // An unknown mirror pushes an unknown state for every event,
        // which is no divergence.
        mirror.confirm_preedit(true);
        assert!(!mirror.take_diverged(), "{name}");
        assert_eq!(mirror.expected().len(), 3, "{name}");
        assert_eq!(mirror.expected().back(), Some(&Entry::Unknown), "{name}");
    }
}

/// The history keeps fingerprints, so a field that gets many events
/// of Exo's with no report, such as v11's empty confirmation for each
/// key the input method does not handle, still holds the oldest state
/// Exo does, which a report then matches. Past [`HISTORY_CAP`], the
/// oldest goes, and an unknown state stands for it, which Exo could
/// match: a report then leaves the history unknown.
#[test]
fn an_evicted_state_still_matches() {
    let mut mirror = at(state("", (0, 0), NO_COMPOSITION));
    for _ in 0..100 {
        mirror.confirm_preedit(true);
    }
    mirror.commit_string("가", &none());
    assert_eq!(mirror.expected().len(), 102);
    report(&mut mirror, "", 0, 0);
    assert_eq!(mirror.health(), Health::Exact);
    assert_eq!(mirror.expected().len(), 102, "the oldest state matched");

    for _ in 0..HISTORY_CAP {
        mirror.confirm_preedit(true);
    }
    assert_eq!(mirror.expected().len(), HISTORY_CAP);
    assert_eq!(mirror.expected().front(), Some(&Entry::Unknown));
    assert_eq!(mirror.health(), Health::Exact);
    // Exo's prediction is a report of it whether Exo matched a lost
    // state or reset; after another, the mirror cannot tell.
    report(&mut mirror, "가", 3, 3);
    assert_eq!(mirror.health(), Health::Exact);
    assert_eq!(history(&mirror), [Entry::Unknown]);
    report(&mut mirror, "나", 3, 3);
    assert_eq!(mirror.health(), Health::Unknown);
}

/// Text over [`TEXT_CAP`] makes the prediction unknown, and events are
/// then neither checked nor applied, until a press's `reset` and a
/// report.
#[test]
fn long_text_makes_it_unknown() {
    let full = "가".repeat(TEXT_CAP / 3);
    let mut mirror = at(state(&full, (full.len(), full.len()), NO_COMPOSITION));
    mirror.commit_string("a", &none());
    assert_eq!(mirror.predicted().text.len(), TEXT_CAP);
    mirror.commit_string("가", &none());
    assert_eq!(mirror.health(), Health::Unknown);
    assert_eq!(mirror.expected().back(), Some(&Entry::Unknown));
    mirror.delete_surrounding_text(30, 3);
    assert!(!mirror.take_diverged());
    report(&mut mirror, "가", 3, 3);
    assert_eq!(mirror.health(), Health::Unknown);
    cancel(&mut mirror);
    report(&mut mirror, "가", 3, 3);
    assert_pushed(&mirror, &state("가", (3, 3), NO_COMPOSITION));
}

/// A report heals the mirror whenever Exo must reset to it: no state
/// Exo could match (an unknown one, or a vague one with the report's
/// text) comes before, and none equals it. That needs no press. After
/// a press's `reset` the history is empty, so any report before the
/// next event heals.
#[test]
fn a_definite_reset_heals_without_a_press() {
    let mut mirror = at(state(GANADA, (9, 9), NO_COMPOSITION));
    convert(&mut mirror, "漢字", 3);
    assert_eq!(mirror.health(), Health::Vague);
    report(&mut mirror, "가나다漢", 12, 12);
    assert_pushed(&mirror, &state("가나다漢", (12, 12), NO_COMPOSITION));
    assert_eq!(history(&mirror).len(), 1);

    let mut mirror = at(state(GANADA, (9, 9), NO_COMPOSITION));
    mirror.delete_surrounding_text(0, 3);
    assert_eq!(mirror.health(), Health::Unknown);
    cancel(&mut mirror);
    report(&mut mirror, "라", 3, 3);
    assert_pushed(&mirror, &state("라", (3, 3), NO_COMPOSITION));
}

/// A report whose outcome the mirror cannot tell, as an unknown state
/// or a vague one with its text comes first, leaves the history
/// unknown, and the prediction too, unless it is the report's with no
/// composition. Only a press's `reset` with no event before the next
/// report heals that. A report that matches a state before the unknown
/// one keeps the history from there.
#[test]
fn an_ambiguous_report_stays_unknown() {
    let start = state(GANADA, (9, 9), NO_COMPOSITION);
    let mut mirror = at(start.clone());
    mirror.delete_surrounding_text(0, 3);
    report(&mut mirror, GANADA, 9, 9);
    assert_eq!(mirror.health(), Health::Unknown);
    assert_eq!(mirror.expected().len(), 2);
    report(&mut mirror, "가", 3, 3);
    assert_eq!(history(&mirror), [Entry::Unknown]);
    assert_eq!(mirror.health(), Health::Unknown);
    // An event between the press's reset and the report.
    cancel(&mut mirror);
    mirror.commit_string("나", &none());
    report(&mut mirror, "다", 3, 3);
    assert_eq!(mirror.health(), Health::Unknown);

    // A vague state with the report's text and a selection it may
    // have.
    let mut mirror = at(start);
    convert(&mut mirror, "漢字", 3);
    report(&mut mirror, "가나다漢字", 12, 12);
    assert_eq!(history(&mirror), [Entry::Unknown]);
    assert_eq!(mirror.health(), Health::Unknown);
}

/// A report's selection is forward. A vague one starts at the preedit
/// cursor, and is empty there or runs to the highlight's other end, so
/// only a report with one of those selections may match a vague state.
/// After a conversion that a confirmation keeping the selection ends,
/// as Japanese input methods send, the guest's report of the committed
/// text with its cursor after it resets Exo, which heals the mirror.
#[test]
fn a_report_unlike_a_vague_selection_heals_it() {
    let confirmed = || {
        let mut mirror = at(state(GANADA, (9, 9), NO_COMPOSITION));
        convert(&mut mirror, "漢字", 3);
        mirror.confirm_preedit(true);
        mirror
    };
    let mut mirror = confirmed();
    report(&mut mirror, "가나다漢字", 15, 15);
    assert_pushed(&mirror, &state("가나다漢字", (15, 15), NO_COMPOSITION));
    assert_eq!(history(&mirror).len(), 1);
    // The cursor at the highlight's end, and the highlight selected.
    for (cursor, anchor) in [(12, 12), (12, 9)] {
        let mut mirror = confirmed();
        report(&mut mirror, "가나다漢字", cursor, anchor);
        assert_eq!(mirror.health(), Health::Unknown, "{cursor}, {anchor}");
        assert_eq!(history(&mirror), [Entry::Unknown]);
    }
}

/// A conversion's preedit `text`, its first `selected` bytes
/// highlighted, with the cursor at their end.
fn convert(mirror: &mut ExoMirror, text: &str, selected: u32) {
    mirror.preedit_string(
        text,
        text,
        &styled(
            selected as i32,
            0,
            selected,
            zwp_text_input_v1::PREEDIT_STYLE_HIGHLIGHT,
        ),
    );
}

/// IMA makes a composition selection from a highlight span with the
/// cursor at one end (`ExtractCompositionText`), whose other end the
/// wire does not carry: the selection is vague, inside the
/// composition, and the rest of the state is known. An event that sets
/// the selection absolutely or replaces the whole composition makes
/// the mirror exact again; one that depends on where the selection is
/// makes it unknown.
#[test]
fn a_highlight_is_vague_until_a_commit() {
    let highlight = zwp_text_input_v1::PREEDIT_STYLE_HIGHLIGHT;
    // The selection is empty at the cursor, or runs to the other end.
    for (case, style, (index, length), other) in [
        (
            "a highlight ending at the cursor",
            highlight,
            (0, 3),
            Some(0),
        ),
        ("one starting at it", highlight, (3, 3), Some(6)),
        ("one apart from it", highlight, (6, 3), None),
        (
            "an underline",
            zwp_text_input_v1::PREEDIT_STYLE_UNDERLINE,
            (0, 3),
            None,
        ),
    ] {
        let mut mirror = at(state("", (0, 0), NO_COMPOSITION));
        mirror.preedit_string(GANADA, GANADA, &styled(3, index, length, style));
        let health = [Health::Exact, Health::Vague][usize::from(other.is_some())];
        assert_eq!(mirror.health(), health, "{case}");
        let vague = other.map(|other: usize| [Span::at(3), Span::new(other.min(3), other.max(3))]);
        let print = Fingerprint::of(&state(GANADA, (3, 3), 0..9), vague.as_ref().map(|v| &v[..]));
        let pushed = Entry::Known {
            print,
            in_step: false,
        };
        assert_eq!(history(&mirror).last(), Some(&pushed), "{case}");
        // The spans are spent.
        mirror.preedit_string("가", "가", &cursor(3));
        assert_eq!(mirror.health(), Health::Exact, "{case}");
    }

    let vague = || {
        let mut mirror = at(state(GANADA, (9, 9), NO_COMPOSITION));
        convert(&mut mirror, "漢字", 3);
        mirror
    };
    let converted = state("가나다漢字", (15, 15), NO_COMPOSITION);
    // A commit replaces the composition, which holds the selection.
    let mut mirror = vague();
    mirror.commit_string("漢字", &none());
    assert_pushed(&mirror, &converted);
    let mut mirror = vague();
    mirror.confirm_preedit(false);
    assert_pushed(&mirror, &converted);
    let mut mirror = vague();
    mirror.commit_string("漢字", &position(9, 9));
    assert_pushed(&mirror, &state("가나다漢字", (9, 9), 9..15));
    // A confirm that keeps the selection keeps it vague, and a delete
    // over it sets it.
    let mut mirror = vague();
    mirror.confirm_preedit(true);
    assert_eq!(mirror.health(), Health::Vague);
    mirror.delete_surrounding_text(9, 6);
    assert_pushed(&mirror, &state(GANADA, (9, 9), NO_COMPOSITION));
    // A region is measured from the selection, and a commit with no
    // composition replaces it.
    let mut mirror = vague();
    mirror.set_preedit_region(-3, 3);
    assert_eq!(mirror.health(), Health::Unknown);
    let mut mirror = vague();
    mirror.confirm_preedit(true);
    mirror.commit_string("라", &none());
    assert_eq!(mirror.health(), Health::Unknown);
    assert!(!mirror.take_diverged());
}

/// A report that may reach Exo apart from its barrier leaves the
/// mirror exact if no event of Exo's comes while it is on its way.
#[test]
fn a_split_report_without_an_event_stays_exact() {
    let mut mirror = at(state("가", (3, 3), NO_COMPOSITION));
    mirror.sent(BarrierToken(1), split_report("가", 0));
    mirror.barrier_done(BarrierToken(1));
    assert_pushed(&mirror, &state("가", (0, 0), NO_COMPOSITION));
    mirror.commit_string("나", &none());
    assert_pushed(&mirror, &state("나가", (3, 3), NO_COMPOSITION));
}

/// An event of Exo's that comes while a report that may reach Exo
/// apart from its barrier is on its way may have come before the
/// report or after it: its state is unknown, and so is the mirror,
/// until a press's `reset` and a report.
#[test]
fn a_split_report_with_an_event_is_unknown() {
    let mut mirror = at(state("가", (3, 3), NO_COMPOSITION));
    mirror.sent(BarrierToken(1), split_report("가", 0));
    mirror.commit_string("나", &none());
    assert_eq!(mirror.health(), Health::Unknown);
    assert!(!mirror.take_diverged());
    mirror.barrier_done(BarrierToken(1));
    assert_eq!(mirror.health(), Health::Unknown);
    cancel(&mut mirror);
    report(&mut mirror, "가나", 6, 6);
    assert_pushed(&mirror, &state("가나", (6, 6), NO_COMPOSITION));
}

fn split_report(text: &str, cursor: u32) -> Sent {
    Sent::report(text.to_owned(), cursor, cursor, true)
}

/// A report on its way to Exo crosses the events since it was sent if
/// it will name no state then, or follows a `reset`.
#[test]
fn a_report_crosses_when_it_will_name_no_state() {
    let mut mirror = at(state("ab", (2, 2), NO_COMPOSITION));
    mirror.commit_string("c", &none());
    let sent = |text: &str, cursor| sent_report(text, cursor, cursor);
    mirror.sent(BarrierToken(1), sent("ab", 2));
    mirror.sent(BarrierToken(2), sent("abc", 3));
    assert!(!mirror.report_crosses());
    mirror.sent(BarrierToken(3), sent("abX", 3));
    assert!(mirror.report_crosses());

    let mut mirror = at(state("ab", (2, 2), NO_COMPOSITION));
    mirror.sent(BarrierToken(1), Sent::Cancel);
    assert!(!mirror.report_crosses());
    mirror.sent(BarrierToken(2), sent("ab", 2));
    assert!(mirror.report_crosses());
}

/// A matched report puts a guest that shows no preedit at the first
/// equal state without a composition, and in step if every transaction
/// since kept it so.
#[test]
fn a_matched_report_finds_the_guest_without_a_composition() {
    let applied = |second: bool| {
        let mut mirror = at(state("", (0, 0), NO_COMPOSITION));
        mirror.preedit_string("가", "가", &none());
        mirror.mark(false);
        mirror.commit_string("가", &none());
        mirror.mark(true);
        mirror.preedit_string("나", "나", &none());
        mirror.mark(second);
        let token = BarrierToken(1);
        mirror.sent(token, sent_report("가", 3, 3));
        mirror.barrier_done(token)
    };
    // The report names the state with the composition "가" first, whose
    // transaction did not keep the guest in step; but the guest, which
    // reports "가" as text, committed it.
    assert_eq!(applied(true), Some(Applied::Matched { in_step: true }));
    assert_eq!(applied(false), Some(Applied::Matched { in_step: false }));
}

/// Korean typing, end to end on the wire: each preedit replaces the
/// last, the commit replaces the composition, and the guest's report
/// after it names the latest state.
#[test]
fn korean_typing_through_the_wire() {
    let mut mirror = ExoMirror::new();
    for syllable in ["ㅎ", "하", "한"] {
        mirror.preedit_string(syllable, syllable, &cursor(3));
    }
    assert_pushed(&mirror, &state("한", (3, 3), 0..3));
    mirror.commit_string("한", &none());
    mirror.preedit_string("ㄱ", "ㄱ", &cursor(3));
    assert_pushed(&mirror, &state("한ㄱ", (6, 6), 3..6));
    // The guest reports the commit, not the preedit it shows. That
    // matches the oldest state equal to it, the preedit `한`, as
    // `Update` ignores compositions.
    report(&mut mirror, "한", 3, 3);
    assert_eq!(history(&mirror)[0], state("한", (3, 3), 0..3));
    assert_eq!(history(&mirror).len(), 3);
    assert_eq!(mirror.predicted(), &state("한ㄱ", (6, 6), 3..6));
    mirror.preedit_string("글", "글", &none());
    mirror.confirm_preedit(false);
    assert_pushed(&mirror, &state("한글", (6, 6), NO_COMPOSITION));
    report(&mut mirror, "한글", 6, 6);
    assert_eq!(
        history(&mirror),
        [
            state("한글", (6, 6), 3..6),
            state("한글", (6, 6), NO_COMPOSITION)
        ]
    );
    mirror.delete_surrounding_text(3, 3);
    assert_pushed(&mirror, &state("한", (3, 3), NO_COMPOSITION));
    assert_eq!(mirror.health(), Health::Exact);
}
