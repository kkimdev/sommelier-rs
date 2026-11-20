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

//! The choices that fix a case.
//!
//! Every model makes each choice it leaves open, such as a latency or a
//! host behaviour no source settles, through [`Choose`], so that a recorded
//! stream of choices replays a case exactly. Choice 0 is always the
//! shortest latency or the first variant, so an all-zero stream is the
//! most sequential schedule.
//!
//! A case's messages travel FIFO [`Channel`]s, each delivered when its
//! drawn latency has passed, and [`earliest`] picks what happens next, so
//! the only reorderings are across channels, never within one.
//!
//! [`World`] joins a client model, the real proxy (a [`Session`]), the Exo
//! model, its engine and the host's [`Display`] by those channels, and
//! steps them on one clock (the design's section 4.2):
//!
//! | Channel | Carries | Latency (µs) |
//! |---|---|---|
//! | C→P | the guest's requests | 0 to 2,000 |
//! | P→C | the proxy's events to the guest | 0 to 2,000 |
//! | P→E | the proxy's requests to the host | 0 to 3,000 |
//! | E→P | the host's events to the proxy | 0 to 3,000 |
//! | E→Eng | what Exo hands the engine | 0 to 1,000 |
//!
//! The engine answers after its own service time, and the user acts at
//! the times the actions draw. The proxy gets one message at a time, the
//! finest interleaving its dispatch can produce, and what it sends goes
//! out at once. Exo, the engine and the client each handle one message
//! atomically.

use std::collections::{BTreeMap, VecDeque};

use super::client::chromium::Chromium;
use super::client::gtk::Gtk;
use super::client::{Client, Field, PressKind};
use super::clock::Micros;
use super::display::{self, Display};
use super::editor::{Target, TextModel, UserAction};
use super::engine::{Engine, EngineIn, Lag, Request};
use super::exo::{Answer, Exo, Predicted, RegionOutcome, RegionTruth};
use super::hangul::Key;
use super::keymap;
use super::oracle::{self, Judge, Origin, Seen};
use super::surfaces::{Surfaces, Windows};
use super::ClientKind;
use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1 as extended;
use crate::protocols::text_input_unstable_v3::{zwp_text_input_manager_v3, zwp_text_input_v3};
use crate::protocols::wayland::{
    wl_callback, wl_compositor, wl_display, wl_keyboard, wl_pointer, wl_registry, wl_seat,
    wl_surface, wl_touch,
};
use crate::protocols::{AnyEvent, AnyRequest};
use crate::proxy::harness::Session;
use crate::test_support::trace::Tape;
use crate::test_support::{decode, Rng};
use crate::wire::{Outgoing, WL_DISPLAY_ID};

/// The source of a case's choices.
pub(crate) trait Choose {
    /// A choice below `n`.
    fn pick(&mut self, n: u32) -> u32;

    /// A value from `low` to `high`, both included; choice 0 is `low`.
    fn between(&mut self, low: u64, high: u64) -> u64 {
        let span = u32::try_from(high - low + 1).expect("a span that fits a choice");
        low + u64::from(self.pick(span))
    }
}

/// A function of `n` is a source too, for tests that script the choices.
impl<F: FnMut(u32) -> u32> Choose for F {
    fn pick(&mut self, n: u32) -> u32 {
        self(n)
    }
}

/// A case's choices, drawn from a seed and recorded, or replayed from a
/// recording, so that a shrinker can edit and replay them.
pub(crate) struct Choices {
    recorded: Vec<u32>,
    /// The choices to replay; draws past their end are 0.
    replay: Option<Vec<u32>>,
    rng: Rng,
}

impl Choices {
    /// Choices drawn from `seed`.
    pub fn new(seed: u64) -> Self {
        Self {
            recorded: Vec::new(),
            replay: None,
            rng: Rng::new(seed),
        }
    }

    /// Choices that replay `choices`, each taken modulo the number of
    /// options of its draw.
    pub fn replay(choices: Vec<u32>) -> Self {
        Self {
            replay: Some(choices),
            ..Self::new(0)
        }
    }

    /// The choices made so far, in order.
    pub fn recorded(&self) -> &[u32] {
        &self.recorded
    }
}

impl Choose for Choices {
    fn pick(&mut self, n: u32) -> u32 {
        let choice = match &self.replay {
            Some(replay) => replay.get(self.recorded.len()).map_or(0, |c| c % n),
            None => (self.rng.next_u64() % u64::from(n)) as u32,
        };
        self.recorded.push(choice);
        choice
    }
}

/// A FIFO channel whose messages each arrive after a drawn latency, but
/// never before the message ahead of them.
pub(crate) struct Channel<T> {
    /// The longest latency a message draws.
    latency: Micros,
    queue: VecDeque<(Micros, T)>,
}

impl<T> Channel<T> {
    /// A channel whose latencies are drawn from 0 to `latency`.
    pub fn new(latency: Micros) -> Self {
        Self {
            latency,
            queue: VecDeque::new(),
        }
    }

    /// Sends `message` at `now`.
    pub fn send(&mut self, now: Micros, message: T, choices: &mut impl Choose) {
        let drawn = now + Micros(choices.between(0, self.latency.0));
        let ahead = self.queue.back().map_or(drawn, |&(ready, _)| ready);
        self.queue.push_back((drawn.max(ahead), message));
    }

    /// When the message at the head arrives, if there is one.
    pub fn ready_at(&self) -> Option<Micros> {
        self.queue.front().map(|&(ready, _)| ready)
    }

    /// The message at the head.
    ///
    /// # Panics
    ///
    /// If the channel is empty.
    pub fn take(&mut self) -> T {
        self.queue.pop_front().expect("a message in the channel").1
    }
}

/// Which of the sources, each ready at its time or idle (`None`), goes
/// next: the earliest; of several ready at once, a choice among them in
/// the sources' order, so choice 0 is the first.
pub(crate) fn earliest(sources: &[Option<Micros>], choices: &mut impl Choose) -> Option<usize> {
    let first = sources.iter().flatten().min()?;
    let tied: Vec<usize> = (0..sources.len())
        .filter(|&i| sources[i] == Some(*first))
        .collect();
    let pick = match tied.len() {
        1 => 0,
        n => choices.pick(u32::try_from(n).expect("a few sources")) as usize,
    };
    Some(tied[pick])
}

/// The channels' longest latencies (section 4.2).
const GUEST_LATENCY: Micros = Micros(2_000);
const HOST_LATENCY: Micros = Micros(3_000);
const ENGINE_LATENCY: Micros = Micros(1_000);
/// How long the user waits once everything has settled.
const SETTLE: Micros = Micros::from_millis(50);
/// How long a held key waits before it repeats, and then between its
/// repeats: 500 ms, then 20 Hz, ChromeOS's default.
const REPEAT_DELAY: Micros = Micros::from_millis(500);
const REPEAT_INTERVAL: Micros = Micros::from_millis(50);
/// The longest a user holds a key before the next action.
const HOLD: u64 = 1_500;

/// Where a line of a case's trace happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Place {
    /// A guest request reached the proxy.
    CToP,
    /// The proxy sent the guest an event.
    PToC,
    /// The proxy sent the host a request.
    PToE,
    /// A host event reached the proxy.
    EToP,
    /// The engine answered part of a key.
    Engine,
    /// The user acted.
    User,
}

impl std::fmt::Display for Place {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let place = match self {
            Self::CToP => "C→P",
            Self::PToC => "P→C",
            Self::PToE => "P→E",
            Self::EToP => "E→P",
            Self::Engine => "Eng",
            Self::User => "U",
        };
        f.write_str(place)
    }
}

/// A line of a case's trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Line {
    pub at: Micros,
    pub place: Place,
    pub text: String,
}

impl std::fmt::Display for Line {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ms = self.at.0 as f64 / 1_000.0;
        write!(f, "{ms:10.3} {} {}", self.place, self.text)
    }
}

/// What a case's world is made of, beside its choices.
pub(super) struct Setup<'a> {
    pub client: ClientKind,
    /// The input method is a Latin layout, which has no engine.
    pub latin: bool,
    pub lag: Lag,
    /// The guest releases its keyboard and binds a new one once its text
    /// input is active, so the new one hears of the next focus move after
    /// the text input (E12).
    pub late_keyboard: bool,
    /// The fields' initial texts.
    pub texts: &'a [String],
    /// Where the proxy records its wire trace (`crate::trace`), if it does.
    pub wire: Option<&'a Tape>,
}

/// A press the guest got: its surface and the target it lands on.
type Press = (u32, Target);

/// Where the guest's pointer is, as the proxy tells it.
#[derive(Clone, Copy, Debug)]
enum Pointer {
    /// On guest surface `.0`, at x `.1`.
    On(u32, i32),
    /// On a surface the guest destroyed, whose enter the proxy dropped.
    Gone,
}

/// What can happen next in a [`World`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    /// C→P delivers.
    GuestRequests,
    /// P→C delivers.
    GuestEvents,
    /// P→E delivers.
    HostRequests,
    /// E→P delivers.
    HostEvents,
    /// E→Eng delivers.
    EngineInput,
    /// The engine answers.
    EngineAnswers,
    /// A held key repeats.
    Repeat,
    /// The user acts.
    User,
}

impl Source {
    /// Every source, in the order whose position a tie's choice picks
    /// ([`earliest`]): reordering it changes every recorded schedule.
    const ORDER: [Self; 8] = [
        Self::GuestRequests,
        Self::GuestEvents,
        Self::HostRequests,
        Self::HostEvents,
        Self::EngineInput,
        Self::EngineAnswers,
        Self::Repeat,
        Self::User,
    ];
}

/// One case's client, proxy, Exo, engine and display, joined by channels.
pub(super) struct World<'t> {
    now: Micros,
    pub choices: Choices,
    session: Session,
    /// The client model, once the setup created its text input.
    client: Option<Box<dyn Client>>,
    pub exo: Exo,
    engine: Engine,
    display: Display,
    /// The fields' windows on the host.
    windows: Windows,
    /// Each guest request, with the edits the guest had got when it sent
    /// it.
    c_to_p: Channel<(Outgoing, u64)>,
    /// Each event to the guest, the press it is, if it is one: where the
    /// guest's pointer or finger is on which of its surfaces; and whether
    /// it is a key that reaches no window ([`World::keyboard_gone`]).
    p_to_c: Channel<(AnyEvent, Option<Press>, bool)>,
    /// Each run of requests Exo's display dispatches in one read: one
    /// request, or a surrounding text or a `reset` and the barrier glued
    /// to it.
    p_to_e: Channel<Vec<(u32, AnyRequest)>>,
    /// Each host event, what engine text it carries, and how many of the
    /// proxy's surrounding texts Exo had got when it sent the event.
    e_to_p: Channel<(Outgoing, Origin, Option<RegionTruth>, u64)>,
    to_engine: Channel<EngineIn>,
    /// The guest's surrounding texts the proxy sent the host, and those
    /// that reached Exo.
    reports_sent: u64,
    reports_got: u64,
    /// The guest's view of what the client models do not take: the
    /// globals, Shift and where its pointer is, as the proxy tells it.
    globals: BTreeMap<String, (u32, u32)>,
    shift: bool,
    pointer: Option<Pointer>,
    /// The proxy dropped the last keyboard enter Exo sent, which named a
    /// surface the guest destroyed (F2), so the guest's keyboard focus is
    /// on none of its windows: the keys Exo sent with that focus, which
    /// the proxy passes on, reach no window, as they would reach no window
    /// of a client of Exo's own (GTK's and Chromium's seats drop a key
    /// while no surface has the keyboard focus).
    keyboard_gone: bool,
    /// The resets Exo handed the engine.
    resets: u64,
    /// The key the user holds, which repeats, and when it next does.
    repeat: Option<(Key, Micros)>,
    /// The transactions that edit the text (a delete or a commit) the
    /// proxy sent the guest, and those the guest got; and whether the
    /// transaction each is in so far edits.
    edits: Edits,
    /// The guest's text input and compositor.
    text_input: u32,
    compositor: u32,
    /// Which field each surface is, shared with the windows and the
    /// judge.
    surfaces: Surfaces,
    /// Tier 1, from the end of the setup.
    pub judge: Option<Judge>,
    /// The proxy's requests that change what Exo's text input serves
    /// that Exo has handled ([`Origin::requests`]).
    requests: u64,
    /// The first answered syncs at which an exact Exo mirror of the proxy's
    /// differed from Exo's tracker ([`World::check_mirrors`]).
    pub mirrors: Vec<String>,
    /// The proxy ended the session.
    pub ended: bool,
    /// The client's fields at each `Settle`, in order, for the no-loss
    /// check ([`oracle::lost`]).
    pub settles: Vec<Vec<TextModel>>,
    trace: &'t mut Vec<Line>,
}

impl<'t> World<'t> {
    /// A world set up as `setup` says, its every draw from `choices`, its
    /// trace in `trace`: the guest has bound the globals, created the
    /// fields' surfaces, a keyboard, a pointer, a touch and a text input,
    /// and the keyboard focus is on field 0; with a late keyboard, it has
    /// then bound its keyboard again. If the proxy ends the session
    /// on the way, the world stops there, [`World::ended`].
    pub async fn new(setup: &Setup<'_>, choices: Choices, trace: &'t mut Vec<Line>) -> World<'t> {
        let mut exo = Exo::default();
        exo.latin = setup.latin;
        let surfaces = Surfaces::default();
        let mut world = World {
            now: Micros(0),
            choices,
            session: Session::new(),
            client: None,
            exo,
            engine: Engine::new(setup.lag),
            display: Display::new(display::exo_globals())
                .unanswered(&[zcr_extended_keyboard_v1::NAME]),
            windows: Windows::new(surfaces.clone()),
            c_to_p: Channel::new(GUEST_LATENCY),
            p_to_c: Channel::new(GUEST_LATENCY),
            p_to_e: Channel::new(HOST_LATENCY),
            e_to_p: Channel::new(HOST_LATENCY),
            to_engine: Channel::new(ENGINE_LATENCY),
            globals: BTreeMap::new(),
            shift: false,
            pointer: None,
            keyboard_gone: false,
            reports_sent: 0,
            reports_got: 0,
            resets: 0,
            repeat: None,
            edits: Edits::default(),
            text_input: 0,
            compositor: 0,
            surfaces,
            judge: None,
            requests: 0,
            mirrors: Vec::new(),
            ended: false,
            settles: Vec::new(),
            trace,
        };
        if let Some(wire) = setup.wire {
            world.session.record(wire.clone());
        }
        let registry = world.session.new_id();
        world.guest([wl_display::req::get_registry(WL_DISPLAY_ID, registry)]);
        world.run(&[]).await;
        if world.ended {
            return world;
        }
        let [compositor, seat, manager] = [
            wl_compositor::NAME,
            wl_seat::NAME,
            zwp_text_input_manager_v3::NAME,
        ]
        .map(|interface| {
            let (name, version) = world.globals[interface];
            let id = world.session.new_id();
            let bind = wl_registry::req::bind(registry, name, interface, version, id);
            world.guest([bind.expect("a bind fits a message")]);
            id
        });
        world.compositor = compositor;
        for _ in setup.texts {
            let surface = world.session.new_id();
            world.surfaces.add_field(surface);
            world.guest([wl_compositor::req::create_surface(compositor, surface)]);
        }
        let [keyboard, pointer, touch, text_input] = [(); 4].map(|_| world.session.new_id());
        world.guest([
            wl_seat::req::get_keyboard(seat, keyboard),
            wl_seat::req::get_pointer(seat, pointer),
            wl_seat::req::get_touch(seat, touch),
            zwp_text_input_manager_v3::req::get_text_input(manager, text_input, seat),
        ]);
        world.text_input = text_input;
        world.run(&[]).await;
        if world.ended {
            return world;
        }
        let fields = (0..setup.texts.len())
            .map(|field| world.surfaces.guest(field))
            .zip(setup.texts)
            .map(|(surface, text)| Field::new(surface, TextModel::new(text)))
            .collect();
        world.client = Some(match setup.client {
            ClientKind::Chromium => Box::new(Chromium::new(text_input, fields)),
            ClientKind::Gtk3 => Box::new(Gtk::new(false, text_input, fields)),
            ClientKind::Gtk4 => Box::new(Gtk::new(true, text_input, fields)),
        });
        world.judge = Some(Judge::new(world.surfaces.clone(), setup.texts));
        world.act(UserAction::Focus(0));
        world.run(&[]).await;
        if setup.late_keyboard && !world.ended {
            let late = world.session.new_id();
            // The new keyboard first, and the old one's release once it
            // has entered, so the seat keeps its focus.
            world.guest([wl_seat::req::get_keyboard(seat, late)]);
            world.run(&[]).await;
            world.guest([wl_keyboard::req::release(keyboard)]);
            world.run(&[]).await;
        }
        world
    }

    /// The engine's composition.
    pub fn engine_composition(&self) -> String {
        self.engine.composition()
    }

    /// The resets Exo handed the engine.
    pub fn engine_resets(&self) -> u64 {
        self.resets
    }

    /// The client model.
    pub fn client(&self) -> &dyn Client {
        self.client.as_deref().expect("a client model")
    }

    /// Plays `actions`, each at its time, and then delivers until every
    /// channel is empty and the engine idle. Each action but a `Settle`
    /// comes a drawn gap after the one before: 30 to 300 ms, as in normal
    /// typing, or, half the time, 0 to 20 ms, a burst. A `Settle` waits
    /// for everything to settle, and the next action comes 50 ms later.
    /// Stops early if the proxy ends the session.
    pub async fn run(&mut self, actions: &[UserAction]) {
        let mut actions = actions.iter().copied().peekable();
        let mut next = self.now;
        while !self.ended {
            let user = match actions.peek() {
                Some(UserAction::Settle) if self.busy() => None,
                Some(_) => Some(next.max(self.now)),
                None => None,
            };
            // A held key repeats until it is up, which no settle waits for,
            // or until the case ends, as a shrunk one may before its release.
            if actions.peek().is_none() {
                self.repeat = None;
            }
            let ready = Source::ORDER.map(|source| match source {
                Source::User => user,
                source => self.ready_at(source),
            });
            let Some(source) = earliest(&ready, &mut self.choices) else {
                return;
            };
            self.now = self.now.max(ready[source].expect("a ready source"));
            match Source::ORDER[source] {
                Source::GuestRequests => {
                    let (request, edits) = self.c_to_p.take();
                    self.deliver_guest_request(request, edits).await;
                }
                Source::GuestEvents => {
                    let (event, press, gone) = self.p_to_c.take();
                    self.deliver_to_guest(event, press, gone);
                }
                Source::HostRequests => {
                    for (sender, request) in self.p_to_e.take() {
                        self.reports_got += u64::from(is_report(&request));
                        self.windows.request(sender, &request, &mut self.exo);
                        let events = self.display.request(sender, &request, &mut self.exo);
                        self.host_sends(events, false);
                        // What Exo sent as it handled the request is of the
                        // state before it.
                        self.requests += u64::from(oracle::changes_service(&request));
                    }
                }
                Source::HostEvents => {
                    let (event, origin, truth, reports) = self.e_to_p.take();
                    self.deliver_host_event(event, origin, truth, reports).await;
                }
                Source::EngineInput => {
                    let input = self.to_engine.take();
                    self.engine.send(self.now, input, &mut self.choices);
                }
                Source::EngineAnswers => self.engine_answers(),
                Source::Repeat => {
                    let (key, at) = self.repeat.expect("a held key");
                    self.repeat = Some((key, at + REPEAT_INTERVAL));
                    self.log(Place::User, format!("repeat {key:?}"));
                    let handed = self.exo.autorepeat(self.now, key);
                    if let Some(judge) = &mut self.judge {
                        judge.counts.repeats += u64::from(handed);
                    }
                    self.host_sends(Vec::new(), false);
                }
                Source::User => {
                    let action = actions.next().expect("an action");
                    self.act(action);
                    next = self.now
                        + match action {
                            UserAction::Settle => SETTLE,
                            UserAction::KeyDown(_) => {
                                Micros::from_millis(self.choices.between(0, HOLD))
                            }
                            _ if self.choices.pick(2) == 0 => {
                                Micros::from_millis(self.choices.between(30, 300))
                            }
                            _ => Micros::from_millis(self.choices.between(0, 20)),
                        };
                }
            }
        }
    }

    /// When `source` is next ready, `None` while it is idle; the user's
    /// next action is [`World::run`]'s to time, and is `None` here.
    fn ready_at(&self, source: Source) -> Option<Micros> {
        match source {
            Source::GuestRequests => self.c_to_p.ready_at(),
            Source::GuestEvents => self.p_to_c.ready_at(),
            Source::HostRequests => self.p_to_e.ready_at(),
            Source::HostEvents => self.e_to_p.ready_at(),
            Source::EngineInput => self.to_engine.ready_at(),
            Source::EngineAnswers => self.engine.ready_at(),
            Source::Repeat => self.repeat.map(|(_, at)| at),
            Source::User => None,
        }
    }

    /// Whether a channel or the engine has something to do, which a
    /// `Settle` waits for; a held key's repeats it does not.
    fn busy(&self) -> bool {
        let busy = |&source: &Source| source != Source::Repeat && self.ready_at(source).is_some();
        Source::ORDER.iter().any(busy)
    }

    /// The user acts: Exo takes a key's press and release; a click or a
    /// tap focuses its field's surface, as a click on a window does, and
    /// then presses there; a focus move moves the seat's focus; a restart
    /// destroys the input method, which Exo replaces; a suggestion or a
    /// reconversion is a request of the engine's.
    fn act(&mut self, action: UserAction) {
        self.log(Place::User, format!("{action:?}"));
        let time = (self.now.0 / 1_000) as u32;
        let field = match action {
            UserAction::Click { field, .. } | UserAction::Tap { field, .. } => field,
            UserAction::Focus(field) => field,
            UserAction::Key(key) => {
                for state in [
                    wl_keyboard::KEY_STATE_PRESSED,
                    wl_keyboard::KEY_STATE_RELEASED,
                ] {
                    self.exo.key(self.now, key, state);
                }
                // The key the user typed last repeats, and it is up.
                self.repeat = None;
                return self.host_sends(Vec::new(), false);
            }
            UserAction::KeyDown(key) => {
                self.exo.key(self.now, key, wl_keyboard::KEY_STATE_PRESSED);
                self.repeat = Some((key, self.now + REPEAT_DELAY));
                return self.host_sends(Vec::new(), false);
            }
            UserAction::KeyUp(key) => {
                self.exo.key(self.now, key, wl_keyboard::KEY_STATE_RELEASED);
                self.repeat = self.repeat.filter(|&(repeating, _)| repeating != key);
                return self.host_sends(Vec::new(), false);
            }
            UserAction::RestartInputMethod => {
                self.exo.destroy_input_method();
                return self.host_sends(Vec::new(), false);
            }
            UserAction::AcceptSuggestion | UserAction::Reconvert => {
                self.exo.ask(match action {
                    UserAction::AcceptSuggestion => Request::AcceptSuggestion,
                    _ => Request::Reconvert,
                });
                return self.host_sends(Vec::new(), false);
            }
            UserAction::Reopen(field) => return self.reopen(field),
            UserAction::Settle => {
                let fields = self.client().fields().iter();
                let fields = fields.map(|field| field.model.clone()).collect();
                return self.settles.push(fields);
            }
        };
        // A window that is opening again, after a replay or a shrink drops
        // the `Settle` behind its reopen, cannot be clicked yet.
        let Some(surface) = self.surfaces.host(field) else {
            return;
        };
        if self.exo.focus() != Some(surface) {
            let before_keyboard = self.exo.text_inputs_hear_first();
            if let Some(judge) = self.judge.as_mut().filter(|_| before_keyboard) {
                judge.counts.detaches_before_keyboard += 1;
            }
            self.exo.move_focus(Some(surface));
        }
        let presses = match action {
            UserAction::Click { at, .. } => {
                let pointer = *self.display.pointers().first().expect("a pointer");
                self.display
                    .click(pointer, surface, keymap::position(at), time)
            }
            UserAction::Tap { at, .. } => {
                let touch = *self.display.touches().first().expect("a touch");
                self.display.tap(touch, surface, keymap::position(at), time)
            }
            _ => Vec::new(),
        };
        self.host_sends(presses, false);
    }

    /// The client closes `field`'s window and opens it again (F2): it
    /// destroys the field's surface and creates another, and the display
    /// gives the new one the seat's focus if the old one had it.
    fn reopen(&mut self, field: usize) {
        let old = self.surfaces.guest(field);
        let new = self.session.new_id();
        let refocus = self.exo.focus().is_some() && self.exo.focus() == self.surfaces.host(field);
        self.windows.reopen(field, refocus);
        self.surfaces.add_guest(field, new);
        let now = self.now;
        let client = self.client.as_mut().expect("a client model");
        let mut requests = client.reopen(now, field, new);
        requests.push(wl_surface::req::destroy(old));
        requests.push(wl_compositor::req::create_surface(self.compositor, new));
        requests.push(wl_surface::req::commit(new));
        self.guest(requests);
    }

    /// Exo's events since its last input, then `events`, go to the proxy,
    /// and what Exo handed the engine goes to the engine. Exo's events carry
    /// a late answer if `late`, made before the resets this input hands the
    /// engine.
    fn host_sends(&mut self, events: Vec<Outgoing>, late: bool) {
        self.host_sends_of(events, late, (None, None));
    }

    /// [`World::host_sends`], of Exo's answer to a press of `press` or an
    /// autorepeat of `repeat`.
    fn host_sends_of(
        &mut self,
        events: Vec<Outgoing>,
        late: bool,
        (press, repeat): (Option<Key>, Option<Key>),
    ) {
        let origin = Origin {
            late,
            resets: self.resets,
            press,
            repeat,
            requests: self.requests,
            stale_held: self.exo.holds_stale_key(),
        };
        let reports = self.reports_got;
        for (event, truth) in self.exo.take_events_with_truths() {
            let sent = (event, origin, truth, reports);
            self.e_to_p.send(self.now, sent, &mut self.choices);
        }
        for event in events {
            let sent = (event, Origin::default(), None, reports);
            self.e_to_p.send(self.now, sent, &mut self.choices);
        }
        for input in self.exo.take_engine() {
            self.resets += u64::from(input == EngineIn::Reset);
            self.to_engine.send(self.now, input, &mut self.choices);
        }
    }

    /// The engine answers part of the key in service, and Exo applies it.
    /// The engine learns the text of a region it reconverted.
    fn engine_answers(&mut self) {
        let answer = self.engine.take(&mut self.choices);
        self.log(Place::Engine, format!("{answer:?}"));
        let keys = (self.exo.press_of(&answer), self.exo.repeat_of(&answer));
        let verdict = self.exo.answer(&answer);
        let outcome = self.exo.take_region_outcome();
        let refused = outcome == Some(RegionOutcome::Refused);
        match outcome {
            Some(RegionOutcome::Composed(text)) => self.engine.learn_region(text),
            Some(RegionOutcome::Refused) => self.engine.refused_region(),
            None => {}
        }
        if let Some(judge) = &mut self.judge {
            judge.counts.regions_refused += u64::from(refused);
            judge.counts.late += u64::from(verdict == Answer::Late);
            judge.counts.dropped += u64::from(verdict == Answer::Dropped);
        }
        self.host_sends_of(Vec::new(), verdict == Answer::Late, keys);
    }

    /// The guest sends `requests` at once, on C→P.
    fn guest(&mut self, requests: impl IntoIterator<Item = Outgoing>) {
        for request in requests {
            let edits = self.edits.got.0;
            self.c_to_p
                .send(self.now, (request, edits), &mut self.choices);
        }
    }

    /// A guest request, sent once the guest had got `edits` edits,
    /// reaches the proxy. A surrounding text sent before the guest got an
    /// edit the proxy has sent crossed it (P4).
    async fn deliver_guest_request(&mut self, request: Outgoing, edits: u64) {
        let text_input = self.text_input;
        let decoded = decode(&request.0, &mut VecDeque::new(), |msg| {
            match msg.sender_id == text_input {
                true => zwp_text_input_v3::Request::from_wire(msg),
                false => Err(crate::wire::ProtocolError::InvalidSender(msg.sender_id)),
            }
        });
        match &decoded {
            Ok((_, request)) => {
                self.log(Place::CToP, format!("{request:?}"));
                let report = matches!(
                    request,
                    zwp_text_input_v3::Request::SetSurroundingText { .. }
                );
                if let Some(judge) = self.judge.as_mut().filter(|_| report) {
                    let crossed = edits < self.edits.sent.0;
                    judge.counts.crossed_reports += u64::from(crossed);
                }
                self.see(Seen::Guest(request));
            }
            Err(_) => self.log(Place::CToP, "a setup request".to_owned()),
        }
        // A field's surface the guest destroys (F2).
        let surfaces = self.surfaces.clone();
        let destroyed = decode(&request.0, &mut VecDeque::new(), |msg| {
            let surface = msg.sender_id;
            match surfaces.field_of_guest(surface) {
                Some(field) => wl_surface::Request::from_wire(msg).map(|r| (surface, field, r)),
                None => Err(crate::wire::ProtocolError::InvalidSender(surface)),
            }
        });
        if let Ok((_, (surface, field, wl_surface::Request::Destroy {}))) = destroyed {
            self.log(
                Place::CToP,
                format!("the surface of field {field} destroyed"),
            );
            self.surfaces.destroy(surface);
            self.see(Seen::SurfaceDestroyed { field });
        }
        let goes_on = self.session.try_guest([request]).await;
        self.send_proxy_output(goes_on);
    }

    /// A host event, which Exo sent once it had got `reports` of the
    /// proxy's surrounding texts, reaches the proxy. A region that a later
    /// report the proxy has sent did not reach in time crossed it.
    async fn deliver_host_event(
        &mut self,
        event: Outgoing,
        origin: Origin,
        truth: Option<RegionTruth>,
        reports: u64,
    ) {
        let (sender, decoded) = self.session.decode_host_event(&event);
        let region = matches!(
            decoded,
            AnyEvent::ZcrExtendedTextInputV1(extended::Event::SetPreeditRegion { .. })
        );
        if let Some(judge) = self.judge.as_mut().filter(|_| region) {
            judge.counts.regions_crossing_reports += u64::from(self.reports_sent > reports);
        }
        // The proxy drops a pointer enter that names a surface the guest
        // destroyed, so the guest's pointer is on none of its surfaces.
        if let AnyEvent::WlPointer(wl_pointer::Event::Enter { surface, .. }) = decoded {
            if self.surfaces.host_destroyed(surface) {
                self.pointer = Some(Pointer::Gone);
            }
        }
        // And a keyboard enter, so the keys that follow reach no window.
        if let AnyEvent::WlKeyboard(wl_keyboard::Event::Enter { surface, .. }) = decoded {
            self.keyboard_gone = self.surfaces.host_destroyed(surface);
        }
        let note = if origin.late { " (late)" } else { "" };
        self.log(Place::EToP, format!("{sender} {}{note}", shown(&decoded)));
        self.see(Seen::Host {
            sender,
            event: &decoded,
            origin,
            region: truth.as_ref(),
        });
        let goes_on = self.session.try_host([event]).await;
        if let AnyEvent::WlCallback(wl_callback::Event::Done { .. }) = decoded {
            self.check_mirrors(sender);
        }
        self.send_proxy_output(goes_on);
    }

    /// The proxy has handled the `done` of the sync with callback
    /// `callback`: each of its Exo mirrors that says it is exact must hold
    /// what Exo's tracker did when the display answered the sync, as every
    /// request before the sync and every event before its `done` has
    /// reached it, and nothing after them has.
    fn check_mirrors(&mut self, callback: u32) {
        let Some(at) = self.exo.synced.iter().position(|(c, _)| *c == callback) else {
            return;
        };
        let (_, trackers) = self.exo.synced.remove(at);
        for (id, tracker) in trackers {
            let state = |state: &Predicted| {
                let selection = (state.selection.start, state.selection.end);
                (
                    state.text.clone(),
                    selection,
                    state.composition.clone().unwrap_or(0..0),
                )
            };
            let predicted = state(&tracker.predicted);
            let history: Vec<_> = tracker.expected.iter().map(state).collect();
            // The mirror is the proxy's own state, which no peer sees.
            let ime = &self.session.ctx().ime;
            let Some(mirror) = ime.mirror_differs_for_test(id, &predicted, &history) else {
                continue;
            };
            if self.mirrors.len() < 3 {
                let at = self.now.0 as f64 / 1_000.0;
                self.mirrors.push(format!(
                    "{at:.3} ms, at the done of sync {callback}: the exact mirror of host text \
                     input {id} holds {mirror}, Exo's tracker {:?}",
                    (predicted, history)
                ));
            }
        }
    }

    /// What the proxy sent goes out on P→E and P→C. A surrounding text or a
    /// `reset` and the barrier right after it go together, as one
    /// submission, which Exo's display dispatches in one read
    /// (`HostOut::Glued`).
    fn send_proxy_output(&mut self, goes_on: bool) {
        let mut batch: Vec<(u32, AnyRequest)> = Vec::new();
        for (sender, request) in self.session.host_received() {
            self.log(Place::PToE, format!("{sender} {request:?}"));
            self.reports_sent += u64::from(is_report(&request));
            self.see(Seen::ToHost(&request));
            let glued = matches!(
                request,
                AnyRequest::WlDisplay(wl_display::Request::Sync { .. })
            ) && batch
                .last()
                .is_some_and(|(_, last)| is_report(last) || is_reset(last));
            if !glued && !batch.is_empty() {
                let run = std::mem::take(&mut batch);
                self.p_to_e.send(self.now, run, &mut self.choices);
            }
            batch.push((sender, request));
        }
        if !batch.is_empty() {
            self.p_to_e.send(self.now, batch, &mut self.choices);
        }
        for (sender, event) in self.session.guest_received() {
            self.log(Place::PToC, format!("{sender} {}", shown(&event)));
            self.edits.sent.follow(&event);
            self.see(Seen::ToGuest(&event));
            let press = self.press(&event);
            if let Some((surface, at)) = press {
                self.see(Seen::GuestPress { surface, at });
            }
            let gone = self.keyboard_gone
                && matches!(event, AnyEvent::WlKeyboard(wl_keyboard::Event::Key { .. }));
            self.p_to_c
                .send(self.now, (event, press, gone), &mut self.choices);
        }
        self.see(Seen::StepEnd);
        self.ended |= !goes_on;
    }

    /// The press `event` to the guest is, if it is one: its surface and
    /// target. Follows the guest's pointer.
    fn press(&mut self, event: &AnyEvent) -> Option<Press> {
        let target = |x| keymap::target(x).expect("a press at a target");
        match *event {
            AnyEvent::WlPointer(wl_pointer::Event::Enter {
                surface, surface_x, ..
            }) => self.pointer = Some(Pointer::On(surface, surface_x)),
            AnyEvent::WlPointer(wl_pointer::Event::Motion { surface_x, .. }) => {
                if let Some(Pointer::On(_, x)) = &mut self.pointer {
                    *x = surface_x;
                }
            }
            AnyEvent::WlPointer(wl_pointer::Event::Leave { .. }) => self.pointer = None,
            AnyEvent::WlPointer(wl_pointer::Event::Button { state, .. })
                if state == wl_pointer::BUTTON_STATE_PRESSED =>
            {
                // A press on a surface the guest destroyed lands nowhere.
                match self.pointer.expect("a press on a surface") {
                    Pointer::On(surface, x) if !self.surfaces.guest_destroyed(surface) => {
                        return Some((surface, target(x)));
                    }
                    Pointer::On(..) | Pointer::Gone => return None,
                }
            }
            AnyEvent::WlTouch(wl_touch::Event::Down { surface, x, .. }) => {
                return Some((surface, target(x)));
            }
            _ => {}
        }
        None
    }

    /// An event reaches the guest: the client model takes what it models,
    /// a press as `press` resolved it, but no key that is `gone` to no
    /// window, and the guest keeps the globals and Shift.
    ///
    /// # Panics
    ///
    /// If a key gone to no window reaches a client with the keyboard
    /// focus: the proxy kept from it the `leave` that came before the enter
    /// it dropped.
    fn deliver_to_guest(&mut self, event: AnyEvent, press: Option<Press>, gone: bool) {
        let now = self.now;
        self.edits.got.follow(&event);
        let key = matches!(event, AnyEvent::WlKeyboard(wl_keyboard::Event::Key { .. }));
        if let Some(judge) = self.judge.as_mut().filter(|_| key && gone) {
            judge.counts.keys_to_no_window += 1;
        }
        if let AnyEvent::WlRegistry(wl_registry::Event::Global {
            name,
            interface,
            version,
        }) = event
        {
            self.globals.insert(interface, (name, version));
            return;
        }
        let Some(client) = self.client.as_mut() else {
            return;
        };
        let requests = match (event, press) {
            (AnyEvent::WlPointer(_), Some((surface, at))) => {
                client.press(now, surface, at, PressKind::Button)
            }
            (AnyEvent::WlTouch(_), Some((surface, at))) => {
                client.press(now, surface, at, PressKind::Touch)
            }
            (AnyEvent::WlPointer(wl_pointer::Event::Button { .. }), None)
            | (AnyEvent::WlTouch(wl_touch::Event::Up { .. }), None) => client.release(now, false),
            (AnyEvent::ZwpTextInputV3(event), _) => client.event(now, event),
            (AnyEvent::WlDisplay(wl_display::Event::DeleteId { id }), _) => {
                client.deleted(id);
                Vec::new()
            }
            (AnyEvent::WlKeyboard(wl_keyboard::Event::Enter { surface, .. }), _) => {
                client.keyboard_focus(now, Some(surface))
            }
            (AnyEvent::WlKeyboard(wl_keyboard::Event::Leave { .. }), _) => {
                client.keyboard_focus(now, None)
            }
            (AnyEvent::WlKeyboard(wl_keyboard::Event::Modifiers { mods_depressed, .. }), _) => {
                self.shift = mods_depressed & keymap::SHIFT != 0;
                Vec::new()
            }
            (AnyEvent::WlKeyboard(wl_keyboard::Event::Key { .. }), _) if gone => {
                assert_eq!(
                    client.focused_field(),
                    None,
                    "protocol violation: a key to a destroyed surface while a field has the \
                     keyboard focus"
                );
                Vec::new()
            }
            (AnyEvent::WlKeyboard(wl_keyboard::Event::Key { key, state, .. }), _)
                if state == wl_keyboard::KEY_STATE_PRESSED =>
            {
                match keymap::key(key, self.shift) {
                    Some(key) => client.key(now, key),
                    None => Vec::new(),
                }
            }
            _ => Vec::new(),
        };
        self.guest(requests);
    }

    /// The judge follows `seen`, and checks each step's output.
    fn see(&mut self, seen: Seen<'_>) {
        if let Some(judge) = &mut self.judge {
            judge.see(seen, self.now, &self.exo);
        }
    }

    fn log(&mut self, place: Place, text: String) {
        self.trace.push(Line {
            at: self.now,
            place,
            text,
        });
    }
}

/// Whether `request` to the host reports the guest's surrounding text.
fn is_report(request: &AnyRequest) -> bool {
    use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
    matches!(
        request,
        AnyRequest::ZwpTextInputV1(zwp_text_input_v1::Request::SetSurroundingText { .. })
    )
}

fn is_reset(request: &AnyRequest) -> bool {
    use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
    matches!(
        request,
        AnyRequest::ZwpTextInputV1(zwp_text_input_v1::Request::Reset {})
    )
}

/// `event` as a trace shows it: a keymap without its descriptor, whose
/// number a replay need not repeat.
fn shown(event: &AnyEvent) -> String {
    match event {
        AnyEvent::WlKeyboard(wl_keyboard::Event::Keymap { format, size, .. }) => {
            format!("WlKeyboard(Keymap {{ format: {format}, size: {size} }})")
        }
        event => format!("{event:?}"),
    }
}

/// The transactions that edit the text, which the proxy sent the guest
/// (`sent`) and the guest got (`got`).
#[derive(Default)]
struct Edits {
    sent: EditCount,
    got: EditCount,
}

/// A count of the transactions that edit the text, and whether the one in
/// progress does.
#[derive(Default)]
struct EditCount(u64, bool);

impl EditCount {
    /// Follows `event`, an event to the guest.
    fn follow(&mut self, event: &AnyEvent) {
        use zwp_text_input_v3::Event;
        match event {
            AnyEvent::ZwpTextInputV3(Event::DeleteSurroundingText { .. }) => self.1 = true,
            AnyEvent::ZwpTextInputV3(Event::CommitString { text }) => {
                self.1 |= text.as_ref().is_some_and(|text| !text.is_empty());
            }
            AnyEvent::ZwpTextInputV3(Event::Done { .. }) => {
                self.0 += u64::from(std::mem::take(&mut self.1));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_replay_what_they_recorded_and_zero_past_the_end() {
        let mut drawn = Choices::new(7);
        let first: Vec<u32> = (0..20).map(|_| drawn.pick(5)).collect();
        assert!(first.iter().any(|&c| c != 0) && first.iter().all(|&c| c < 5));
        assert_eq!(drawn.recorded(), first);
        let mut replayed = Choices::replay(first.clone());
        let again: Vec<u32> = (0..20).map(|_| replayed.pick(5)).collect();
        assert_eq!(again, first);
        // A replayed choice is taken modulo the draw's options, and a draw
        // past the recording is 0, the most sequential.
        let mut edited = Choices::replay(vec![7]);
        assert_eq!((edited.pick(3), edited.pick(3)), (1, 0));
    }

    #[test]
    fn a_channel_keeps_its_order_whatever_the_latencies() {
        let mut channel = Channel::new(Micros(100));
        // The first draws the longest latency and the second none, which
        // would overtake it on a channel that is not FIFO.
        let mut latencies = [100, 0].into_iter();
        let mut choices = |_| latencies.next().unwrap();
        channel.send(Micros(0), 'a', &mut choices);
        channel.send(Micros(10), 'b', &mut choices);
        assert_eq!(channel.ready_at(), Some(Micros(100)));
        assert_eq!(channel.take(), 'a');
        assert_eq!(channel.ready_at(), Some(Micros(100)));
        assert_eq!(channel.take(), 'b');
        assert_eq!(channel.ready_at(), None);
    }

    #[test]
    fn the_earliest_source_goes_and_a_choice_breaks_a_tie() {
        let sources = [None, Some(Micros(5)), Some(Micros(3)), Some(Micros(3))];
        let mut never = |_| panic!("no tie to break");
        assert_eq!(earliest(&sources[..2], &mut never), Some(1));
        assert_eq!(earliest(&sources, &mut |n| n - 1), Some(3));
        assert_eq!(earliest(&sources, &mut |_| 0), Some(2));
        assert_eq!(earliest(&[None, None], &mut never), None);
    }
}
