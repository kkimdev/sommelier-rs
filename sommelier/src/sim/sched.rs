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
use super::display::Display;
use super::editor::{Target, TextModel, UserAction};
use super::engine::{Engine, EngineIn, EngineOut, Lag, Request, TextAction};
use super::exo::{Answer, Exo};
use super::hangul::Key;
use super::keymap;
use super::oracle::{self, Origin, Seen, Shadow, SpecText};
use super::ClientKind;
use crate::protocols::text_input_unstable_v3::{zwp_text_input_manager_v3, zwp_text_input_v3};
use crate::protocols::wayland::{
    wl_compositor, wl_display, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_touch,
};
use crate::protocols::{AnyEvent, AnyRequest};
use crate::proxy::harness::Session;
use crate::test_support::{decode, Rng};
use crate::wire::{Outgoing, WL_DISPLAY_ID};

/// The source of a case's choices.
pub(super) trait Choose {
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
pub(super) struct Choices {
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
pub(super) struct Channel<T> {
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
pub(super) fn earliest(sources: &[Option<Micros>], choices: &mut impl Choose) -> Option<usize> {
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
/// repeats: 500 ms, then 33 Hz (the design's section 4.4).
const REPEAT_DELAY: Micros = Micros::from_millis(500);
const REPEAT_INTERVAL: Micros = Micros(30_303);
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
    /// The text-input extension's version, if the host has one.
    pub extension: Option<u32>,
    pub lag: Lag,
    /// The fields' initial texts.
    pub texts: &'a [String],
}

/// A press the guest got: its surface and the target it lands on.
type Press = (u32, Target);

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
    /// Each guest request, with the edits the guest had got when it sent
    /// it.
    c_to_p: Channel<(Outgoing, u64)>,
    /// Each event to the guest, and the press it is, if it is one: where
    /// the guest's pointer or finger is on which of its surfaces.
    p_to_c: Channel<(u32, AnyEvent, Option<Press>)>,
    p_to_e: Channel<(u32, AnyRequest)>,
    /// Each host event, and what engine text it carries.
    e_to_p: Channel<(Outgoing, Origin)>,
    to_engine: Channel<EngineIn>,
    /// The guest's view of what the client models do not take: the
    /// globals, Shift and where its pointer is, as the proxy tells it.
    globals: BTreeMap<String, (u32, u32)>,
    shift: bool,
    pointer: Option<(u32, i32)>,
    /// The resets Exo handed the engine.
    resets: u64,
    /// The key the user holds, which repeats, and when it next does.
    repeat: Option<(Key, Micros)>,
    /// The transactions that edit the text (a delete or a commit) the
    /// proxy sent the guest, and those the guest got; and whether the
    /// transaction each is in so far edits.
    edits: Edits,
    /// The guest's text input.
    text_input: u32,
    /// The fields' guest surfaces.
    surfaces: Vec<u32>,
    /// The oracle's two readers, from the end of the setup.
    pub spec: Option<SpecText>,
    pub shadow: Option<Shadow>,
    /// The first steps whose output tier 1 failed ([`oracle::step`]).
    pub steps: Vec<String>,
    /// The proxy ended the session.
    pub ended: bool,
    /// The host's text-input extension, if it has one.
    extension: Option<u32>,
    /// For each restart of the input method, whether Exo's detach confirms
    /// no composition there: below v11 of the extension it confirms only
    /// one it tracks (E6), and it tracked none.
    pub untracked_restarts: Vec<bool>,
    trace: &'t mut Vec<Line>,
}

impl<'t> World<'t> {
    /// A world set up as `setup` says, its every draw from `choices`, its
    /// trace in `trace`: the guest has bound the globals, created the
    /// fields' surfaces, a keyboard, a pointer, a touch and a text input,
    /// and the keyboard focus is on field 0. If the proxy ends the session
    /// on the way, the world stops there, [`World::ended`].
    pub async fn new(setup: &Setup<'_>, choices: Choices, trace: &'t mut Vec<Line>) -> World<'t> {
        let mut exo = Exo::default();
        exo.latin = setup.latin;
        let mut world = World {
            now: Micros(0),
            choices,
            session: Session::new(),
            client: None,
            exo,
            engine: Engine::new(setup.lag),
            display: Display::new(setup.extension),
            c_to_p: Channel::new(GUEST_LATENCY),
            p_to_c: Channel::new(GUEST_LATENCY),
            p_to_e: Channel::new(HOST_LATENCY),
            e_to_p: Channel::new(HOST_LATENCY),
            to_engine: Channel::new(ENGINE_LATENCY),
            globals: BTreeMap::new(),
            shift: false,
            pointer: None,
            resets: 0,
            repeat: None,
            edits: Edits::default(),
            text_input: 0,
            surfaces: Vec::new(),
            spec: None,
            shadow: None,
            steps: Vec::new(),
            ended: false,
            extension: setup.extension,
            untracked_restarts: Vec::new(),
            trace,
        };
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
        world.surfaces = setup.texts.iter().map(|_| world.session.new_id()).collect();
        for surface in world.surfaces.clone() {
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
        let fields = world
            .surfaces
            .iter()
            .zip(setup.texts)
            .map(|(&surface, text)| Field {
                surface,
                model: TextModel::new(text),
            })
            .collect();
        world.client = Some(match setup.client {
            ClientKind::Chromium => Box::new(Chromium::new(text_input, fields)),
            ClientKind::Gtk3 => Box::new(Gtk::new(false, text_input, fields)),
            ClientKind::Gtk4 => Box::new(Gtk::new(true, text_input, fields)),
        });
        world.spec = Some(SpecText::new(world.display.surfaces(), setup.texts));
        world.shadow = Some(Shadow::new(&world.surfaces, setup.texts));
        world.act(UserAction::Focus(0));
        world.run(&[]).await;
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
            let idle = [
                self.c_to_p.ready_at(),
                self.p_to_c.ready_at(),
                self.p_to_e.ready_at(),
                self.e_to_p.ready_at(),
                self.to_engine.ready_at(),
                self.engine.ready_at(),
            ];
            let user = match actions.peek() {
                Some(UserAction::Settle) if idle.iter().any(Option::is_some) => None,
                Some(_) => Some(next.max(self.now)),
                None => None,
            };
            // A held key repeats until it is up, which no settle waits for.
            let mut sources = idle.to_vec();
            sources.push(self.repeat.map(|(_, at)| at));
            sources.push(user);
            let Some(source) = earliest(&sources, &mut self.choices) else {
                return;
            };
            self.now = self.now.max(sources[source].expect("a ready source"));
            match source {
                0 => {
                    let (request, edits) = self.c_to_p.take();
                    self.deliver_guest_request(request, edits).await;
                }
                1 => {
                    let (sender, event, press) = self.p_to_c.take();
                    self.deliver_to_guest(sender, event, press);
                }
                2 => {
                    let (sender, request) = self.p_to_e.take();
                    let events = self.display.request(sender, &request, &mut self.exo);
                    self.host_sends(events, false);
                }
                3 => {
                    let (event, origin) = self.e_to_p.take();
                    self.deliver_host_event(event, origin).await;
                }
                4 => {
                    let input = self.to_engine.take();
                    self.engine.send(self.now, input, &mut self.choices);
                }
                5 => self.engine_answers(),
                6 => {
                    let (key, at) = self.repeat.expect("a held key");
                    self.repeat = Some((key, at + REPEAT_INTERVAL));
                    self.log(Place::User, format!("repeat {key:?}"));
                    let handed = self.exo.autorepeat(self.now, key);
                    if let Some(spec) = &mut self.spec {
                        spec.counts.repeats += u64::from(handed);
                    }
                    self.host_sends(Vec::new(), false);
                }
                _ => {
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
                let confirms = self.extension >= Some(11);
                let tracked = self.exo.composition();
                let untracked = !confirms && tracked.is_some_and(|text| text.is_empty());
                self.untracked_restarts.push(untracked);
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
            UserAction::Settle => return,
        };
        let surface = self.display.surfaces()[field];
        if self.exo.focus() != Some(surface) {
            self.exo.move_focus(Some(surface));
        }
        let presses = match action {
            UserAction::Click { at, .. } => self.display.click(surface, at, time),
            UserAction::Tap { at, .. } => self.display.tap(surface, at, time),
            _ => Vec::new(),
        };
        self.host_sends(presses, false);
    }

    /// Exo's events since its last input, then `events`, go to the proxy,
    /// and what Exo handed the engine goes to the engine. Exo's events carry
    /// a late answer if `late`, made before the resets this input hands the
    /// engine.
    fn host_sends(&mut self, events: Vec<Outgoing>, late: bool) {
        self.host_sends_of(events, late, None);
    }

    /// [`World::host_sends`], of Exo's answer to a press of `press`.
    fn host_sends_of(&mut self, events: Vec<Outgoing>, late: bool, press: Option<Key>) {
        let origin = Origin {
            late,
            resets: self.resets,
            press,
        };
        for event in self.exo.take_events() {
            self.e_to_p
                .send(self.now, (event, origin), &mut self.choices);
        }
        for event in events {
            let origin = Origin::default();
            self.e_to_p
                .send(self.now, (event, origin), &mut self.choices);
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
        let press = self.exo.press_of(&answer);
        let verdict = self.exo.answer(&answer);
        if let EngineOut::Text { actions, .. } = &answer {
            if actions
                .iter()
                .any(|a| matches!(a, TextAction::Region { .. }))
            {
                self.engine.learn_region(self.exo.served_composition());
            }
        }
        if let Some(spec) = &mut self.spec {
            spec.counts.late += u64::from(verdict == Answer::Late);
            spec.counts.dropped += u64::from(verdict == Answer::Dropped);
        }
        self.host_sends_of(Vec::new(), verdict == Answer::Late, press);
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
                if let Some(spec) = self.spec.as_mut().filter(|_| report) {
                    spec.counts.crossed_reports += u64::from(edits < self.edits.sent.0);
                }
                self.see(Seen::Guest(request));
            }
            Err(_) => self.log(Place::CToP, "a setup request".to_owned()),
        }
        let goes_on = self.session.try_guest([request]).await;
        self.send_proxy_output(goes_on);
    }

    /// A host event reaches the proxy.
    async fn deliver_host_event(&mut self, event: Outgoing, origin: Origin) {
        let (sender, decoded) = self.display.decode(&event);
        let note = if origin.late { " (late)" } else { "" };
        self.log(Place::EToP, format!("{sender} {}{note}", shown(&decoded)));
        self.see(Seen::Host {
            sender,
            event: &decoded,
            origin,
        });
        let goes_on = self.session.try_host([event]).await;
        self.send_proxy_output(goes_on);
    }

    /// What the proxy sent goes out on P→E and P→C.
    fn send_proxy_output(&mut self, goes_on: bool) {
        for (sender, request) in self.session.host_received() {
            self.log(Place::PToE, format!("{sender} {request:?}"));
            self.see(Seen::ToHost(&request));
            self.p_to_e
                .send(self.now, (sender, request), &mut self.choices);
        }
        for (sender, event) in self.session.guest_received() {
            self.log(Place::PToC, format!("{sender} {}", shown(&event)));
            self.edits.sent.follow(&event);
            self.see(Seen::ToGuest(&event));
            let press = self.press(&event);
            if let Some((surface, at)) = press {
                self.see(Seen::GuestPress { surface, at });
            }
            self.p_to_c
                .send(self.now, (sender, event, press), &mut self.choices);
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
            }) => self.pointer = Some((surface, surface_x)),
            AnyEvent::WlPointer(wl_pointer::Event::Motion { surface_x, .. }) => {
                if let Some((_, x)) = &mut self.pointer {
                    *x = surface_x;
                }
            }
            AnyEvent::WlPointer(wl_pointer::Event::Leave { .. }) => self.pointer = None,
            AnyEvent::WlPointer(wl_pointer::Event::Button { state, .. })
                if state == wl_pointer::BUTTON_STATE_PRESSED =>
            {
                let (surface, x) = self.pointer.expect("a press on a surface");
                return Some((surface, target(x)));
            }
            AnyEvent::WlTouch(wl_touch::Event::Down { surface, x, .. }) => {
                return Some((surface, target(x)));
            }
            _ => {}
        }
        None
    }

    /// An event reaches the guest: the client model takes what it models,
    /// a press as `press` resolved it, and the guest keeps the globals and
    /// Shift.
    fn deliver_to_guest(&mut self, sender: u32, event: AnyEvent, press: Option<Press>) {
        let now = self.now;
        self.edits.got.follow(&event);
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
            (AnyEvent::ZwpTextInputV3(event), _) => client.event(now, sender, event),
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

    /// The oracle's readers follow `seen`, and check each step's output.
    fn see(&mut self, seen: Seen<'_>) {
        let (Some(spec), Some(shadow)) = (&mut self.spec, &mut self.shadow) else {
            return;
        };
        spec.see(seen);
        shadow.see(seen);
        if !matches!(seen, Seen::StepEnd) {
            return;
        }
        if let Some(failed) = oracle::step(spec, shadow) {
            if self.steps.len() < 3 {
                let at = self.now.0 as f64 / 1_000.0;
                self.steps.push(format!("{at:.3} ms {failed}"));
            }
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
