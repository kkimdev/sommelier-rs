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

//! Randomized guest streams through a whole [`Client`].
//!
//! A guest client controls the bytes and descriptors it sends, and the proxy
//! must survive whatever arrives. Each case builds a request stream from the
//! vendored protocol XML (requests of live objects, typed arguments, real
//! descriptors), mutates some requests, and writes the stream in random pieces
//! over a socket to `Client::run`. Half the cases end like a client typing into
//! text fields, switching the focus between surfaces and waiting for each with
//! a roundtrip, and sometimes releasing its seat. The host peer is a compositor
//! that advertised every global (and one `wl_output` it then removed, so a bind
//! of it crosses the removal), answers each `wl_display.sync`, offers each new
//! data device a selection and answers each asynchronous dma-buf create with a
//! buffer of its own, deletes each object the proxy destroys, as weston does,
//! destroys a destroyed surface's pending frame callbacks without `done`, and
//! gives the keyboard focus to each surface the proxy commits, as a click on
//! its window would, holds its answer to a text input's drain barrier, as a
//! slow host would, presses a pointer button or touches down on the focused
//! surface while a preedit is live, and keeps the text-input state Exo would
//! ([`crate::sim::exo`]). In some cases it also destroys live objects of one
//! interface on its own ([`HOST_DESTROYED`]).
//!
//! Every case must:
//! - end within [`CASE_TIMEOUT`] without a panic;
//! - leave the process with as many open descriptors as before it;
//! - when its framing is intact, either answer the stream's closing
//!   `wl_display.sync` (the connection stayed healthy) or send the guest a
//!   `wl_display.error` before closing it;
//! - send the guest at most one `wl_display.error`, and nothing after it;
//! - never end a session with `SessionEnd::Internal`: that is the proxy
//!   failing, never a rejection of the guest, and it is counted even when
//!   the guest has gone or a protocol error overrides it;
//! - leave no state behind an ended object after any batch that keeps the
//!   session (`Context::assert_no_orphans`);
//! - send the host only what a real compositor accepts: requests from live
//!   host objects with opcodes their interfaces have, fresh `new_id`s, and
//!   object arguments naming live objects of the declared interface. The
//!   fake compositor deletes each destroyed object at once, so the proxy's
//!   host ID reuse runs too;
//! - when the guest's stream is whole and undamaged, send the guest only
//!   what the proxy should send, which is stricter than what
//!   libwayland-client tolerates: events from and naming only objects the
//!   guest has, a `wl_display.delete_id` only for an object it destroyed,
//!   a frame callback the compositor cancelled, or an object the compositor
//!   destroyed on its own, once (and, in a healthy
//!   session, one for each cancelled callback), and objects the server
//!   creates under fresh IDs from the server range.
//!
//! What Exo holds of a text field at the end is the DST's to check
//! ([`crate::sim`]).
//!
//! The cases run in a child process that runs nothing else, so the
//! descriptor count is exact while other tests run in parallel. A case's
//! input follows from its seed, but the proxy's timing (how the pieces
//! arrive, which side it serves first) does not, so a failing seed may need
//! a few replays. CONTRIBUTING.md says how to run more cases, from another
//! seed, and how to replay one.

// Generated requests are written argument by argument and then mutated,
// which no encoder can do.
#![allow(clippy::disallowed_methods)]

mod typing;

use super::{Client, Direction};
use crate::connection::{WaylandConnection, MAX_FDS_PER_SEND};
use crate::protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1;
use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
use crate::protocols::wayland::{
    wl_callback, wl_data_device, wl_data_device_manager, wl_display, wl_fixes, wl_keyboard,
    wl_pointer, wl_registry, wl_seat, wl_surface, wl_touch,
};
use crate::protocols::AnyRequest;
use crate::sim::exo::Exo;
use crate::state::Context;
use crate::test_support::{env_u64, frames, msg_opcode, Rng};
use crate::wire::{Framing, MessageBuilder, MessageHeader, Outgoing, WireMessage, WL_DISPLAY_ID};
use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IoSlice, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use wayland_codegen::protocol::{EnumItem, Interface, InterfaceItem, MessageItem, ProtocolItem};

/// Cases per run unless `SOMMELIER_FUZZ_CASES` says otherwise.
const DEFAULT_CASES: u64 = 5000;
/// The fewest cases a run needs before it must have exercised ID reuse and
/// server objects; a replay of one seed need not.
const COVERAGE_CASES: u64 = 1000;
/// How long one case may take before it counts as a hang.
const CASE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the proxy may take to finish once it has closed the guest
/// connection.
const EXIT_GRACE: Duration = Duration::from_secs(5);
/// The most random requests one case sends before its closing sync.
const MAX_REQUESTS: usize = 64;
/// The guest's `wl_registry`, bound before each case's requests.
const REGISTRY: u32 = 2;
/// `BTN_LEFT` of `linux/input-event-codes.h`.
const BTN_LEFT: u32 = 0x110;
/// The first ID of the range servers allocate from (`WL_SERVER_ID_START`).
const SERVER_ID_START: u32 = 0xff00_0000;

#[test]
fn guest_streams_cannot_crash_hang_or_leak() {
    in_own_process(|| {
        // `RUST_LOG` shows the proxy's view of a failing seed.
        install_logger();
        let cases = env_u64("SOMMELIER_FUZZ_CASES", DEFAULT_CASES);
        let first_seed = env_u64("SOMMELIER_FUZZ_SEED", 0);
        let protocols = Arc::new(Protocols::load());
        // Tokio's first runtime opens a socket pair for the whole process.
        drop(tokio::runtime::Runtime::new());
        let baseline = open_fds();
        let mut advertised = BTreeMap::new();
        let mut outcomes = BTreeMap::new();
        for seed in first_seed..first_seed + cases {
            let config = Config::from_seed(seed);
            let globals = advertised
                .entry(config)
                .or_insert_with(|| discover_globals(&protocols, config));
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let outcome = run_case(&protocols, globals, config, seed);
                assert_eq!(open_fds(), baseline, "descriptors leaked");
                outcome
            }))
            .unwrap_or_else(|_| {
                panic!(
                    "seed {seed} failed; rerun it alone with SOMMELIER_FUZZ_SEED={seed} \
                     SOMMELIER_FUZZ_CASES=1, repeating it if need be: the proxy and the \
                     compositor write to their sockets at once, so the order their \
                     messages arrive in, and a failure, may not recur on every run"
                )
            });
            *outcomes.entry(outcome).or_insert(0) += 1;
        }
        let reused = HOST_IDS_REUSED.load(Ordering::Relaxed);
        let server_objects = GUEST_SERVER_OBJECTS.load(Ordering::Relaxed);
        let cancelled = FRAME_CALLBACKS_CANCELLED.load(Ordering::Relaxed);
        let host_destroyed = LIVE_OBJECTS_HOST_DESTROYED.load(Ordering::Relaxed);
        let leaves = TEXT_INPUT_LEAVES.load(Ordering::Relaxed);
        let restarts = INPUT_METHOD_RESTARTS.load(Ordering::Relaxed);
        let seatless = SEATLESS_ACTIVATIONS.load(Ordering::Relaxed);
        let dead_seat_deactivates = DEACTIVATES_OF_DESTROYED_SEATS.load(Ordering::Relaxed);
        let presses = COMPOSITIONS_ENDED_BY_PRESSES.load(Ordering::Relaxed);
        let press_restarts = RESTARTS_AFTER_PRESSES.load(Ordering::Relaxed);
        let aura_toplevels = AURA_TOPLEVELS.load(Ordering::Relaxed);
        if cases >= COVERAGE_CASES {
            assert!(
                aura_toplevels > 0,
                "no case created an Aura toplevel, so placement's went untested"
            );
            assert!(
                presses > 0,
                "no press ended a composition, so the reset fence went untested"
            );
            assert!(
                press_restarts > 0,
                "no input method restarted after a press that ended a composition, so a leave during the reset fence went untested"
            );
            assert!(
                reused > 0,
                "no case reused a host ID, so ID reuse went untested"
            );
            assert!(
                server_objects > 0,
                "no checked case created a server object, so their IDs went untested"
            );
            assert!(
                cancelled > 0,
                "no case cancelled a frame callback, so host-destroyed objects went untested"
            );
            assert!(
                host_destroyed > 0,
                "the host destroyed no live object, so forgetting one went untested"
            );
            assert!(
                restarts > 0,
                "no input method restarted while attached, so Exo's own detach went untested"
            );
            assert!(
                seatless > 0,
                "no activation outlived its seat, so ending one without a seat went untested"
            );
        }
        println!(
            "{cases} cases from seed {first_seed} passed: {outcomes:?}, {reused} host IDs reused, \
             {server_objects} guest server objects checked, {cancelled} frame callbacks cancelled, \
             {leaves} text-input leaves sent, {restarts} input methods restarted, \
             {seatless} activations outlived their seat, \
             {dead_seat_deactivates} deactivates named a destroyed seat, \
             {presses} compositions ended by a press, {press_restarts} restarts after a press, \
             {aura_toplevels} Aura toplevels created"
        );
    });
}

/// What the proxy logged at `error` since the last [`take_proxy_errors`]:
/// how many sessions ended as `SessionEnd::Internal`
/// (`SessionEnd::internal`), and every message, which says why.
static PROXY_ERRORS: std::sync::Mutex<(usize, Vec<String>)> =
    std::sync::Mutex::new((0, Vec::new()));

/// A logger that keeps the proxy's errors and passes everything to
/// `env_logger`, which `RUST_LOG` configures.
struct CaptureErrors(env_logger::Logger);

impl log::Log for CaptureErrors {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Error || self.0.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if record.level() == log::Level::Error {
            let mut errors = PROXY_ERRORS.lock().expect("the error list");
            if record.target() == crate::state::INTERNAL_END_TARGET {
                errors.0 += 1;
            } else {
                errors.1.push(record.args().to_string());
            }
        }
        self.0.log(record);
    }

    fn flush(&self) {
        self.0.flush();
    }
}

fn install_logger() {
    let inner = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("off"))
        .is_test(true)
        .build();
    let level = inner.filter().max(log::LevelFilter::Error);
    if log::set_boxed_logger(Box::new(CaptureErrors(inner))).is_ok() {
        log::set_max_level(level);
    }
}

fn take_proxy_errors() -> (usize, Vec<String>) {
    std::mem::take(&mut *PROXY_ERRORS.lock().expect("the error list"))
}

/// Run `cases` in a child copy of this test binary that runs only this
/// test, so the process's descriptors belong to the cases alone.
fn in_own_process(cases: impl FnOnce()) {
    let stdout = crate::test_support::in_own_process(
        "proxy::fuzz::guest_streams_cannot_crash_hang_or_leak",
        cases,
    );
    // The case count and outcomes, shown with `--nocapture`.
    for summary in stdout
        .lines()
        .filter(|line| line.contains("cases from seed"))
    {
        println!("{summary}");
    }
}

/// What each open descriptor of the process refers to.
fn open_fds() -> Vec<std::path::PathBuf> {
    let mut targets: Vec<_> = std::fs::read_dir("/proc/self/fd")
        .expect("list open descriptors")
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .collect();
    targets.sort();
    targets
}

/// The proxy options a case runs with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Config {
    gpu_accel: bool,
    xdg_decoration: bool,
    /// The version of the host's `zcr_text_input_extension_v1`.
    text_input_extension: u32,
    /// Whether window placement is on, so every XDG toplevel gets a
    /// host-only Aura toplevel in screen coordinates.
    placement: bool,
}

impl Config {
    fn from_seed(seed: u64) -> Self {
        Self {
            gpu_accel: seed & 1 != 0,
            xdg_decoration: seed & 2 != 0,
            text_input_extension: TEXT_INPUT_EXTENSION_VERSIONS[(seed >> 2) as usize % 4],
            // A quarter of the cases, independent of the bits above.
            placement: (seed >> 4) & 3 == 0,
        }
    }

    /// The proxy's context for a case.
    fn context(self) -> Context {
        if !self.placement {
            return Context::new_for_test(self.gpu_accel, self.xdg_decoration, Vec::new());
        }
        let mut context = Context::with_placement_for_test(
            crate::placement_config::PlacementConfig::active_for_test(&[]),
        );
        context.gpu_accel = self.gpu_accel;
        context.xdg_decoration = self.xdg_decoration;
        context
    }
}

/// The host's `zcr_text_input_extension_v1` versions: the newest, which
/// the proxy creates text inputs with, three cases in four, and the one
/// before the first it does (`confirm_preedit`'s), whose text inputs have
/// no input method.
const TEXT_INPUT_EXTENSION_VERSIONS: [u32; 4] = {
    use crate::protocols::text_input_extension_unstable_v1::{
        zcr_extended_text_input_v1 as extended, zcr_text_input_extension_v1,
    };
    let newest = zcr_text_input_extension_v1::VERSION;
    [
        newest,
        extended::EVT_CONFIRM_PREEDIT_SINCE - 1,
        newest,
        newest,
    ]
};

/// One argument of a request, as the protocol XML declares it.
enum Arg {
    /// An `int`, `uint` or `fixed`, with the values of its enum, if any.
    Number {
        values: Vec<u32>,
    },
    Str {
        nullable: bool,
    },
    Array,
    Fd,
    Object {
        interface: Option<String>,
        nullable: bool,
    },
    NewId {
        interface: Option<String>,
    },
}

/// A request or an event, as the protocol XML declares it.
struct Signature {
    args: Vec<Arg>,
    /// The object version that introduced the message.
    since: u32,
    destructor: bool,
}

/// The requests of every interface the proxy is built with, and the
/// globals a compositor can advertise.
struct Protocols {
    requests: BTreeMap<String, Vec<Signature>>,
    events: BTreeMap<String, Vec<Signature>>,
    /// `(interface, version)` of each interface that no message creates,
    /// in the order the fake compositor advertises them: global name `n`
    /// is entry `n - 1`.
    globals: Vec<(String, u32)>,
}

impl Protocols {
    fn load() -> Self {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../third_party/protocols");
        let mut paths: Vec<_> = std::fs::read_dir(dir)
            .expect("list the protocol XML")
            .map(|entry| entry.expect("protocol directory entry").path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "xml"))
            .collect();
        paths.sort();
        let interfaces: Vec<Interface> = paths
            .iter()
            .flat_map(|path| {
                wayland_codegen::parse(path)
                    .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
                    .items
            })
            .filter_map(|item| match item {
                ProtocolItem::Interface(interface) => Some(interface),
                _ => None,
            })
            .collect();

        // Enums by their qualified name, `interface.enum`, as arguments
        // name them across protocols.
        let mut enums: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        for interface in &interfaces {
            for item in &interface.items {
                if let InterfaceItem::Enum(declared) = item {
                    let values = declared.items.iter().filter_map(|item| match item {
                        EnumItem::Entry(entry) => Some(parse_enum_value(&entry.value)),
                        EnumItem::Description(_) => None,
                    });
                    enums.insert(
                        format!("{}.{}", interface.name, declared.name),
                        values.collect(),
                    );
                }
            }
        }

        let mut requests = BTreeMap::new();
        let mut events = BTreeMap::new();
        let mut created = BTreeSet::new();
        for interface in &interfaces {
            let mut interface_requests = Vec::new();
            let mut interface_events = Vec::new();
            for item in &interface.items {
                let (InterfaceItem::Request(message) | InterfaceItem::Event(message)) = item else {
                    continue;
                };
                let args: Vec<Arg> = message
                    .items
                    .iter()
                    .filter_map(|item| match item {
                        MessageItem::Arg(arg) => Some(arg),
                        MessageItem::Description(_) => None,
                    })
                    .map(|arg| {
                        let nullable = arg.allow_null == Some(true);
                        match arg.typ.as_str() {
                            "int" | "uint" | "fixed" => Arg::Number {
                                values: arg.enum_name.as_ref().map_or_else(Vec::new, |name| {
                                    let qualified = if name.contains('.') {
                                        name.clone()
                                    } else {
                                        format!("{}.{name}", interface.name)
                                    };
                                    enums[&qualified].clone()
                                }),
                            },
                            "string" => Arg::Str { nullable },
                            "array" => Arg::Array,
                            "fd" => Arg::Fd,
                            "object" => Arg::Object {
                                interface: arg.interface.clone(),
                                nullable,
                            },
                            "new_id" => {
                                created.extend(arg.interface.clone());
                                Arg::NewId {
                                    interface: arg.interface.clone(),
                                }
                            }
                            other => panic!("unknown argument type {other}"),
                        }
                    })
                    .collect();
                let signature = Signature {
                    args,
                    since: message.since.unwrap_or(1),
                    destructor: message.msg_type.as_deref() == Some("destructor"),
                };
                match item {
                    InterfaceItem::Request(_) => interface_requests.push(signature),
                    _ => interface_events.push(signature),
                }
            }
            requests.insert(interface.name.clone(), interface_requests);
            events.insert(interface.name.clone(), interface_events);
        }
        let globals = interfaces
            .iter()
            .filter(|interface| {
                interface.name != "wl_display" && !created.contains(&interface.name)
            })
            .map(|interface| (interface.name.clone(), interface.version))
            .collect();
        Self {
            requests,
            events,
            globals,
        }
    }
}

/// An enum entry's value: decimal, `0x` hexadecimal, or `a << b`.
fn parse_enum_value(value: &str) -> u32 {
    let number = |text: &str| match text.trim().strip_prefix("0x") {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => text.trim().parse(),
    };
    match value.split_once("<<") {
        Some((base, shift)) => number(base).and_then(|base| Ok(base << number(shift)?)),
        None => number(value),
    }
    .unwrap_or_else(|_| panic!("enum value {value}"))
}

/// A global as the proxy advertised it to the guest.
struct Global {
    name: u32,
    interface: String,
    version: u32,
}

/// The globals the proxy advertises to a guest under `config`.
fn discover_globals(protocols: &Arc<Protocols>, config: Config) -> Vec<Global> {
    let callback = REGISTRY + 1;
    let sync = Submission {
        bytes: sync_request(callback).encode(),
        ..Submission::default()
    };
    let events = exchange(protocols, config, 0, vec![sync], true, callback, None)
        .expect("the proxy answers a lone sync in time");
    frames(&events)
        .filter(|(header, _)| {
            (header.sender_id, header.opcode) == (REGISTRY, wl_registry::EVT_GLOBAL)
        })
        .map(|(header, payload)| {
            let mut global = WireMessage::new(header.sender_id, header.opcode, payload);
            Global {
                name: global.read_u32().expect("global name"),
                interface: global.read_string().expect("global interface"),
                version: global.read_u32().expect("global version"),
            }
        })
        .collect()
}

/// One request, kept unencoded so a mutation keeps its framing.
struct Message {
    sender: u32,
    opcode: u16,
    payload: Vec<u8>,
    fds: Vec<OwnedFd>,
    /// For the `wl_display.sync` of a roundtrip, its callback, whose `done`
    /// the guest waits for before it sends more.
    roundtrip: Option<u32>,
}

impl Message {
    /// The message's bytes. The header is written by hand, because a guest
    /// that does not use libwayland can send a message longer than any
    /// encoder builds.
    fn encode(&self) -> Vec<u8> {
        let len = MessageHeader::LEN + self.payload.len();
        let word = (len as u32) << 16 | u32::from(self.opcode);
        [
            &self.sender.to_ne_bytes()[..],
            &word.to_ne_bytes(),
            &self.payload,
        ]
        .concat()
    }
}

fn sync_request(callback: u32) -> Message {
    request(WL_DISPLAY_ID, wl_display::REQ_SYNC, &[callback])
}

/// A request of `sender` whose arguments are `words`.
fn request(sender: u32, opcode: u16, words: &[u32]) -> Message {
    let mut payload = MessageBuilder::new();
    words.iter().for_each(|&word| payload.write_u32(word));
    Message {
        sender,
        opcode,
        payload: payload.into_payload(),
        fds: Vec::new(),
        roundtrip: None,
    }
}

/// A guest client that tracks the objects it created, so most requests
/// come from live objects and name objects of the right interface.
struct Guest<'a> {
    protocols: &'a Protocols,
    globals: &'a [Global],
    rng: Rng,
    /// Each live object's interface and version.
    objects: BTreeMap<u32, (&'a str, u32)>,
    /// What the guest's requests did to its objects.
    view: GuestView,
    /// Each live surface's frame callbacks.
    frame_callbacks: BTreeMap<u32, Vec<u32>>,
    next_id: u32,
}

impl<'a> Guest<'a> {
    fn new(protocols: &'a Protocols, globals: &'a [Global], rng: Rng) -> Self {
        let objects = BTreeMap::from([
            (WL_DISPLAY_ID, ("wl_display", 1)),
            (REGISTRY, ("wl_registry", 1)),
        ]);
        let view = GuestView {
            interfaces: objects
                .iter()
                .map(|(&id, &(interface, _))| (id, interface.to_string()))
                .collect(),
            destroyed: BTreeSet::new(),
            cancelled: BTreeSet::new(),
            reused: BTreeMap::new(),
        };
        Self {
            protocols,
            globals,
            rng,
            objects,
            view,
            frame_callbacks: BTreeMap::new(),
            next_id: REGISTRY + 1,
        }
    }

    fn create(&mut self, interface: &'a str, version: u32) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.objects.insert(id, (interface, version));
        self.view.interfaces.insert(id, interface.to_string());
        id
    }

    /// Create an object of `interface` under `id`, an object the guest
    /// destroyed and whose `delete_id` it has received.
    fn reuse(&mut self, id: u32, interface: &'a str, version: u32) -> u32 {
        self.objects.insert(id, (interface, version));
        self.view.reused.insert(id, interface.to_string());
        id
    }

    fn destroy(&mut self, id: u32) {
        if self.objects.remove(&id).is_some() {
            self.view.destroyed.insert(id);
            let cancelled = self.frame_callbacks.remove(&id).unwrap_or_default();
            self.view.cancelled.extend(cancelled);
        }
    }

    /// Record `callback` as a frame callback of `surface`.
    fn frame(&mut self, surface: u32, callback: u32) {
        self.frame_callbacks
            .entry(surface)
            .or_default()
            .push(callback);
    }

    /// A request a live object has, or a bind of an advertised global.
    ///
    /// Takes a few tries to find a request whose object arguments can name
    /// live objects, so most requests get past argument checks.
    fn request(&mut self) -> Message {
        let protocols = self.protocols;
        for _ in 0..8 {
            let index = self.rng.below(self.objects.len());
            let (&sender, &(interface, version)) =
                self.objects.iter().nth(index).expect("a live object");
            // Only the first registry has globals: the compositor
            // advertises none to a later one.
            if interface == "wl_registry" || self.rng.one_in(4) {
                break;
            }
            let requests = &protocols.requests[interface];
            if requests.is_empty() {
                continue;
            }
            let opcode = self.rng.below(requests.len());
            let request = &requests[opcode];
            if request.since > version || !request.args.iter().all(|arg| self.can_name(arg)) {
                continue;
            }
            let message = self.message(sender, opcode as u16);
            if (interface, message.opcode) == ("wl_surface", wl_surface::REQ_FRAME) {
                let callback = WireMessage::new(sender, message.opcode, &message.payload)
                    .read_u32()
                    .expect("a callback");
                self.frame(sender, callback);
            }
            if request.destructor {
                self.destroy(sender);
            }
            // `wl_fixes.destroy_registry` destroys the registry it names,
            // not its sender.
            if (interface, message.opcode) == ("wl_fixes", wl_fixes::REQ_DESTROY_REGISTRY) {
                let registry = WireMessage::new(sender, message.opcode, &message.payload)
                    .read_u32()
                    .expect("a registry");
                self.destroy(registry);
            }
            return message;
        }
        self.bind()
    }

    /// Request `opcode` of the live object `sender`, with arguments of the
    /// declared types.
    fn message(&mut self, sender: u32, opcode: u16) -> Message {
        let protocols = self.protocols;
        let (interface, version) = self.objects[&sender];
        let mut message = request(sender, opcode, &[]);
        for arg in &protocols.requests[interface][usize::from(opcode)].args {
            self.write_arg(arg, version, &mut message);
        }
        message
    }

    /// Whether a live object can fill `arg`.
    fn can_name(&self, arg: &Arg) -> bool {
        match arg {
            Arg::Object {
                interface: Some(interface),
                nullable: false,
            } => self
                .objects
                .iter()
                .any(|(&id, (live, _))| id != REGISTRY && live == interface),
            _ => true,
        }
    }

    /// `wl_registry.bind` of an advertised global at a supported version.
    fn bind(&mut self) -> Message {
        let global = self.rng.pick(self.globals);
        self.bind_global(global).0
    }

    /// `wl_registry.bind` of `global`, and the new object's ID and version.
    fn bind_global(&mut self, global: &'a Global) -> (Message, u32, u32) {
        let mut payload = MessageBuilder::new();
        payload.write_u32(global.name);
        let (id, version) = self.write_new_object(global, &mut payload);
        let message = Message {
            sender: REGISTRY,
            opcode: wl_registry::REQ_BIND,
            payload: payload.into_payload(),
            fds: Vec::new(),
            roundtrip: None,
        };
        (message, id, version)
    }

    /// What a simple client sends first: a 32x32 SHM buffer shown on an
    /// xdg toplevel. Random requests rarely build so long a chain of valid
    /// objects and arguments, and the SHM copy path is behind it.
    fn draw_a_frame(&mut self) -> Vec<Message> {
        use crate::protocols::wayland::{wl_compositor, wl_shm, wl_shm_pool, wl_surface};
        use crate::protocols::xdg_shell::{xdg_surface, xdg_wm_base};
        const SIZE: u32 = 32;
        const STRIDE: u32 = 4 * SIZE;
        const XRGB8888: u32 = 1;
        let find = |interface: &str| {
            self.globals
                .iter()
                .find(|global| global.interface == interface)
        };
        let (Some(compositor), Some(shm), Some(wm_base)) =
            (find("wl_compositor"), find("wl_shm"), find("xdg_wm_base"))
        else {
            return Vec::new();
        };
        // A new object takes its parent's version.
        let (bind_compositor, compositor, compositor_version) = self.bind_global(compositor);
        let (bind_shm, shm, shm_version) = self.bind_global(shm);
        let (bind_wm_base, wm_base, wm_base_version) = self.bind_global(wm_base);
        let pool = self.create("wl_shm_pool", shm_version);
        let buffer = self.create("wl_buffer", shm_version);
        let surface = self.create("wl_surface", compositor_version);
        let role = self.create("xdg_surface", wm_base_version);
        let toplevel = self.create("xdg_toplevel", wm_base_version);
        let frame = self.create("wl_callback", 1);
        self.frame(surface, frame);
        let mut create_pool = request(shm, wl_shm::REQ_CREATE_POOL, &[pool, 1 << 16]);
        create_pool.fds.push(self.memfd());
        vec![
            bind_compositor,
            bind_shm,
            bind_wm_base,
            create_pool,
            request(
                pool,
                wl_shm_pool::REQ_CREATE_BUFFER,
                &[buffer, 0, SIZE, SIZE, STRIDE, XRGB8888],
            ),
            request(compositor, wl_compositor::REQ_CREATE_SURFACE, &[surface]),
            request(wm_base, xdg_wm_base::REQ_GET_XDG_SURFACE, &[role, surface]),
            request(role, xdg_surface::REQ_GET_TOPLEVEL, &[toplevel]),
            request(surface, wl_surface::REQ_ATTACH, &[buffer, 0, 0]),
            request(surface, wl_surface::REQ_DAMAGE, &[0, 0, SIZE, SIZE]),
            request(surface, wl_surface::REQ_FRAME, &[frame]),
            request(surface, wl_surface::REQ_COMMIT, &[]),
        ]
    }

    /// A `wl_display.sync` the guest waits for, as `wl_display_roundtrip`
    /// does.
    fn roundtrip(&mut self) -> Message {
        let callback = self.create("wl_callback", 1);
        Message {
            roundtrip: Some(callback),
            ..sync_request(callback)
        }
    }

    /// The untyped `new_id` of a bind: interface, version and ID. Returns
    /// the new object's ID and version.
    fn write_new_object(&mut self, global: &'a Global, payload: &mut MessageBuilder) -> (u32, u32) {
        payload.write_string(&global.interface);
        let version = 1 + self.rng.below(global.version as usize) as u32;
        payload.write_u32(version);
        let id = self.create(&global.interface, version);
        payload.write_u32(id);
        (id, version)
    }

    /// Write `arg` of a request from an object of `version`, which objects
    /// the request creates share.
    fn write_arg(&mut self, arg: &'a Arg, version: u32, message: &mut Message) {
        let mut payload = MessageBuilder::from_payload(std::mem::take(&mut message.payload));
        match arg {
            // Mostly a declared value, as a client would send, and often one
            // of the first few, which the XML lists as the common ones.
            Arg::Number { values } if !values.is_empty() && !self.rng.one_in(4) => {
                let common = &values[..values.len().min(4)];
                let values = if self.rng.one_in(2) { common } else { values };
                payload.write_u32(*self.rng.pick(values))
            }
            Arg::Number { .. } => payload.write_u32(self.number()),
            Arg::Str { nullable } if *nullable && self.rng.one_in(4) => payload.write_u32(0),
            Arg::Str { .. } => payload.write_string_bytes(&self.string()),
            Arg::Array => {
                let len = self.rng.below(33);
                payload.write_array(&self.rng.bytes(len));
            }
            Arg::Fd => message.fds.push(self.memfd()),
            Arg::Object {
                interface,
                nullable,
            } => payload.write_u32(self.object(interface.as_deref(), *nullable)),
            Arg::NewId {
                interface: Some(interface),
            } => payload.write_u32(self.create(interface, version)),
            Arg::NewId { interface: None } => {
                let global = self.rng.pick(self.globals);
                self.write_new_object(global, &mut payload);
            }
        }
        message.payload = payload.into_payload();
    }

    /// A number, mostly a small power of two (or 0), so that sizes,
    /// offsets and strides often fit each other; otherwise an edge of a
    /// range, or anything at all.
    fn number(&mut self) -> u32 {
        const EDGES: [u32; 5] = [
            u32::MAX,
            i32::MAX as u32,
            i32::MIN as u32,
            0xff00_0000,
            0xffff,
        ];
        match self.rng.below(8) {
            0 => *self.rng.pick(&EDGES),
            1 => self.rng.next_u32(),
            _ => *self.rng.pick(&[0, 1, 2, 4, 8, 16, 32, 64, 256, 4096]),
        }
    }

    /// The bytes of a string, which carry no NUL and are not always
    /// UTF-8.
    fn string(&mut self) -> Vec<u8> {
        match self.rng.below(5) {
            0 => Vec::new(),
            1 => self.rng.pick(self.globals).interface.clone().into_bytes(),
            // Rarely, about as long as a whole message can be, so that a
            // request lands on either side of the limit. The proxy replaces
            // each 0xff with the three bytes of U+FFFD, which can push the
            // forward over it.
            2 if self.rng.one_in(8) => {
                let byte = if self.rng.one_in(2) { 0xff } else { b'x' };
                vec![byte; MessageHeader::MAX_LEN - 24 + self.rng.below(24)]
            }
            2 => vec![b'x'; self.rng.below(300)],
            // High bytes, mostly not UTF-8, as from a client that sends
            // Latin-1.
            3 => (0..self.rng.below(16))
                .map(|_| 0x80 | self.rng.below(128) as u8)
                .collect(),
            _ => (0..self.rng.below(16))
                .map(|_| b'a' + self.rng.below(26) as u8)
                .collect(),
        }
    }

    /// A live object of `interface` (any interface if `None`), or null.
    ///
    /// Never the first registry: `wl_fixes.destroy_registry` would end
    /// the registry every bind uses.
    fn object(&mut self, interface: Option<&str>, nullable: bool) -> u32 {
        let live: Vec<u32> = self
            .objects
            .iter()
            .filter(|&(&id, (live, _))| {
                id != REGISTRY && interface.is_none_or(|interface| interface == *live)
            })
            .map(|(&id, _)| id)
            .collect();
        if live.is_empty() || (nullable && self.rng.one_in(4)) {
            0
        } else {
            *self.rng.pick(&live)
        }
    }

    /// A memfd large enough for a small `wl_shm` pool, now and then empty.
    fn memfd(&mut self) -> OwnedFd {
        let fd = nix::sys::memfd::memfd_create(
            c"sommelier-fuzz",
            nix::sys::memfd::MFdFlags::MFD_CLOEXEC,
        )
        .expect("memfd_create");
        let size = if self.rng.one_in(8) { 0 } else { 1 << 16 };
        nix::unistd::ftruncate(&fd, size).expect("size the memfd");
        fd
    }

    /// Damage one request without breaking the stream's framing.
    fn mutate(&mut self, message: &mut Message) {
        let payload = &mut message.payload;
        match self.rng.below(6) {
            0 if !payload.is_empty() => {
                let byte = self.rng.below(payload.len());
                payload[byte] ^= 1 << self.rng.below(8);
            }
            1 if payload.len() >= 4 => {
                let word = 4 * self.rng.below(payload.len() / 4);
                payload[word..word + 4].copy_from_slice(&self.number().to_ne_bytes());
            }
            2 if payload.len() >= 4 && self.rng.one_in(2) => payload.truncate(payload.len() - 4),
            2 => payload.extend_from_slice(&self.number().to_ne_bytes()),
            3 => message.opcode = self.rng.below(16) as u16,
            4 => message.sender = self.object(None, false),
            _ if message.fds.is_empty() => message.fds.push(self.memfd()),
            _ => drop(message.fds.pop()),
        }
    }
}

/// One generated input: the transport submissions the guest makes.
struct Case {
    sends: Vec<Submission>,
    /// Whether every message is still whole, so the closing sync is a
    /// message of its own.
    framed: bool,
    /// The guest callback of the closing `wl_display.sync`.
    callback: u32,
    /// What the guest knows of its objects, when its requests are whole
    /// and undamaged, so the events it receives can be checked.
    guest: Option<GuestView>,
    /// The interface whose live objects the compositor destroys on its
    /// own, if any (see [`HOST_DESTROYED`]).
    host_destroys: Option<&'static str>,
}

/// One transport submission of the guest.
#[derive(Default)]
struct Submission {
    bytes: Vec<u8>,
    fds: Vec<OwnedFd>,
    /// The callback of a roundtrip this submission ends, whose `done` the
    /// guest waits for before its next submission.
    then_await: Option<u32>,
}

/// The guest's objects, as its requests created and destroyed them.
struct GuestView {
    /// The interface of every object the guest created, live or not.
    interfaces: BTreeMap<u32, String>,
    /// The objects the guest destroyed.
    destroyed: BTreeSet<u32>,
    /// The frame callbacks of surfaces the guest destroyed, which the
    /// compositor destroys with their surface.
    cancelled: BTreeSet<u32>,
    /// The interface of each object the guest created under the ID of one
    /// it destroyed, once that one's `delete_id` arrived.
    reused: BTreeMap<u32, String>,
}

impl Case {
    fn generate(protocols: &Protocols, globals: &[Global], seed: u64) -> Self {
        let mut guest = Guest::new(protocols, globals, Rng::new(seed));
        let count = 1 + guest.rng.below(MAX_REQUESTS);
        let mut messages = if guest.rng.one_in(2) {
            guest.draw_a_frame()
        } else {
            Vec::new()
        };
        messages.extend((0..count).map(|_| guest.request()));
        if guest.rng.one_in(2) {
            messages.extend(guest.type_in_fields());
        }
        // Half the cases stay valid as far as the guest knows, so they
        // reach deep into the handlers; the rest damage a few requests.
        let mutations = if guest.rng.one_in(2) {
            0
        } else {
            1 + guest.rng.below(3)
        };
        for _ in 0..mutations {
            let index = guest.rng.below(messages.len());
            guest.mutate(&mut messages[index]);
        }
        let callback = guest.create("wl_callback", 1);
        messages.push(sync_request(callback));

        let rng = &mut guest.rng;
        let mut sends: Vec<Submission> = Vec::new();
        for message in messages {
            let bytes = message.encode();
            let full = sends.last().is_none_or(|send| {
                send.then_await.is_some() || send.fds.len() + message.fds.len() > MAX_FDS_PER_SEND
            });
            if full || rng.one_in(3) {
                sends.push(Submission::default());
            }
            let send = sends.last_mut().expect("a submission");
            send.fds.extend(message.fds);
            // Sometimes the rest of the message arrives in a later receive.
            let split = if bytes.len() > 1 && rng.one_in(4) {
                1 + rng.below(bytes.len() - 1)
            } else {
                bytes.len()
            };
            send.bytes.extend_from_slice(&bytes[..split]);
            if split < bytes.len() {
                sends.push(Submission {
                    bytes: bytes[split..].to_vec(),
                    ..Submission::default()
                });
            }
            // A damaged roundtrip may never be answered.
            if mutations == 0 {
                sends.last_mut().expect("a submission").then_await = message.roundtrip;
            }
        }

        // Rarely, break the framing itself.
        let framed = !rng.one_in(10);
        if !framed {
            sends.iter_mut().for_each(|send| send.then_await = None);
            let index = rng.below(sends.len());
            let bytes = &mut sends[index].bytes;
            let at = rng.below(bytes.len() + 1);
            match rng.below(3) {
                0 => bytes.truncate(at),
                1 => {
                    let len = 1 + rng.below(16);
                    bytes.splice(at..at, rng.bytes(len));
                }
                _ => {
                    let end = (at + 4).min(bytes.len());
                    let noise = rng.bytes(end - at);
                    bytes[at..end].copy_from_slice(&noise);
                }
            }
        }
        let host_destroys = rng.one_in(4).then(|| *rng.pick(&HOST_DESTROYED));
        Self {
            sends,
            framed,
            callback,
            guest: (framed && mutations == 0).then_some(guest.view),
            host_destroys,
        }
    }
}

/// How a case ended.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Outcome {
    /// The proxy answered the closing sync.
    Healthy,
    /// The proxy sent `wl_display.error` and closed the connection.
    Rejected,
    /// The stream's framing was broken, and the proxy closed it.
    Unframed,
}

/// Run one case and check its properties; see the module documentation.
fn run_case(protocols: &Arc<Protocols>, globals: &[Global], config: Config, seed: u64) -> Outcome {
    let case = Case::generate(protocols, globals, seed);
    let framed = case.framed;
    take_proxy_errors();
    let events = exchange(
        protocols,
        config,
        seed,
        case.sends,
        framed,
        case.callback,
        case.host_destroys,
    )
    .expect("the proxy answers or closes the connection in time");
    let headers: Vec<_> = frames(&events)
        .map(|(header, _)| (header.sender_id, header.opcode))
        .collect();
    let answered = headers.contains(&(case.callback, wl_callback::EVT_DONE));
    let error = (WL_DISPLAY_ID, wl_display::EVT_ERROR);
    let errors = headers.iter().filter(|&&event| event == error).count();
    assert!(
        errors <= 1,
        "the guest received {errors} wl_display.error events"
    );
    let rejected = errors == 1;
    // `SessionEnd::Internal` is the proxy failing, never a rejection of a
    // guest: its bookkeeping disagreed with itself or the host. It logs why
    // (`Context::fail`), whether or not the guest is still there to be told
    // and whatever protocol error overrides it, so [`SessionEnd::internal`]'s
    // log is the signal.
    // A fuzz case is made only of what a guest and a compositor may do, so
    // each is a bug in the proxy.
    let (internal_ends, proxy_errors) = take_proxy_errors();
    assert_eq!(
        internal_ends, 0,
        "seed {seed} ended {internal_ends} time(s) with SessionEnd::Internal; the proxy \
         logged these errors: {proxy_errors:?}"
    );
    assert!(
        !rejected || headers.last() == Some(&error),
        "the guest received {:?} after its wl_display.error",
        headers.last()
    );
    if let Some(guest) = &case.guest {
        let healthy = answered && !rejected;
        check_guest_events(protocols, guest, &events, healthy, case.host_destroys);
    }
    match (framed, answered, rejected) {
        (false, ..) => Outcome::Unframed,
        (true, _, true) => Outcome::Rejected,
        (true, true, false) => Outcome::Healthy,
        (true, false, false) => {
            panic!("the proxy closed the guest without a wl_display.error")
        }
    }
}

/// Guest objects the server created that [`check_guest_events`] saw, across
/// all cases: evidence that their IDs were checked.
static GUEST_SERVER_OBJECTS: AtomicU64 = AtomicU64::new(0);

/// Check `events` against what the proxy should send the guest: each comes
/// from, and names, only objects the guest has, live or destroyed and
/// awaiting their `wl_display.delete_id`, with an opcode its interface has;
/// a `delete_id` names an object the guest destroyed or a frame callback
/// the compositor destroyed with its surface, which the guest then no
/// longer has, and in a `healthy` session, which answered the closing sync,
/// every such frame callback gets one; and an object the server creates
/// gets an ID from the server range that no guest object has. The guest
/// never destroys an object the server created, so each needs an ID of its
/// own. When the compositor destroys live objects of an interface on its
/// own (`host_destroys`), a `delete_id` may also name one of those. A
/// `delete_id` hands its ID on: the next object the guest or the server
/// creates under it is a new object.
fn check_guest_events(
    protocols: &Protocols,
    guest: &GuestView,
    events: &[u8],
    healthy: bool,
    host_destroys: Option<&str>,
) {
    let mut interfaces: BTreeMap<u32, &str> = guest
        .interfaces
        .iter()
        .map(|(&id, interface)| (id, interface.as_str()))
        .collect();
    // The objects that await their delete_id.
    let mut destroyed = guest.destroyed.clone();
    let mut cancelled = guest.cancelled.clone();
    for (header, payload) in frames(events) {
        let (sender, opcode) = (header.sender_id, header.opcode);
        let interface = *interfaces.get(&sender).unwrap_or_else(|| {
            panic!("an event from guest object {sender}, which it does not have")
        });
        let event = protocols.events[interface]
            .get(usize::from(opcode))
            .unwrap_or_else(|| panic!("{interface} has no event {opcode}"));
        let mut message = WireMessage::new(sender, opcode, payload);
        let word = |message: &mut WireMessage| message.read_u32().expect("a whole event");
        if (sender, opcode) == (WL_DISPLAY_ID, wl_display::EVT_DELETE_ID) {
            let id = word(&mut message);
            assert!(
                destroyed.remove(&id)
                    || cancelled.remove(&id)
                    || interfaces
                        .get(&id)
                        .is_some_and(|interface| host_may_destroy(host_destroys, interface)),
                "delete_id for guest object {id}, which was not destroyed, or a second one"
            );
            // A reused ID names the new object from here on.
            match guest.reused.get(&id) {
                Some(interface) => interfaces.insert(id, interface),
                None => interfaces.remove(&id),
            };
            continue;
        }
        for arg in &event.args {
            match arg {
                Arg::Str { .. } => drop(message.read_nullable_string().expect("a string")),
                Arg::Array => drop(message.read_array().expect("an array")),
                Arg::Fd => {}
                Arg::Number { .. } => drop(word(&mut message)),
                Arg::Object { .. } => {
                    let object = word(&mut message);
                    assert!(
                        object == 0 || interfaces.contains_key(&object),
                        "{interface}.{opcode} names guest object {object}, which it does not have"
                    );
                }
                Arg::NewId { interface: created } => {
                    let created = created.as_deref().expect("events create typed objects");
                    let id = word(&mut message);
                    assert!(
                        id >= SERVER_ID_START,
                        "{interface}.{opcode} creates guest object {id:#x} outside the server range"
                    );
                    let previous = interfaces.insert(id, created);
                    assert!(
                        previous.is_none(),
                        "new server object {id:#x} reuses the ID of a guest {previous:?}"
                    );
                    GUEST_SERVER_OBJECTS.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if event.destructor {
            destroyed.insert(sender);
        }
    }
    assert!(
        !healthy || cancelled.is_empty(),
        "frame callbacks {cancelled:?} of destroyed surfaces never got their delete_id"
    );
}

/// Serve a guest that makes `sends` from one `Client`, behind a compositor
/// whose random choices follow from `seed`, and return the events it
/// received, or `None` if the proxy neither answered the guest's closing
/// sync (`callback`) nor closed the connection within [`CASE_TIMEOUT`].
/// With `host_destroys`, the compositor destroys live objects of that
/// interface on its own (see [`compositor`]).
///
/// Panics if the proxy panics, does not finish after the guest leaves, or
/// sends the host a stream the wire cannot frame.
fn exchange(
    protocols: &Arc<Protocols>,
    config: Config,
    seed: u64,
    sends: Vec<Submission>,
    framed: bool,
    callback: u32,
    host_destroys: Option<&'static str>,
) -> Option<Vec<u8>> {
    let deadline = Instant::now() + CASE_TIMEOUT;
    let (guest, proxy_guest) = UnixStream::pair().expect("guest socket pair");
    let (host, proxy_host) = UnixStream::pair().expect("host socket pair");

    let globals = protocols
        .globals
        .iter()
        .map(|(interface, version)| match interface.as_str() {
            "zcr_text_input_extension_v1" => (interface.clone(), config.text_input_extension),
            _ => (interface.clone(), *version),
        })
        .collect::<Vec<_>>();
    let (finished, proxy_finished) = std::sync::mpsc::channel();
    let proxy = std::thread::spawn(move || {
        serve(proxy_guest, proxy_host, config.context(), &globals);
        let _ = finished.send(());
    });
    let compositor = {
        let protocols = Arc::clone(protocols);
        std::thread::spawn(move || compositor(host, &protocols, host_destroys, Rng::new(seed)))
    };
    let (callbacks_done, done_callbacks) = std::sync::mpsc::channel();
    let writer = {
        let guest = guest.try_clone().expect("clone the guest socket");
        std::thread::spawn(move || {
            send_all(&guest, sends, &done_callbacks);
            // A broken stream may hold a partial message that the closing
            // sync completes; end it rather than wait for an answer.
            if !framed {
                let _ = guest.shutdown(Shutdown::Write);
            }
        })
    };

    let events = read_until_closed(&guest, callback, deadline, callbacks_done)?;
    writer.join().expect("the guest writer");
    proxy_finished
        .recv_timeout(EXIT_GRACE)
        .expect("the proxy finishes once the guest has left");
    proxy.join().expect("the proxy does not panic");
    compositor
        .join()
        .expect("the host compositor accepts every request");
    Some(events)
}

/// Serve the guest on this thread, as the accept loop serves a client,
/// behind a compositor that advertised every global in `globals`, and one
/// more `wl_output` that it then removed.
fn serve(guest: UnixStream, host: UnixStream, context: Context, globals: &[(String, u32)]) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("proxy runtime");
    runtime.block_on(async {
        let mut client = Client::new(
            WaylandConnection::new(guest.into()).expect("register the guest socket"),
            WaylandConnection::new(host.into()).expect("register the host socket"),
            context,
        );
        let (get_registry, _) = wl_display::req::get_registry(WL_DISPLAY_ID, REGISTRY);
        client.client_conn.read_buf.extend(get_registry);
        assert!(client.handle_msgs(Direction::ClientToHost).await);
        let host_registry = client
            .ctx
            .shadow_table
            .get_host_id(REGISTRY)
            .expect("the registry reaches the host");
        for (index, (interface, version)) in globals.iter().enumerate() {
            let (global, _) =
                wl_registry::evt::global(host_registry, index as u32 + 1, interface, *version)
                    .expect("a global fits a message");
            client.host_conn.read_buf.extend(global);
        }
        // An output unplugged before the guest's requests arrive. The guest
        // saw it advertised, so a bind of it crosses its removal and makes
        // an inert output.
        let unplugged = globals.len() as u32 + 1;
        let (_, output_version) = globals
            .iter()
            .find(|(interface, _)| interface == "wl_output")
            .expect("wl_output is a global");
        let global =
            wl_registry::evt::global(host_registry, unplugged, "wl_output", *output_version)
                .expect("a global fits a message");
        for (event, _) in [
            global,
            wl_registry::evt::global_remove(host_registry, unplugged),
        ] {
            client.host_conn.read_buf.extend(event);
        }
        while client.host_conn.has_complete_message() {
            assert!(client.handle_msgs(Direction::HostToClient).await);
        }
        client.run().await;
    });
}

/// Send each submission with its descriptors, waiting after a roundtrip
/// for its callback among the `done_callbacks` the guest receives, until
/// the proxy goes away.
fn send_all(socket: &UnixStream, sends: Vec<Submission>, done_callbacks: &Receiver<u32>) {
    for Submission {
        bytes,
        fds,
        then_await,
    } in sends
    {
        let raw: Vec<RawFd> = fds.iter().map(AsRawFd::as_raw_fd).collect();
        let rights = [ControlMessage::ScmRights(&raw)];
        let cmsgs: &[ControlMessage] = if raw.is_empty() { &[] } else { &rights };
        let mut sent = match sendmsg::<()>(
            socket.as_raw_fd(),
            &[IoSlice::new(&bytes)],
            cmsgs,
            MsgFlags::MSG_NOSIGNAL,
            None,
        ) {
            Ok(sent) => sent,
            Err(_) => return,
        };
        while sent < bytes.len() {
            match (&*socket).write(&bytes[sent..]) {
                Ok(written) => sent += written,
                Err(_) => return,
            }
        }
        // The reader hangs up once the proxy has closed the connection.
        if then_await.is_some_and(|awaited| !done_callbacks.iter().any(|done| done == awaited)) {
            return;
        }
    }
}

/// Read the guest's events until the proxy closes the connection, or
/// `None` once `deadline` passes.
///
/// The guest stops sending once the closing sync (`callback`) is answered
/// or an error arrives, as a client would. The sender of each event with
/// `wl_callback.done`'s opcode goes to `done_callbacks`, for the writer to
/// find the callback of a roundtrip among them; no other object has its ID.
fn read_until_closed(
    guest: &UnixStream,
    callback: u32,
    deadline: Instant,
    done_callbacks: Sender<u32>,
) -> Option<Vec<u8>> {
    let mut events = Vec::new();
    let mut parsed = 0;
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        guest
            .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))
            .expect("set the guest read timeout");
        match receive(guest, &mut events) {
            Ok(0) => return Some(events),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return None,
            Err(error) => panic!("read guest events: {error}"),
        }
        let mut finished = false;
        for (header, _) in frames(&events[parsed..]) {
            parsed += header.len;
            let event = (header.sender_id, header.opcode);
            if header.opcode == wl_callback::EVT_DONE {
                // The writer is gone once it has sent everything.
                let _ = done_callbacks.send(header.sender_id);
            }
            finished |= event == (callback, wl_callback::EVT_DONE)
                || event == (WL_DISPLAY_ID, wl_display::EVT_ERROR);
        }
        if finished {
            let _ = guest.shutdown(Shutdown::Write);
        }
    }
}

/// Frame callbacks the fake compositor destroyed with their surface,
/// across all cases.
static FRAME_CALLBACKS_CANCELLED: AtomicU64 = AtomicU64::new(0);

/// `zaura_toplevel` objects the proxy created, one per XDG toplevel of a
/// case with placement on, across all cases.
static AURA_TOPLEVELS: AtomicU64 = AtomicU64::new(0);

/// Host object IDs the fake compositor saw created again after their
/// `delete_id`, across all cases: evidence that the proxy's ID reuse runs.
static HOST_IDS_REUSED: AtomicU64 = AtomicU64::new(0);

/// The IDs the fake compositor allocates for objects it creates. Like
/// libwayland-server's map, it reuses an ID as soon as the client destroys
/// its object, last freed first.
struct ServerIds {
    next: u32,
    free: Vec<u32>,
}

impl ServerIds {
    fn allocate(&mut self) -> u32 {
        self.free.pop().unwrap_or_else(|| {
            self.next += 1;
            self.next - 1
        })
    }

    fn release(&mut self, id: u32) {
        self.free.push(id);
    }
}

/// A compositor that answers every `wl_display.sync`, offers each new
/// `wl_data_device` a `wl_data_offer`, answers each
/// `zwp_linux_buffer_params_v1.create` with a `wl_buffer` it creates, sends
/// `delete_id` after every destructor of an object the proxy created,
/// destroys a destroyed surface's frame callbacks (it never sends their
/// `done`), gives the keyboard focus to each surface the proxy commits,
/// with `host_destroys` destroys live objects of that interface on its own
/// as it answers some syncs ([`destroy_live_objects`]), answers a text input's
/// drain barrier late (see below), and does nothing else, until the proxy
/// closes the connection.
///
/// The proxy deactivates a text input and then syncs, as a drain barrier
/// that a later activation waits for. Like a slow host, the compositor
/// holds the answer to that sync until it answers anything else, so a
/// transaction the guest sends meanwhile reaches the proxy first and the
/// proxy defers the field's activation. A guest that wants the activation
/// at once first waits for a roundtrip, whose sync releases the held
/// answer. Answers stay in order.
///
/// Panics on what a real compositor would refuse: a header the wire cannot
/// frame, a request from an object the host does not have (never created,
/// or already destroyed), an opcode its interface lacks, a `new_id` that is
/// still in use, or an object argument naming an object the host does not
/// have or one of the wrong interface. Proxy-side ID translation bugs show
/// up here.
fn compositor(host: UnixStream, protocols: &Protocols, host_destroys: Option<&str>, mut rng: Rng) {
    let mut objects = BTreeMap::from([(WL_DISPLAY_ID, "wl_display".to_string())]);
    // Objects destroyed on their own, until their ID is reused (see
    // `destroy_live_objects`).
    let mut zombies = BTreeMap::new();
    let mut deleted = BTreeSet::new();
    // Each surface's frame callbacks, which never get `done` here.
    let mut frame_callbacks: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    // Exo's view, whose seat focuses the surface committed last, as if
    // each commit were a click on its window.
    let mut exo = Exo::default();
    let mut serial = 0;
    // Answers held back, and whether the last request was a deactivation.
    let mut held = Vec::new();
    let mut after_deactivate = false;
    // The compositor's pointers and touches ([`Pointers`]), and whether the
    // last request was a `reset`, whose sync is the proxy's fence.
    let mut pointers = Pointers::default();
    let mut after_reset = false;
    // Whether the last reply restarted the input method after a press, which
    // counts once the proxy's `reset` shows that the press ended a
    // composition, so that its fence was pending when the `leave` arrived.
    let mut restarted_after_press = false;
    // The seat each active host text input was activated with.
    let mut activations: BTreeMap<u32, u32> = BTreeMap::new();
    let mut server_ids = ServerIds {
        next: SERVER_ID_START,
        free: Vec::new(),
    };
    let mut requests = Vec::new();
    let mut parsed = 0;
    while receive(&host, &mut requests).expect("read host requests") > 0 {
        for (header, payload) in frames(&requests[parsed..]) {
            parsed += header.len;
            let (sender, opcode) = (header.sender_id, header.opcode);
            let zombie = zombies.contains_key(&sender);
            let interface = objects
                .get(&sender)
                .or_else(|| zombies.get(&sender))
                .unwrap_or_else(|| panic!("a request from host object {sender}, which is gone"))
                .clone();
            let request = protocols.requests[&interface]
                .get(usize::from(opcode))
                .unwrap_or_else(|| panic!("{interface} has no request {opcode}"));
            let mut message = WireMessage::new(sender, opcode, payload);
            let word = |message: &mut WireMessage| message.read_u32().expect("a whole request");
            let mut created = Vec::new();
            let mut named = Vec::new();
            for arg in &request.args {
                match arg {
                    Arg::Str { .. } => drop(message.read_nullable_string().expect("a string")),
                    Arg::Array => drop(message.read_array().expect("an array")),
                    Arg::Fd => {}
                    Arg::Number { .. } => drop(word(&mut message)),
                    Arg::Object {
                        interface: expected,
                        ..
                    } => {
                        let object = word(&mut message);
                        named.push(object);
                        let actual = objects.get(&object).or_else(|| zombies.get(&object));
                        assert!(
                            object == 0
                                || actual.is_some_and(|actual| {
                                    expected.as_ref().is_none_or(|expected| expected == actual)
                                }),
                            "{interface}.{opcode} names host object {object} ({actual:?}), \
                             not a live {expected:?}"
                        );
                    }
                    Arg::NewId { interface } => {
                        let (interface, version) = match interface {
                            Some(interface) => (interface.clone(), None),
                            None => {
                                let interface = message.read_string().expect("an interface");
                                (interface, Some(word(&mut message)))
                            }
                        };
                        created.push((word(&mut message), interface, version));
                    }
                }
            }
            let first_created = created.first().map(|&(id, ..)| id);
            for (id, interface, version) in created {
                pointers.created(sender, id, &interface, version);
                if deleted.remove(&id) {
                    HOST_IDS_REUSED.fetch_add(1, Ordering::Relaxed);
                }
                if interface == "zaura_toplevel" {
                    AURA_TOPLEVELS.fetch_add(1, Ordering::Relaxed);
                }
                zombies.remove(&id);
                let previous = objects.insert(id, interface);
                assert!(
                    previous.is_none(),
                    "new host object {id} is still {previous:?}"
                );
            }
            // A request from or naming an object the compositor destroyed
            // changes nothing. Exo models the text inputs; the rest of the
            // host is this compositor's.
            if !zombie
                && !named.iter().any(|object| zombies.contains_key(object))
                && interface.contains("text_input")
            {
                let mut message = WireMessage::new(sender, opcode, payload);
                let request = AnyRequest::from_wire(&interface, &mut message)
                    .expect("an accepted text-input request");
                exo.request(sender, &request);
            }
            match (interface.as_str(), opcode) {
                // The proxy's own answer to a press on a composition.
                ("zwp_text_input_v1", zwp_text_input_v1::REQ_RESET) if !zombie => {
                    COMPOSITIONS_ENDED_BY_PRESSES.fetch_add(1, Ordering::Relaxed);
                    if restarted_after_press {
                        RESTARTS_AFTER_PRESSES.fetch_add(1, Ordering::Relaxed);
                        restarted_after_press = false;
                    }
                }
                ("zwp_text_input_v1", zwp_text_input_v1::REQ_ACTIVATE) if !zombie => {
                    activations.insert(sender, named[0]);
                }
                ("zwp_text_input_v1", zwp_text_input_v1::REQ_DEACTIVATE) => {
                    // The proxy had not yet read the seat's delete_id.
                    if zombies.contains_key(&named[0]) {
                        DEACTIVATES_OF_DESTROYED_SEATS.fetch_add(1, Ordering::Relaxed);
                    }
                    activations.remove(&sender);
                }
                _ => {}
            }
            let mut reply = Vec::new();
            // Objects the compositor creates in reply.
            match (interface.as_str(), opcode) {
                ("wl_data_device_manager", wl_data_device_manager::REQ_GET_DATA_DEVICE) => {
                    let offer = server_ids.allocate();
                    objects.insert(offer, "wl_data_offer".to_string());
                    let device = first_created.expect("a data device");
                    reply.push(wl_data_device::evt::data_offer(device, offer).0);
                }
                ("zwp_linux_buffer_params_v1", zwp_linux_buffer_params_v1::REQ_CREATE) => {
                    let buffer = server_ids.allocate();
                    objects.insert(buffer, "wl_buffer".to_string());
                    reply.push(zwp_linux_buffer_params_v1::evt::created(sender, buffer).0);
                }
                ("wl_surface", wl_surface::REQ_COMMIT)
                    if !zombie && exo.focus() != Some(sender) =>
                {
                    let keyboard_events = reply.len();
                    let keyboards = objects
                        .iter()
                        .filter(|&(_, interface)| interface == "wl_keyboard")
                        .map(|(&keyboard, _)| keyboard);
                    for keyboard in keyboards {
                        if let Some(surface) = exo.focus() {
                            serial += 1;
                            reply.push(wl_keyboard::evt::leave(keyboard, serial, surface).0);
                        }
                        serial += 1;
                        reply.push(keyboard_enter(keyboard, serial, sender));
                    }
                    exo.set_text_input_focus(Some(sender));
                    // Which of Exo's seat observers hears of the focus first,
                    // its keyboards or its text inputs, is not modelled.
                    let text_input_events = text_input_events(exo.take_events());
                    if rng.one_in(2) {
                        reply.splice(keyboard_events..keyboard_events, text_input_events);
                    } else {
                        reply.extend(text_input_events);
                    }
                }
                // A new keyboard learns the seat's focus at once.
                ("wl_seat", wl_seat::REQ_GET_KEYBOARD) => {
                    if let Some(surface) = exo.focus() {
                        serial += 1;
                        let keyboard = first_created.expect("a keyboard");
                        reply.push(keyboard_enter(keyboard, serial, surface));
                    }
                }
                _ => {}
            }
            if (interface.as_str(), opcode) == ("wl_surface", wl_surface::REQ_FRAME) {
                let callback = first_created.expect("a frame callback");
                frame_callbacks.entry(sender).or_default().push(callback);
            }
            // Objects this request ends, which the host deletes at once.
            let mut ended = Vec::new();
            if request.destructor {
                // A destroyed surface's pending frame callbacks go first,
                // without `done`, as in weston's surface destructor.
                let cancelled = frame_callbacks.remove(&sender).unwrap_or_default();
                FRAME_CALLBACKS_CANCELLED.fetch_add(cancelled.len() as u64, Ordering::Relaxed);
                ended.extend(cancelled);
                // A destroyed object's destructor crossed its delete_id.
                if !zombie {
                    ended.push(sender);
                }
            }
            // A registry the compositor destroyed already got its delete_id.
            if (interface.as_str(), opcode) == ("wl_fixes", wl_fixes::REQ_DESTROY_REGISTRY)
                && !zombies.contains_key(&named[0])
            {
                ended.push(named[0]);
            }
            let sync = (sender, opcode) == (WL_DISPLAY_ID, wl_display::REQ_SYNC);
            let mut sync_callback = None;
            let hold = sync && after_deactivate;
            let fence = sync && after_reset;
            after_reset =
                (interface.as_str(), opcode) == ("zwp_text_input_v1", zwp_text_input_v1::REQ_RESET);
            after_deactivate = (interface.as_str(), opcode)
                == ("zwp_text_input_v1", zwp_text_input_v1::REQ_DEACTIVATE);
            if sync {
                if let Some(interface) = host_destroys.filter(|_| rng.one_in(3)) {
                    reply.extend(destroy_live_objects(
                        interface,
                        &mut objects,
                        &mut zombies,
                        &mut frame_callbacks,
                        &mut deleted,
                    ));
                }
                sync_callback = Some(
                    WireMessage::new(sender, opcode, payload)
                        .read_u32()
                        .expect("a callback ID"),
                );
            }
            for id in ended {
                objects.remove(&id);
                // libwayland-server sends no delete_id for an ID it
                // allocated; it may reuse the ID at once.
                if id >= SERVER_ID_START {
                    server_ids.release(id);
                } else {
                    deleted.insert(id);
                    reply.push(delete_id(id));
                }
            }
            // An activation whose seat ended while active, released by the
            // guest or destroyed here, ends with its seat.
            activations.retain(|text_input, seat| {
                let live = objects.contains_key(text_input);
                let seat_live = objects.get(seat).is_some_and(|seat| seat == "wl_seat");
                if live && !seat_live {
                    SEATLESS_ACTIVATIONS.fetch_add(1, Ordering::Relaxed);
                }
                live && seat_live
            });
            // A destroyed surface loses the focus without a `leave`.
            exo.forget(|id| objects.contains_key(&id));
            if sync_callback.is_some() {
                restarted_after_press = false;
                // The presses name only what the compositor still has, and
                // come before the sync's `done`, which the guest may close
                // on. So does a restart, except on the answer to the proxy's
                // own fence sync, which follows a press: the guest may have
                // closed by then, and the proxy exits when it does, before
                // it reads the `leave` the answer carries.
                let sent = pointers.presses(&exo, &objects, &mut serial, &mut rng);
                reply.extend(sent.events);
                if !fence && rng.one_in(8) && exo.destroy_input_method() {
                    INPUT_METHOD_RESTARTS.fetch_add(1, Ordering::Relaxed);
                    restarted_after_press = sent.pressed;
                }
            }
            reply.extend(text_input_events(exo.take_events()));
            if let Some(callback) = sync_callback {
                // `done` destroys the callback.
                reply.push(wl_callback::evt::done(callback, 0).0);
                objects.remove(&callback);
                deleted.insert(callback);
                reply.push(delete_id(callback));
            }
            let release = !hold && !reply.is_empty();
            held.append(&mut reply);
            if release {
                // The proxy may already have closed the connection.
                let _ = (&host).write_all(&held.concat());
                held.clear();
            }
        }
        assert!(
            !matches!(
                MessageHeader::frame(&requests[parsed..]),
                Framing::Invalid(_)
            ),
            "the proxy sent an invalid header"
        );
    }
}

/// Interfaces of objects that the compositor of a case that lets it
/// destroys on its own, one interface per case, as a host may (see
/// INVARIANTS "Object lifetimes"): the proxy pairs each with a guest object
/// ([`guest_interface`]) and keeps per-object state for it, or, for a
/// `wl_seat`, its keyboards and text inputs name it. Only a
/// `zwp_text_input_v1` can be an object only the proxy uses, once the guest
/// has destroyed its v3 text input; its `delete_id` then changes nothing.
const HOST_DESTROYED: [&str; 9] = [
    "wl_registry",
    "wl_output",
    "wl_seat",
    "wl_keyboard",
    "wl_surface",
    "wp_viewport",
    "xdg_surface",
    "xdg_toplevel",
    "zwp_text_input_v1",
];

/// The interface of the guest object a host object of `host` stands for.
fn guest_interface(host: &str) -> &str {
    match host {
        "zwp_text_input_v1" => "zwp_text_input_v3",
        _ => host,
    }
}

/// Live objects the compositor destroyed on its own, across all cases.
static LIVE_OBJECTS_HOST_DESTROYED: AtomicU64 = AtomicU64::new(0);

/// Whether a compositor that destroys live host objects of `destroyed` on
/// its own may destroy the guest's object of `interface`: one of those, or
/// the frame callback of a surface it destroyed.
fn host_may_destroy(destroyed: Option<&str>, interface: &str) -> bool {
    destroyed.is_some_and(|destroyed| {
        interface == guest_interface(destroyed)
            || (destroyed == "wl_surface" && interface == "wl_callback")
    })
}

/// Destroy every live object of `interface` the proxy created, each
/// surface's frame callbacks first, as a compositor destroys a surface.
///
/// Requests the proxy sent before it reads their `delete_id` may still come
/// from or name these objects, where a real compositor would raise an
/// error; they stay `zombies` until their ID is reused, and such requests
/// change nothing.
///
/// # Returns
///
/// The `delete_id` of each destroyed object.
fn destroy_live_objects(
    interface: &str,
    objects: &mut BTreeMap<u32, String>,
    zombies: &mut BTreeMap<u32, String>,
    frame_callbacks: &mut BTreeMap<u32, Vec<u32>>,
    deleted: &mut BTreeSet<u32>,
) -> Vec<Vec<u8>> {
    let destroyed: Vec<u32> = objects
        .iter()
        .filter(|&(&id, live)| id < SERVER_ID_START && live == interface)
        .map(|(&id, _)| id)
        .collect();
    LIVE_OBJECTS_HOST_DESTROYED.fetch_add(destroyed.len() as u64, Ordering::Relaxed);
    let callbacks: Vec<u32> = destroyed
        .iter()
        .flat_map(|surface| frame_callbacks.remove(surface).unwrap_or_default())
        .collect();
    callbacks
        .into_iter()
        .chain(destroyed)
        .map(|id| {
            let interface = objects.remove(&id).expect("a live object");
            zombies.insert(id, interface);
            deleted.insert(id);
            delete_id(id)
        })
        .collect()
}

/// Host text-input activations whose seat ended while active, across all
/// cases.
static SEATLESS_ACTIVATIONS: AtomicU64 = AtomicU64::new(0);

/// `zwp_text_input_v1.deactivate` requests naming a seat the compositor
/// had destroyed, whose `delete_id` the proxy had not read yet, across all
/// cases.
static DEACTIVATES_OF_DESTROYED_SEATS: AtomicU64 = AtomicU64::new(0);

/// Input methods that went away while attached, as when their process
/// restarts: one answer to a sync in eight takes them away. Across all
/// cases.
static INPUT_METHOD_RESTARTS: AtomicU64 = AtomicU64::new(0);

/// `zwp_text_input_v1.leave` events the compositor sent, across all cases.
static TEXT_INPUT_LEAVES: AtomicU64 = AtomicU64::new(0);

/// `events`, which Exo sent, after counting their `zwp_text_input_v1.leave`
/// events: the requests Exo is given here make it send nothing else.
fn text_input_events(events: Vec<Outgoing>) -> Vec<Vec<u8>> {
    let leaves = events
        .iter()
        .filter(|(event, _)| msg_opcode(event) == zwp_text_input_v1::EVT_LEAVE)
        .count();
    TEXT_INPUT_LEAVES.fetch_add(leaves as u64, Ordering::Relaxed);
    events.into_iter().map(|(event, _)| event).collect()
}

/// Compositor presses that made the proxy `reset` Exo's composition, across
/// all cases.
static COMPOSITIONS_ENDED_BY_PRESSES: AtomicU64 = AtomicU64::new(0);

/// Input method restarts that followed, in the same reply, a press that
/// ended a composition (the proxy `reset` Exo), across all cases.
static RESTARTS_AFTER_PRESSES: AtomicU64 = AtomicU64::new(0);

/// The compositor's pointers and touches: what they are bound at, and
/// where each pointer is.
#[derive(Default)]
struct Pointers {
    /// The version of each seat, pointer and touch.
    versions: BTreeMap<u32, u32>,
    /// The surface each pointer has entered.
    focus: BTreeMap<u32, u32>,
}

/// What [`Pointers::presses`] sends.
struct Presses {
    events: Vec<Vec<u8>>,
    /// Whether it includes a button press or a touch-down.
    pressed: bool,
}

impl Pointers {
    /// A request of `sender` created `id`, which `version` names if the
    /// request binds a global; a pointer or touch has its seat's.
    ///
    /// The ID may be one an earlier object had, which the compositor has
    /// not yet been asked to prune ([`Self::presses`]), so the new object
    /// starts with nothing of the old one's focus or version.
    fn created(&mut self, sender: u32, id: u32, interface: &str, version: Option<u32>) {
        let version = match interface {
            "wl_pointer" | "wl_touch" => self.versions.get(&sender).copied(),
            _ => version,
        };
        self.focus.remove(&id);
        match version {
            Some(version) => self.versions.insert(id, version),
            None => self.versions.remove(&id),
        };
    }

    /// Events of a compositor presses on the focused surface, as a click or
    /// tap on its window does, and the preedits they follow: each text input
    /// attached on that surface composes, then each pointer enters the
    /// surface (leaving the one it was on) and presses and releases, and
    /// each touch goes down and up, each with the frames its version has. The
    /// press reaches the proxy while the preedit is live, and ends it
    /// ([`crate::handler::text_input::end_composition_on_press`]); the run's
    /// checks (`Context::assert_no_orphans`, the guest's event checks and the
    /// exchange with Exo) judge what follows. Only live objects are named.
    fn presses(
        &mut self,
        exo: &Exo,
        objects: &BTreeMap<u32, String>,
        serial: &mut u32,
        rng: &mut Rng,
    ) -> Presses {
        let mut sent = Presses {
            events: Vec::new(),
            pressed: false,
        };
        // A pointer or surface that ended takes the pointer's focus with it.
        self.focus.retain(|pointer, surface| {
            objects.contains_key(pointer) && objects.contains_key(surface)
        });
        let Some(surface) = exo.focus() else {
            return sent;
        };
        let events = &mut sent.events;
        for text_input in exo.composing_text_inputs() {
            if rng.one_in(2) {
                *serial += 1;
                events.push(
                    zwp_text_input_v1::evt::preedit_string(text_input, *serial, "나", "나")
                        .expect("a short preedit fits a message")
                        .0,
                );
            }
        }
        let with = |interface: &str| -> Vec<u32> {
            objects
                .iter()
                .filter(|&(_, candidate)| candidate == interface)
                .map(|(&id, _)| id)
                .collect()
        };
        for pointer in with("wl_pointer") {
            let frame = self
                .versions
                .get(&pointer)
                .is_some_and(|&version| version >= 5);
            let frame = |events: &mut Vec<Vec<u8>>| {
                if frame {
                    events.push(wl_pointer::evt::frame(pointer).0);
                }
            };
            if self.focus.get(&pointer) != Some(&surface) {
                if let Some(left) = self.focus.insert(pointer, surface) {
                    *serial += 1;
                    events.push(wl_pointer::evt::leave(pointer, *serial, left).0);
                }
                *serial += 1;
                events.push(wl_pointer::evt::enter(pointer, *serial, surface, 0, 0).0);
                frame(events);
            }
            if rng.one_in(2) {
                sent.pressed = true;
                for state in [
                    wl_pointer::BUTTON_STATE_PRESSED,
                    wl_pointer::BUTTON_STATE_RELEASED,
                ] {
                    *serial += 1;
                    events.push(wl_pointer::evt::button(pointer, *serial, 0, BTN_LEFT, state).0);
                    frame(events);
                }
            }
        }
        for touch in with("wl_touch") {
            if rng.one_in(2) {
                sent.pressed = true;
                *serial += 1;
                events.push(wl_touch::evt::down(touch, *serial, 0, surface, 0, 0, 0).0);
                events.push(wl_touch::evt::frame(touch).0);
                *serial += 1;
                events.push(wl_touch::evt::up(touch, *serial, 0, 0).0);
                events.push(wl_touch::evt::frame(touch).0);
            }
        }
        sent
    }
}

/// `wl_keyboard.enter` of `keyboard` on `surface`, with no keys pressed.
fn keyboard_enter(keyboard: u32, serial: u32, surface: u32) -> Vec<u8> {
    wl_keyboard::evt::enter(keyboard, serial, surface, &[])
        .expect("an enter with no keys fits a message")
        .0
}

/// `wl_display.delete_id(id)`.
fn delete_id(id: u32) -> Vec<u8> {
    wl_display::evt::delete_id(WL_DISPLAY_ID, id).0
}

/// Receive once from `socket`, appending the bytes to `bytes`; returns the
/// byte count, 0 once the peer has closed. A plain read passes no control
/// buffer, so the kernel drops the file references that arrive instead of
/// installing descriptors.
fn receive(socket: &UnixStream, bytes: &mut Vec<u8>) -> io::Result<usize> {
    let mut buffer = [0u8; 4096];
    match (&*socket).read(&mut buffer) {
        Ok(count) => {
            bytes.extend_from_slice(&buffer[..count]);
            Ok(count)
        }
        // The peer closed with requests of ours still unread.
        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => Ok(0),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod pointer_tests {
    use super::Pointers;

    /// An ID that a new pointer reuses carries nothing of the old one: not
    /// its focus, which would give the new pointer a `button` with no
    /// `enter`, and not its version, which decides whether it gets frames.
    #[test]
    fn a_reused_pointer_id_starts_fresh() {
        let mut pointers = Pointers::default();
        pointers.created(1, 1, "wl_seat", Some(7));
        pointers.created(1, 5, "wl_pointer", None);
        pointers.focus.insert(5, 9);
        assert_eq!(pointers.versions.get(&5), Some(&7));

        pointers.created(1, 5, "wl_pointer", None);
        assert_eq!(pointers.focus.get(&5), None);
        // The seat is gone, so the new object has no version to inherit.
        pointers.versions.remove(&1);
        pointers.created(1, 5, "wl_pointer", None);
        assert_eq!(pointers.versions.get(&5), None);
    }
}
