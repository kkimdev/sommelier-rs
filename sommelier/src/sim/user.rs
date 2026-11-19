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
//! actions are about 56% keys (two-set jamo, in the order syllables take,
//! plus Space and Backspace), 2% accepted suggestions and 2%
//! reconversions (in a free case, half of each followed at once by
//! another, for a delete while the guest has not reported the first and a
//! region over a region), 15%
//! clicks, 8% taps, 7% focus moves, 2% restarts of the input method and 5%
//! settles. A free case also holds a key one action in 33: it presses
//! it, may type up to two other keys, and releases it, while the engine
//! repeats it. A Backspace comes only where it deletes no marker, and a
//! suggestion or a reconversion only where it changes something; in a
//! settled case, also only right after a key. GTK3 reports a cursor that a
//! click moved only with its next edit, so after a click the engine would
//! measure the word or the region from where the cursor was; and once the
//! guest reports a region, Exo drops it as a composition (E4), so the
//! engine cannot grow it.
//!
//! A case is free or settled. A settled case is one the user-intent oracle
//! can judge ([`settled`]): a `Settle` comes before and after every click,
//! tap, focus move, restart, suggestion and reconversion. Otherwise Exo can hand the engine a key after a
//! press but before the press's reset, whose result the proxy drops by
//! design (the fence), or a key can reach the client raw while a focus
//! move's activation is in flight (E7). A settled case also settles after
//! a key the engine passes while IMA takes the engine to compose ([`Ima`]):
//! IMA confirms before the key (E13) and then resets the engine behind the
//! next key, if that is already in flight, so the host loses that key's
//! text (E5), which no proxy can restore.

use super::editor::{Reference, Target, TextModel, Typing, UserAction};
use super::hangul::{Composer, Key};
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
    let mut ima = Ima::default();
    let mut actions = Vec::new();
    // A request a free case makes again at once, or the rest of a hold.
    let mut again = None;
    let mut holding = Vec::new();
    // Whether the last action but a settle was a key, which the client
    // reported, so that the engine knows where the cursor is.
    let mut typed = false;
    for _ in 0..10 + rng.below(51) {
        let asks = typed || !settled;
        // A settled case asks so rarely right after a key that it asks then
        // one time in eight, where it can.
        let (takes, reconverts) = (reference.takes_suggestion(), reference.reconverts());
        let more = settled && typed && (takes || reconverts) && rng.one_in(8);
        let action = match rng.below(100) {
            _ if again.is_some() => again.take().expect("a request to repeat"),
            _ if !holding.is_empty() => holding.remove(0),
            56..59 if !settled => {
                // Mostly a key the input method passes once nothing is
                // composed, whose repeats it then confirms (E13).
                let field = &reference.fields()[reference.focus()];
                let held = match rng.below(5) {
                    0 | 1 => Key::Char(' '),
                    2 | 3 if deletes_no_marker(field) => Key::Backspace,
                    _ => key(rng, &reference),
                };
                // Up to two other keys while it is held, then its release.
                let others = (0..rng.below(3)).map(|_| match rng.below(3) {
                    0 => letter(rng, CONSONANTS),
                    1 => letter(rng, VOWELS),
                    _ => Key::Char(' '),
                });
                holding = others
                    .filter(|&other| other != held)
                    .map(UserAction::Key)
                    .chain([UserAction::KeyUp(held)])
                    .collect();
                UserAction::KeyDown(held)
            }
            _ if more && takes && (!reconverts || rng.one_in(2)) => UserAction::AcceptSuggestion,
            _ if more => UserAction::Reconvert,
            59..61 if asks && takes => {
                again = (!settled && rng.one_in(2)).then_some(UserAction::AcceptSuggestion);
                UserAction::AcceptSuggestion
            }
            61..63 if asks && reconverts => {
                again = (!settled && rng.one_in(2)).then_some(UserAction::Reconvert);
                UserAction::Reconvert
            }
            0..63 => UserAction::Key(key(rng, &reference)),
            63..78 => UserAction::Click {
                field: rng.below(FIELDS),
                at: target(rng),
            },
            78..86 => UserAction::Tap {
                field: rng.below(FIELDS),
                at: target(rng),
            },
            86..93 => UserAction::Focus(rng.below(FIELDS)),
            93..95 => UserAction::RestartInputMethod,
            _ => UserAction::Settle,
        };
        let moves = !matches!(
            action,
            UserAction::Key(_) | UserAction::KeyDown(_) | UserAction::KeyUp(_) | UserAction::Settle
        );
        let stale = match action {
            UserAction::Key(key) => !typing.latin && ima.key(key),
            // Only a free case holds a key.
            UserAction::KeyDown(_) | UserAction::KeyUp(_) => false,
            UserAction::Click { field, .. } | UserAction::Tap { field, .. } => {
                ima.moved(field != reference.focus());
                false
            }
            UserAction::Focus(field) => {
                if field != reference.focus() {
                    ima.moved(true);
                }
                false
            }
            // A new input method takes the engine to compose nothing.
            UserAction::RestartInputMethod => {
                ima.moved(true);
                false
            }
            // IMA's flag would not be stale after a key that a region
            // reconverted ahead of: the key commits the region, which
            // clears it. But the host may show no region, so the flag is
            // kept as if there were none.
            UserAction::AcceptSuggestion | UserAction::Reconvert | UserAction::Settle => false,
        };
        if settled && moves {
            settle(&mut actions);
        }
        if action != UserAction::Settle {
            typed = matches!(action, UserAction::Key(_));
        }
        reference.apply(action);
        match action {
            UserAction::Settle => settle(&mut actions),
            action => actions.push(action),
        }
        if settled && (moves || stale) {
            settle(&mut actions);
        }
    }
    // A hold the case ends in is released.
    actions.extend(holding);
    actions
}

/// Whether `actions` make a settled case: each click, tap, focus move and
/// restart has a `Settle` just before it and just after it, where the start and
/// the end of the case count as settled.
pub(super) fn settled(actions: &[UserAction]) -> bool {
    let settles = |at: Option<&UserAction>| matches!(at, None | Some(UserAction::Settle));
    actions.iter().enumerate().all(|(i, action)| {
        matches!(action, UserAction::Key(_) | UserAction::Settle)
            || (settles(i.checked_sub(1).map(|i| &actions[i])) && settles(actions.get(i + 1)))
    })
}

/// Whether `InputMethodAsh` takes the Korean engine to be composing
/// (`composing_text_`), as the user's keys leave it. Any composition the
/// engine shows sets it, an empty one included, and only a commit or a
/// reset clears it (`input_method_ash.cc`).
#[derive(Clone, Debug, Default)]
struct Ima {
    /// The engine's composer.
    composer: Composer,
    composing: bool,
}

impl Ima {
    /// The user types `key`. Returns whether the engine passes it while IMA
    /// takes the engine to compose: IMA's confirmation before it then
    /// resets the engine (E13), behind the next key if that is in flight.
    fn key(&mut self, key: Key) -> bool {
        let shown = self.composer.preedit();
        let out = self.composer.key(key);
        // The engine takes a Space that ends a composition (E15).
        let ends_with_space = key == Key::Char(' ') && !out.commit.is_empty();
        if !out.consumed && !ends_with_space {
            return std::mem::take(&mut self.composing);
        }
        if !out.commit.is_empty() {
            self.composing = !out.preedit.is_empty();
        } else if out.preedit != shown {
            self.composing = true;
        }
        false
    }

    /// A press, or a focus move `away` to another field or a restart of the
    /// input method. A focus move detaches IMA, which resets, and a restart
    /// replaces it; a press resets it only if it ends a composition, as
    /// only then does the proxy reset the host (F4).
    fn moved(&mut self, away: bool) {
        if away || !self.composer.preedit().is_empty() {
            self.composing = false;
        }
        self.composer.confirm();
    }
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
    match rng.below(100) {
        0..40 => letter(rng, CONSONANTS),
        40..75 => letter(rng, VOWELS),
        75..83 => letter(rng, SHIFTED),
        83..93 => Key::Char(' '),
        _ if deletes_no_marker(&reference.fields()[reference.focus()]) => Key::Backspace,
        _ => letter(rng, CONSONANTS),
    }
}

/// A key of `keys`.
fn letter(rng: &mut Rng, keys: &[u8]) -> Key {
    Key::Char(char::from(*rng.pick(keys)))
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
    use crate::sim::hangul::keys;

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
        assert!(!settled(&[key, UserAction::RestartInputMethod]));
        assert!(!settled(&[
            key,
            UserAction::Tap {
                field: 0,
                at: Target::Start
            }
        ]));
    }

    /// Whether IMA confirms with a stale flag before each key of
    /// `script` (see [`crate::sim::hangul::keys`]), `|` being a click on
    /// the focused field and `>` a focus move to the other one.
    fn stale(script: &str) -> Vec<bool> {
        let mut ima = Ima::default();
        let mut stale = Vec::new();
        for c in script.chars() {
            match c {
                '|' => ima.moved(false),
                '>' => ima.moved(true),
                c => stale.push(ima.key(keys(&c.to_string())[0])),
            }
        }
        stale
    }

    #[test]
    fn a_suggestion_or_a_reconversion_comes_only_where_it_changes_something() {
        for (texts, actions) in cases(Typing::default(), false) {
            let fields = texts.iter().map(|text| TextModel::new(text)).collect();
            let mut reference = Reference::new(fields);
            for (i, &action) in actions.iter().enumerate() {
                let again = i > 0 && actions[i - 1] == action;
                match action {
                    UserAction::AcceptSuggestion if !again => assert!(reference.takes_suggestion()),
                    UserAction::Reconvert if !again => assert!(reference.reconverts()),
                    _ => {}
                }
                reference.apply(action);
            }
        }
    }

    #[test]
    fn a_settled_case_asks_the_engine_only_right_after_a_key() {
        for (_, actions) in cases(Typing::default(), true) {
            let acts: Vec<UserAction> = actions
                .into_iter()
                .filter(|&action| action != UserAction::Settle)
                .collect();
            for pair in acts.windows(2) {
                let asks = matches!(
                    pair[1],
                    UserAction::AcceptSuggestion | UserAction::Reconvert
                );
                let after = matches!(pair[0], UserAction::Key(_));
                assert!(!asks || after, "{pair:?}");
            }
        }
    }

    #[test]
    fn ima_stays_composing_after_any_composition_until_a_commit_or_a_reset() {
        // A key the engine passes with nothing ever composed, or after a
        // commit, finds IMA not composing.
        assert_eq!(stale("  rk  "), [false, false, false, false, false, false]);
        // A composition taken back to nothing leaves it composing, until
        // the passed key's confirmation resets it.
        assert_eq!(stale("r⌫  "), [false, false, true, false]);
        // A focus move resets it; a click resets it only if it ends a
        // composition, as only then does the proxy reset the host.
        assert_eq!(stale("r⌫> "), [false, false, false]);
        assert_eq!(stale("r⌫| "), [false, false, true]);
        assert_eq!(stale("r| "), [false, false]);
    }

    #[test]
    fn a_settled_case_settles_after_a_key_that_imas_stale_flag_confirms() {
        for (_, actions) in cases(Typing::default(), true) {
            let mut ima = Ima::default();
            let mut focus = 0;
            for (i, &action) in actions.iter().enumerate() {
                match action {
                    UserAction::Key(key) if ima.key(key) => {
                        assert_eq!(actions.get(i + 1), Some(&UserAction::Settle), "{actions:?}");
                    }
                    UserAction::Click { field, .. } | UserAction::Tap { field, .. } => {
                        ima.moved(field != focus);
                        focus = field;
                    }
                    UserAction::Focus(field) if field != focus => {
                        ima.moved(true);
                        focus = field;
                    }
                    UserAction::RestartInputMethod => ima.moved(true),
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn the_user_types_no_digit_and_deletes_no_marker() {
        let typings = [
            Typing::default(),
            Typing {
                latin: true,
                nfd_backspace: true,
                regionless: false,
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
    fn a_free_case_holds_a_key_and_releases_it_after_other_keys() {
        let mut holds = 0;
        for (_, actions) in cases(Typing::default(), false) {
            let mut held = None;
            for action in actions {
                match action {
                    UserAction::KeyDown(key) => {
                        assert_eq!(held.replace(key), None, "one key held at a time");
                        holds += 1;
                    }
                    UserAction::KeyUp(key) => assert_eq!(held.take(), Some(key)),
                    UserAction::Key(key) => assert_ne!(held, Some(key)),
                    other => assert_eq!(held, None, "{other:?} while a key is held"),
                }
            }
            assert_eq!(held, None);
        }
        assert!(holds > 0);
        for (_, actions) in cases(Typing::default(), true) {
            assert!(!actions.iter().any(|a| matches!(a, UserAction::KeyDown(_))));
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
        assert!((50..75).contains(&share(|a| matches!(a, UserAction::Key(_)))));
        assert!((10..20).contains(&share(|a| matches!(a, UserAction::Click { .. }))));
        assert!((4..12).contains(&share(|a| matches!(a, UserAction::Tap { .. }))));
        assert!((4..10).contains(&share(|a| matches!(a, UserAction::Focus(_)))));
        assert!((1..4).contains(&share(|a| *a == UserAction::RestartInputMethod)));
        assert!(actions.contains(&UserAction::AcceptSuggestion));
        assert!(actions.contains(&UserAction::Reconvert));
        for request in [UserAction::AcceptSuggestion, UserAction::Reconvert] {
            let twice = actions.windows(2).any(|w| w == [request; 2]);
            assert!(twice, "{request:?} is made twice at once");
        }
        assert!((2..8).contains(&share(|a| *a == UserAction::Settle)));
        let backspaces = actions.contains(&UserAction::Key(Key::Backspace));
        assert!(backspaces, "a Backspace is generated");
    }
}
