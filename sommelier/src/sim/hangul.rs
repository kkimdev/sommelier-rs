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

//! A deterministic two-set (dubeolsik) Hangul composer.
//!
//! The simulated input method engine and the reference editor both type
//! Korean through [`Composer`], so the text a correct system should end
//! with and the text the simulated engine produces come from one set of
//! rules. It follows the default two-set behaviour of libhangul, behind
//! ibus-hangul and fcitx-hangul, and takes the ChromeOS Korean input
//! method to behave the same for these keys:
//!
//! - A syllable is an initial consonant, a medial vowel and an optional
//!   final consonant, written as `U+AC00 + (L * 21 + V) * 28 + T`.
//! - Two vowels can make a compound medial (ㅗ + ㅏ = ㅘ), and two
//!   consonants a compound final (ㄱ + ㅅ = ㄳ), from the standard tables.
//! - A vowel after a final takes the final as its initial (각 + ㅏ = 가 +
//!   가); after a compound final, only its second consonant (닭 + ㅏ = 달
//!   + 가).
//! - A jamo that is not part of a syllable is shown alone, as a Hangul
//!   compatibility jamo (U+3131 to U+3163). A consonant after a lone vowel
//!   starts a new syllable rather than becoming the vowel's initial.
//! - Shift with Q, W, E, R, T, O or P types the double consonant or the
//!   other vowel on that key (ㅃ ㅉ ㄸ ㄲ ㅆ ㅒ ㅖ); Shift with any other
//!   letter types the same jamo as without it.
//! - Backspace takes back the last jamo typed, so a compound medial or
//!   final comes apart one keystroke at a time.
//! - Any other key ends the composition and is not consumed: the syllable
//!   is committed, and the key reaches the application.
//!
//! Assumptions only a real ChromeOS host can confirm: that its Korean
//! input method, with default settings, composes and deletes jamo by jamo
//! as above; and that Space, Enter, the arrows and punctuation commit the
//! syllable and then reach the application, rather than being consumed.

/// A key the user presses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Key {
    /// A printable key, as the US layout types it: `' '` is Space, and an
    /// uppercase letter is Shift and that letter.
    Char(char),
    /// Backspace: deletes a jamo while composing, else a character.
    Backspace,
    /// Enter, which an application takes as a line break.
    Enter,
    /// The left arrow, which moves the cursor back one character.
    Left,
    /// The right arrow, which moves the cursor on one character.
    Right,
}

/// The keys a script names, for tests and replays: each character is a
/// [`Key::Char`], except `⌫`, `⏎`, `←` and `→`, which are Backspace,
/// Enter, Left and Right.
pub(super) fn keys(script: &str) -> Vec<Key> {
    script
        .chars()
        .map(|c| match c {
            '⌫' => Key::Backspace,
            '⏎' => Key::Enter,
            '←' => Key::Left,
            '→' => Key::Right,
            c => Key::Char(c),
        })
        .collect()
}

/// What the composer made of one key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Output {
    /// The text the key finished, which leaves the composition; empty if
    /// none. It comes before the key's own effect.
    pub commit: String,
    /// The composition after the key; empty if none.
    pub preedit: String,
    /// Whether the composer used the key. A key it did not use reaches the
    /// application after `commit`.
    pub consumed: bool,
}

/// The syllable being composed, kept as the jamo the user typed so that
/// Backspace can take back one keystroke at a time.
///
/// Keystrokes arrive in the order initial, medials, finals, so Backspace
/// removes from the end of that order. A final needs an initial and a
/// medial; a lone medial takes no consonant.
#[derive(Clone, Debug, Default)]
pub(super) struct Composer {
    /// The initial consonant.
    initial: Option<char>,
    /// The medial vowel's keystrokes: none, one, or the two of a compound.
    medial: Vec<char>,
    /// The final consonant's keystrokes: none, one, or the two of a
    /// compound.
    final_: Vec<char>,
}

/// The 19 initial consonants, in syllable order.
const INITIALS: &str = "ㄱㄲㄴㄷㄸㄹㅁㅂㅃㅅㅆㅇㅈㅉㅊㅋㅌㅍㅎ";

/// The 27 final consonants, in syllable order; index 0 of a syllable's
/// finals means none.
const FINALS: &str = "ㄱㄲㄳㄴㄵㄶㄷㄹㄺㄻㄼㄽㄾㄿㅀㅁㅂㅄㅅㅆㅇㅈㅊㅋㅌㅍㅎ";

/// The first of the 21 medial vowels, which follow in syllable order as
/// consecutive compatibility jamo, ㅏ to ㅣ.
const FIRST_MEDIAL: char = 'ㅏ';

/// Two vowel keystrokes and the compound medial they make.
const COMPOUND_MEDIALS: [(char, char, char); 7] = [
    ('ㅗ', 'ㅏ', 'ㅘ'),
    ('ㅗ', 'ㅐ', 'ㅙ'),
    ('ㅗ', 'ㅣ', 'ㅚ'),
    ('ㅜ', 'ㅓ', 'ㅝ'),
    ('ㅜ', 'ㅔ', 'ㅞ'),
    ('ㅜ', 'ㅣ', 'ㅟ'),
    ('ㅡ', 'ㅣ', 'ㅢ'),
];

/// Two consonant keystrokes and the compound final they make.
const COMPOUND_FINALS: [(char, char, char); 11] = [
    ('ㄱ', 'ㅅ', 'ㄳ'),
    ('ㄴ', 'ㅈ', 'ㄵ'),
    ('ㄴ', 'ㅎ', 'ㄶ'),
    ('ㄹ', 'ㄱ', 'ㄺ'),
    ('ㄹ', 'ㅁ', 'ㄻ'),
    ('ㄹ', 'ㅂ', 'ㄼ'),
    ('ㄹ', 'ㅅ', 'ㄽ'),
    ('ㄹ', 'ㅌ', 'ㄾ'),
    ('ㄹ', 'ㅍ', 'ㄿ'),
    ('ㄹ', 'ㅎ', 'ㅀ'),
    ('ㅂ', 'ㅅ', 'ㅄ'),
];

/// The jamo the two-set layout gives a key, as a compatibility jamo.
fn jamo(c: char) -> Option<char> {
    const LETTERS: &str = "qwertyuiopasdfghjklzxcvbnm";
    const JAMO: &str = "ㅂㅈㄷㄱㅅㅛㅕㅑㅐㅔㅁㄴㅇㄹㅎㅗㅓㅏㅣㅋㅌㅊㅍㅠㅜㅡ";
    const SHIFTED: [(char, char); 7] = [
        ('Q', 'ㅃ'),
        ('W', 'ㅉ'),
        ('E', 'ㄸ'),
        ('R', 'ㄲ'),
        ('T', 'ㅆ'),
        ('O', 'ㅒ'),
        ('P', 'ㅖ'),
    ];
    if let Some(&(_, j)) = SHIFTED.iter().find(|&&(k, _)| k == c) {
        return Some(j);
    }
    let letter = LETTERS.chars().position(|l| l == c.to_ascii_lowercase())?;
    JAMO.chars().nth(letter)
}

/// Whether a compatibility jamo is a vowel; every other one is a consonant.
fn is_vowel(jamo: char) -> bool {
    jamo >= FIRST_MEDIAL
}

/// The jamo that one or two keystrokes make, using `compounds` for two.
fn joined(keystrokes: &[char], compounds: &[(char, char, char)]) -> Option<char> {
    match *keystrokes {
        [] => None,
        [one] => Some(one),
        [first, second] => compound(first, second, compounds),
        _ => unreachable!("a jamo has at most two keystrokes"),
    }
}

/// The compound that `first` then `second` make, if any.
fn compound(first: char, second: char, compounds: &[(char, char, char)]) -> Option<char> {
    compounds
        .iter()
        .find(|&&(a, b, _)| (a, b) == (first, second))
        .map(|&(_, _, c)| c)
}

/// Where `jamo` is in `table`, counting characters.
fn index(table: &str, jamo: char) -> Option<u32> {
    table.chars().position(|c| c == jamo).map(|i| i as u32)
}

impl Composer {
    /// Feeds one key.
    pub fn key(&mut self, key: Key) -> Output {
        let commit = match key {
            Key::Char(c) => match jamo(c) {
                Some(j) if is_vowel(j) => self.vowel(j),
                Some(j) => self.consonant(j),
                None => return self.pass(),
            },
            Key::Backspace => {
                if !self.backspace() {
                    return self.pass();
                }
                String::new()
            }
            Key::Enter | Key::Left | Key::Right => return self.pass(),
        };
        Output {
            commit,
            preedit: self.preedit(),
            consumed: true,
        }
    }

    /// The composition, as the user sees it; empty if none.
    pub fn preedit(&self) -> String {
        let medial = joined(&self.medial, &COMPOUND_MEDIALS);
        let final_ = joined(&self.final_, &COMPOUND_FINALS);
        match (self.initial, medial) {
            (Some(l), Some(v)) => syllable(l, v, final_).to_string(),
            (Some(lone), None) | (None, Some(lone)) => lone.to_string(),
            (None, None) => String::new(),
        }
    }

    /// Discards the composition, as an input method's reset (cancel) does.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Ends the composition and returns its text, as an input method's
    /// confirm does; empty if there was none.
    pub fn confirm(&mut self) -> String {
        let text = self.preedit();
        self.reset();
        text
    }

    /// A key the composer does not use: the composition is committed and
    /// the key passes.
    fn pass(&mut self) -> Output {
        Output {
            commit: self.confirm(),
            preedit: String::new(),
            consumed: false,
        }
    }

    /// Feeds a vowel; returns the text it finished.
    fn vowel(&mut self, v: char) -> String {
        let joins = match *self.medial {
            [] => true,
            [first] => compound(first, v, &COMPOUND_MEDIALS).is_some(),
            _ => false,
        };
        if joins && self.final_.is_empty() {
            self.medial.push(v);
            return String::new();
        }
        // The last final keystroke, if any, becomes the next initial.
        let initial = self.final_.pop();
        let commit = self.confirm();
        self.initial = initial;
        self.medial.push(v);
        commit
    }

    /// Feeds a consonant; returns the text it finished.
    fn consonant(&mut self, c: char) -> String {
        let fits = match (self.initial, &*self.medial, &*self.final_) {
            (Some(_), [_, ..], []) => index(FINALS, c).is_some(),
            (Some(_), [_, ..], [first]) => compound(*first, c, &COMPOUND_FINALS).is_some(),
            _ => false,
        };
        if fits {
            self.final_.push(c);
            return String::new();
        }
        let commit = self.confirm();
        self.initial = Some(c);
        commit
    }

    /// Takes back the last jamo typed; false if there was none.
    fn backspace(&mut self) -> bool {
        self.final_.pop().is_some() || self.medial.pop().is_some() || self.initial.take().is_some()
    }
}

/// The precomposed syllable of an initial, a medial and an optional final.
fn syllable(initial: char, medial: char, final_: Option<char>) -> char {
    let l = index(INITIALS, initial).expect("every consonant key is an initial");
    let v = medial as u32 - FIRST_MEDIAL as u32;
    let t = final_.map_or(0, |t| index(FINALS, t).expect("only finals are kept") + 1);
    char::from_u32(0xAC00 + (l * 21 + v) * 28 + t).expect("a Hangul syllable")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed text and the composition after typing `script` into
    /// an empty composer, with every key's commit appended in order, and
    /// each passed key's character too, as an application would insert it.
    fn typed(script: &str) -> (String, String) {
        let mut composer = Composer::default();
        let mut text = String::new();
        for key in keys(script) {
            let out = composer.key(key);
            text += &out.commit;
            if let (false, Key::Char(c)) = (out.consumed, key) {
                text.push(c);
            }
        }
        (text, composer.preedit())
    }

    /// Asserts what each script in `table` types: (script, committed text,
    /// composition).
    fn check(table: &[(&str, &str, &str)]) {
        for &(script, text, preedit) in table {
            assert_eq!(
                typed(script),
                (text.to_owned(), preedit.to_owned()),
                "typing {script:?}"
            );
        }
    }

    #[test]
    fn initial_medial_and_final_compose_one_syllable() {
        check(&[
            ("r", "", "ㄱ"),
            ("rk", "", "가"),
            ("rkr", "", "각"),
            ("gks", "", "한"),
            // The first and the last syllables of the block.
            ("rk", "", "\u{AC00}"),
            ("glg", "", "\u{D7A3}"),
        ]);
    }

    #[test]
    fn two_vowels_make_a_compound_medial() {
        check(&[
            ("rhk", "", "과"),
            ("rho", "", "괘"),
            ("rhl", "", "괴"),
            ("rnj", "", "궈"),
            ("rnp", "", "궤"),
            ("rnl", "", "귀"),
            ("dml", "", "의"),
            ("rhkr", "", "곽"),
        ]);
    }

    #[test]
    fn a_vowel_that_does_not_combine_starts_a_lone_vowel() {
        check(&[
            ("rkk", "가", "ㅏ"),
            ("rhkl", "과", "ㅣ"),
            ("rkr k", "각 ", "ㅏ"),
        ]);
    }

    #[test]
    fn two_consonants_make_a_compound_final() {
        check(&[
            ("rkrt", "", "갃"),
            ("dksw", "", "앉"),
            ("dksg", "", "않"),
            ("ekfr", "", "닭"),
            ("tkfa", "", "삶"),
            ("dufq", "", "엷"),
            ("rhft", "", "곬"),
            ("gkfx", "", "핥"),
            ("dmfv", "", "읊"),
            ("dlfg", "", "잃"),
            ("djqt", "", "없"),
        ]);
    }

    #[test]
    fn a_consonant_that_does_not_fit_the_final_starts_a_syllable() {
        check(&[
            // The final does not combine with it.
            ("rkrr", "각", "ㄱ"),
            // A compound final takes no third consonant.
            ("ekfrr", "닭", "ㄱ"),
            // ㄸ, ㅃ and ㅉ are never finals.
            ("rkE", "가", "ㄸ"),
            ("rkQ", "가", "ㅃ"),
            ("rkW", "가", "ㅉ"),
        ]);
    }

    #[test]
    fn a_vowel_moves_the_final_to_the_next_syllable() {
        check(&[
            ("rkrk", "가", "가"),
            ("rkRk", "가", "까"),
            ("rkTj", "가", "써"),
            ("rkrkr", "가", "각"),
        ]);
    }

    #[test]
    fn a_vowel_splits_a_compound_final() {
        check(&[
            ("ekfrk", "달", "가"),
            ("rkrtk", "각", "사"),
            ("dksgk", "안", "하"),
            ("djqtj", "업", "서"),
        ]);
    }

    #[test]
    fn a_jamo_without_a_syllable_is_shown_alone() {
        check(&[
            ("k", "", "ㅏ"),
            ("hk", "", "ㅘ"),
            // A consonant after a lone vowel does not become its initial.
            ("kr", "ㅏ", "ㄱ"),
            // Two initials do not combine; Shift types the double one.
            ("rr", "ㄱ", "ㄱ"),
            ("rt", "ㄱ", "ㅅ"),
            ("R", "", "ㄲ"),
        ]);
    }

    #[test]
    fn shift_types_the_double_jamo_or_the_same_jamo() {
        check(&[
            ("Rk", "", "까"),
            ("Ek", "", "따"),
            ("Qk", "", "빠"),
            ("Tk", "", "싸"),
            ("Wk", "", "짜"),
            ("rO", "", "걔"),
            ("rP", "", "계"),
            ("RK", "", "까"),
            ("GKS", "", "한"),
        ]);
    }

    #[test]
    fn whole_words_compose() {
        check(&[
            ("rkskdk", "가나", "아"),
            ("dkssudgktpdy", "안녕하세", "요"),
            ("gksrnrdj", "한국", "어"),
            ("dhkswjsgkek", "완전하", "다"),
            ("ekfrdl", "닭", "이"),
        ]);
    }

    #[test]
    fn backspace_takes_back_one_jamo_at_a_time() {
        check(&[
            ("ekfr⌫", "", "달"),
            ("ekfr⌫⌫", "", "다"),
            ("ekfr⌫⌫⌫", "", "ㄷ"),
            ("ekfr⌫⌫⌫⌫", "", ""),
            ("rhk⌫", "", "고"),
            ("hk⌫", "", "ㅗ"),
            ("rhkr⌫⌫", "", "고"),
            // A finished syllable stays committed.
            ("rkrk⌫", "가", "ㄱ"),
            ("rkrk⌫⌫", "가", ""),
            // A Backspace and the jamo again give the same syllable.
            ("ekfr⌫r", "", "닭"),
        ]);
    }

    #[test]
    fn backspace_without_a_composition_is_not_consumed() {
        let mut composer = Composer::default();
        assert_eq!(composer.key(Key::Backspace), Output::default());
        composer.key(Key::Char('r'));
        assert_eq!(
            composer.key(Key::Backspace),
            Output {
                commit: String::new(),
                preedit: String::new(),
                consumed: true,
            }
        );
        assert_eq!(composer.key(Key::Backspace), Output::default());
    }

    #[test]
    fn a_key_that_types_no_jamo_commits_and_passes() {
        for key in keys(" .1⏎←→") {
            let mut composer = Composer::default();
            for jamo in keys("rkr") {
                composer.key(jamo);
            }
            assert_eq!(
                composer.key(key),
                Output {
                    commit: "각".to_owned(),
                    preedit: String::new(),
                    consumed: false,
                },
                "{key:?}"
            );
            assert_eq!(composer.key(key), Output::default(), "{key:?} again");
        }
    }

    #[test]
    fn a_jamo_key_reports_the_finished_syllable_then_the_composition() {
        let mut composer = Composer::default();
        let outputs: Vec<_> = keys("rkrk").into_iter().map(|k| composer.key(k)).collect();
        let syllables: Vec<_> = outputs
            .iter()
            .map(|o| (o.commit.as_str(), o.preedit.as_str(), o.consumed))
            .collect();
        assert_eq!(
            syllables,
            [
                ("", "ㄱ", true),
                ("", "가", true),
                ("", "각", true),
                ("가", "가", true),
            ]
        );
    }

    #[test]
    fn reset_discards_and_confirm_commits_the_composition() {
        let mut composer = Composer::default();
        for key in keys("ekfr") {
            composer.key(key);
        }
        let mut cancelled = composer.clone();
        cancelled.reset();
        assert_eq!(cancelled.preedit(), "");
        assert_eq!(cancelled.confirm(), "");

        assert_eq!(composer.confirm(), "닭");
        assert_eq!(composer.preedit(), "");
        // Neither leaves anything for the next vowel to join.
        assert_eq!(composer.key(Key::Char('k')).preedit, "ㅏ");
        assert_eq!(cancelled.key(Key::Char('k')).preedit, "ㅏ");
    }
}
