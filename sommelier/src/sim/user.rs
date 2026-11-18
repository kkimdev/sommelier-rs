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

//! The user: the fields a case starts with and what the user does in them.
//!
//! Each field starts with marker digits ([`Target::BeforeMarker`]) between
//! a few letters and syllables, and the user never types a digit, so a
//! marker names the same place to the user and to a client that lags. The
//! actions are about 65% keys (two-set jamo, in the order syllables take,
//! plus Space and Backspace), 15% clicks, 8% taps, 7% focus moves and 5%
//! settles; a Backspace comes only where it deletes no marker.
//!
//! A case is free or settled. A settled case is one the user-intent oracle
//! can judge ([`settled`]): a `Settle` comes before and after every click,
//! tap and focus move. Otherwise Exo can hand the engine a key after a
//! press but before the press's reset, whose result the proxy drops by
//! design (the fence), or a key can reach the client raw while a focus
//! move's activation is in flight (E7).

use super::editor::{Reference, Target, TextModel, Typing, UserAction};
use super::hangul::Key;
use crate::test_support::Rng;

/// The fields a case has.
pub(super) const FIELDS: usize = 2;

/// The keys of the two-set layout's consonants and vowels.
const CONSONANTS: &[u8] = b"qwertasdfgzxcv";
const VOWELS: &[u8] = b"yuiophjklbnm";
/// The keys that Shift turns into another jamo.
const SHIFTED: &[u8] = b"QWERTOP";

/// The initial texts of the fields: markers 0, 1 and 2 in order, each
/// followed by up to two letters or syllables.
pub(super) fn texts(rng: &mut Rng) -> Vec<String> {
    let text = |rng: &mut Rng| {
        let mut text = String::new();
        for marker in ['0', '1', '2'] {
            text.push(marker);
            for _ in 0..rng.below(3) {
                text.push_str(rng.pick(&["a", "b", "가", "닭"]));
            }
        }
        text
    };
    (0..FIELDS).map(|_| text(rng)).collect()
}

/// Between 10 and 60 actions on fields holding `texts`, typed as `typing`
/// says; `settled` makes them a settled case.
pub(super) fn actions(
    rng: &mut Rng,
    texts: &[String],
    typing: Typing,
    settled: bool,
) -> Vec<UserAction> {
    let fields = texts.iter().map(|text| TextModel::new(text)).collect();
    let mut reference = Reference::new(fields).typing(typing);
    let mut actions = Vec::new();
    for _ in 0..10 + rng.below(51) {
        let action = match rng.below(100) {
            0..65 => UserAction::Key(key(rng, &reference)),
            65..80 => UserAction::Click {
                field: rng.below(FIELDS),
                at: target(rng),
            },
            80..88 => UserAction::Tap {
                field: rng.below(FIELDS),
                at: target(rng),
            },
            88..95 => UserAction::Focus(rng.below(FIELDS)),
            _ => UserAction::Settle,
        };
        let moves = !matches!(action, UserAction::Key(_) | UserAction::Settle);
        if settled && moves {
            settle(&mut actions);
        }
        reference.apply(action);
        match action {
            UserAction::Settle => settle(&mut actions),
            action => actions.push(action),
        }
        if settled && moves {
            settle(&mut actions);
        }
    }
    actions
}

/// Whether `actions` make a settled case: each click, tap and focus move
/// has a `Settle` just before it and just after it, where the start and
/// the end of the case count as settled.
pub(super) fn settled(actions: &[UserAction]) -> bool {
    let settles = |at: Option<&UserAction>| matches!(at, None | Some(UserAction::Settle));
    actions.iter().enumerate().all(|(i, action)| {
        matches!(action, UserAction::Key(_) | UserAction::Settle)
            || (settles(i.checked_sub(1).map(|i| &actions[i])) && settles(actions.get(i + 1)))
    })
}

/// A `Settle`, unless the last action is one.
fn settle(actions: &mut Vec<UserAction>) {
    if actions.last() != Some(&UserAction::Settle) {
        actions.push(UserAction::Settle);
    }
}

/// A key: a consonant, a vowel, a shifted jamo, Space, or a Backspace
/// where it deletes no marker in the focused field of `reference`.
fn key(rng: &mut Rng, reference: &Reference) -> Key {
    let letter = |rng: &mut Rng, keys: &[u8]| Key::Char(char::from(*rng.pick(keys)));
    match rng.below(100) {
        0..40 => letter(rng, CONSONANTS),
        40..75 => letter(rng, VOWELS),
        75..83 => letter(rng, SHIFTED),
        83..93 => Key::Char(' '),
        _ if deletes_no_marker(&reference.fields()[reference.focus()]) => Key::Backspace,
        _ => letter(rng, CONSONANTS),
    }
}

/// Whether a Backspace in `field` takes back a jamo or deletes a character
/// that is not a marker.
fn deletes_no_marker(field: &TextModel) -> bool {
    let before = field.text[..field.cursor].chars().next_back();
    !field.preedit.is_empty() || before.is_some_and(|c| !c.is_ascii_digit())
}

/// A press's point: the start, the end or a marker.
fn target(rng: &mut Rng) -> Target {
    match rng.below(5) {
        0 => Target::Start,
        1 => Target::End,
        marker => Target::BeforeMarker(marker as u8 - 2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The actions of seeds 0 to 199, each with its fields' texts.
    fn cases(typing: Typing, settled: bool) -> Vec<(Vec<String>, Vec<UserAction>)> {
        (0..200)
            .map(|seed| {
                let mut rng = Rng::new(seed);
                let texts = texts(&mut rng);
                let actions = actions(&mut rng, &texts, typing, settled);
                (texts, actions)
            })
            .collect()
    }

    #[test]
    fn a_settled_case_settles_around_each_press_and_focus_move() {
        for (_, actions) in cases(Typing::default(), true) {
            assert!(settled(&actions), "{actions:?}");
        }
        let unsettled = cases(Typing::default(), false);
        assert!(unsettled.iter().any(|(_, actions)| !settled(actions)));
        let click = UserAction::Click {
            field: 1,
            at: Target::End,
        };
        let key = UserAction::Key(Key::Char('r'));
        assert!(settled(&[click, UserAction::Settle, key]));
        assert!(!settled(&[key, click]));
        assert!(!settled(&[UserAction::Focus(1), key]));
        assert!(!settled(&[
            key,
            UserAction::Tap {
                field: 0,
                at: Target::Start
            }
        ]));
    }

    #[test]
    fn the_user_types_no_digit_and_deletes_no_marker() {
        let typings = [
            Typing::default(),
            Typing {
                latin: true,
                nfd_backspace: true,
            },
        ];
        for typing in typings {
            for (texts, actions) in cases(typing, false) {
                assert!((10..=60 * 3).contains(&actions.len()));
                let fields = texts.iter().map(|text| TextModel::new(text)).collect();
                let mut reference = Reference::new(fields).typing(typing);
                for action in actions {
                    if let UserAction::Key(Key::Char(c)) = action {
                        assert!(!c.is_ascii_digit(), "{c:?}");
                    }
                    reference.apply(action);
                }
                for field in reference.fields() {
                    let markers: String = field.text.chars().filter(char::is_ascii_digit).collect();
                    assert_eq!(markers, "012", "{field:?}");
                }
            }
        }
    }

    #[test]
    fn the_mix_holds_every_kind_of_action() {
        let actions: Vec<UserAction> = cases(Typing::default(), false)
            .into_iter()
            .flat_map(|(_, actions)| actions)
            .collect();
        let share = |is: fn(&UserAction) -> bool| {
            actions.iter().filter(|action| is(action)).count() * 100 / actions.len()
        };
        assert!((55..75).contains(&share(|a| matches!(a, UserAction::Key(_)))));
        assert!((10..20).contains(&share(|a| matches!(a, UserAction::Click { .. }))));
        assert!((4..12).contains(&share(|a| matches!(a, UserAction::Tap { .. }))));
        assert!((4..10).contains(&share(|a| matches!(a, UserAction::Focus(_)))));
        assert!((2..8).contains(&share(|a| *a == UserAction::Settle)));
        let backspaces = actions.contains(&UserAction::Key(Key::Backspace));
        assert!(backspaces, "a Backspace is generated");
    }
}
