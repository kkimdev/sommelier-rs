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

/// The recovery's transition, for each host event it can meet in each
/// of its states.
#[test]
fn a_waiting_recovery_is_decided_by_the_hosts_next_event() {
    use HostSignal::{KeyPress, Modifiers, Other, PeekPress, TextInputLeave, UnhandledKey};
    use RecoveryStep::{Drop, Recover, Wait};
    use RecoveryWait::NextEvent;
    let held = HeldRepeatKey {
        guest_keyboard_id: 5,
        host_keyboard_id: HostId(50),
        key: 14,
        serial: 7,
        time: 70,
    };
    let (other_keyboard, key, serial) = (HostId(51), 30, 8);
    let press = RecoveryWait::Press {
        keyboard: other_keyboard,
        key,
        serial,
    };
    let peek = |keyboard, new| PeekPress {
        keyboard,
        key,
        serial,
        new,
    };
    let key_press = |keyboard, serial| KeyPress {
        keyboard,
        key,
        serial,
    };
    for (wait, signal, step) in [
        (NextEvent, Modifiers, Wait(NextEvent)),
        (NextEvent, peek(Some(other_keyboard), true), Wait(press)),
        (NextEvent, peek(Some(other_keyboard), false), Recover),
        (NextEvent, peek(None, true), Recover),
        (NextEvent, key_press(other_keyboard, serial), Recover),
        (NextEvent, TextInputLeave { ti: Some(TiId(9)) }, Drop),
        (NextEvent, TextInputLeave { ti: Some(TiId(10)) }, Recover),
        (NextEvent, TextInputLeave { ti: None }, Recover),
        (NextEvent, Other, Recover),
        (press, key_press(other_keyboard, serial), Drop),
        (press, key_press(other_keyboard, serial + 1), Recover),
        (press, key_press(HostId(50), serial), Recover),
        (press, Modifiers, Recover),
        (press, peek(Some(other_keyboard), true), Recover),
        (press, TextInputLeave { ti: Some(TiId(9)) }, Drop),
        (press, Other, Recover),
        (
            NextEvent,
            UnhandledKey {
                keyboard: other_keyboard,
            },
            Recover,
        ),
        (
            press,
            UnhandledKey {
                keyboard: other_keyboard,
            },
            Drop,
        ),
        (
            press,
            UnhandledKey {
                keyboard: HostId(50),
            },
            Recover,
        ),
    ] {
        let pending = PendingRecovery {
            ti: TiId(9),
            seat: SeatId(1),
            held,
            wait,
        };
        assert_eq!(pending.decide(signal), step, "{wait:?} {signal:?}");
    }
}
