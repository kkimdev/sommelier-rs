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

//! Randomized guest streams through a whole [`Client`](super::Client).
//!
//! A guest client controls the bytes and descriptors it sends, and the proxy
//! must survive whatever arrives. Each case builds a request stream from the
//! vendored protocol XML (requests of live objects, typed arguments, real
//! descriptors), mutates some requests, and splits the stream into random
//! pieces. Half the cases end like a client typing into text fields,
//! switching the focus between surfaces and waiting for each with a
//! roundtrip, and sometimes releasing its seat. The host advertises every
//! global, answers as the shared host display does ([`crate::sim::display`],
//! with the Exo model behind it), and adds what a busy user and a slow host
//! do ([`world`]): it focuses each surface the proxy commits, presses on it
//! while a preedit is live, restarts its input method, holds its answer to a
//! text input's drain barrier, and in some cases ends live objects of one
//! interface on its own ([`HOST_DESTROYED`]).
//!
//! Each case runs on the DST's scheduler ([`world`]): every piece of the
//! stream and every message the proxy and the host send arrives after a
//! drawn latency, so a seed fixes the case's input and its timing, and
//! replays it exactly. Every case must:
//! - end without a panic;
//! - leave the process with as many open descriptors as before it;
//! - when its framing is intact, either answer the stream's closing
//!   `wl_display.sync` or send the guest a `wl_display.error`;
//! - send the guest at most one `wl_display.error`, and nothing after it;
//! - never end a session with `SessionEnd::Internal`: that is the proxy
//!   failing, never a rejection of the guest, and it is counted even when
//!   a protocol error overrides it;
//! - leave no state behind an ended object after any batch that keeps the
//!   session (`Context::assert_no_orphans`);
//! - send each peer only what the protocol allows, as the session's peers
//!   and the host display check it (`test_support::wire_peer`): messages
//!   from, and naming, only objects the peer has, of the declared
//!   interface and no newer than their version, new objects under free IDs
//!   of the sender's range, and a `wl_display.delete_id` only for an
//!   object a destructor ended, once. The host deletes each destroyed
//!   object at once, so the proxy's host ID reuse runs too;
//! - when the guest's stream is whole and undamaged, send the guest a
//!   `delete_id` for a live object only for a frame callback the host
//!   cancelled with its surface, or an object the host ended on its own,
//!   and, in a healthy session, one for each cancelled callback.
//!
//! What Exo holds of a text field at the end is the DST's to check
//! ([`crate::sim`]).
//!
//! A few cases also run through `Client::run` over real sockets
//! ([`sockets`]), where the kernel decides the timing, to check that its
//! event loop serves both sides and closes every descriptor. The cases
//! run in a child process that runs nothing else, so the descriptor count
//! is exact while other tests run in parallel. CONTRIBUTING.md says how to
//! run more cases, from another seed, and how to replay one.

// Generated requests are written argument by argument and then mutated,
// which no encoder can do.
#![allow(clippy::disallowed_methods)]

mod host;
mod sockets;
mod typing;
mod world;

use crate::connection::MAX_FDS_PER_SEND;
use crate::protocols::wayland::{wl_callback, wl_display, wl_fixes, wl_registry, wl_surface};
use crate::protocols::AnyEvent;
use crate::state::Context;
use crate::test_support::wire_peer::MayEnd;
use crate::test_support::TestConnection;
use crate::test_support::{env_u64, Rng};
use crate::wire::{MessageBuilder, MessageHeader, WireMessage, WL_DISPLAY_ID};
use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::OwnedFd;
use std::path::Path;
use wayland_codegen::protocol::{EnumItem, Interface, InterfaceItem, MessageItem, ProtocolItem};
use world::World;

/// Cases per run unless `SOMMELIER_FUZZ_CASES` says otherwise.
const DEFAULT_CASES: u64 = 6000;
/// A run sends one case in this many through `Client::run` over real
/// sockets as well.
const SOCKET_SHARE: u64 = 30;
/// The fewest cases a run needs before it must have exercised ID reuse and
/// server objects; a replay of one seed need not.
const COVERAGE_CASES: u64 = 1000;

/// What a run counts across its cases, evidence of the paths they reached;
/// the run prints them.
#[derive(Clone, Copy, Debug, Default)]
struct Counts {
    /// Guest objects the server created, whose IDs the guest checked.
    guest_server_objects: u64,
    /// Host object IDs the host saw created again after their `delete_id`:
    /// evidence that the proxy's ID reuse runs.
    host_ids_reused: u64,
    /// Frame callbacks the host destroyed with their surface.
    frame_callbacks_cancelled: u64,
    /// Live objects the host ended on its own.
    live_objects_host_destroyed: u64,
    /// `zwp_text_input_v1.leave` events the host sent.
    text_input_leaves: u64,
    /// Input methods that went away while attached, as when their process
    /// restarts.
    input_method_restarts: u64,
    /// Host text-input activations whose seat ended while active.
    seatless_activations: u64,
    /// `zwp_text_input_v1.deactivate` requests naming a seat the host had
    /// ended, whose `delete_id` the proxy had not read yet.
    deactivates_of_destroyed_seats: u64,
    /// Host presses that made the proxy `reset` Exo's composition.
    compositions_ended_by_presses: u64,
    /// Input method restarts that followed, in the same answer, a press
    /// that ended a composition (the proxy `reset` Exo).
    restarts_after_presses: u64,
    /// `zaura_toplevel` objects the proxy created, one per XDG toplevel of
    /// a case with placement on.
    aura_toplevels: u64,
}

impl Counts {
    /// The counters a run of [`COVERAGE_CASES`] or more must raise, each
    /// with the path it shows.
    fn reached(&self) -> [(u64, &'static str); 9] {
        [
            (self.aura_toplevels, "no case created an Aura toplevel, so placement's went untested"),
            (self.compositions_ended_by_presses, "no press ended a composition, so the reset fence went untested"),
            (self.restarts_after_presses, "no input method restarted after a press that ended a composition, so a leave during the reset fence went untested"),
            (self.host_ids_reused, "no case reused a host ID, so ID reuse went untested"),
            (self.guest_server_objects, "no checked case created a server object, so their IDs went untested"),
            (self.frame_callbacks_cancelled, "no case cancelled a frame callback, so host-destroyed objects went untested"),
            (self.live_objects_host_destroyed, "the host destroyed no live object, so forgetting one went untested"),
            (self.input_method_restarts, "no input method restarted while attached, so Exo's own detach went untested"),
            (self.seatless_activations, "no activation outlived its seat, so ending one without a seat went untested"),
        ]
    }
}

impl std::ops::AddAssign for Counts {
    fn add_assign(&mut self, other: Self) {
        self.guest_server_objects += other.guest_server_objects;
        self.host_ids_reused += other.host_ids_reused;
        self.frame_callbacks_cancelled += other.frame_callbacks_cancelled;
        self.live_objects_host_destroyed += other.live_objects_host_destroyed;
        self.text_input_leaves += other.text_input_leaves;
        self.input_method_restarts += other.input_method_restarts;
        self.seatless_activations += other.seatless_activations;
        self.deactivates_of_destroyed_seats += other.deactivates_of_destroyed_seats;
        self.compositions_ended_by_presses += other.compositions_ended_by_presses;
        self.restarts_after_presses += other.restarts_after_presses;
        self.aura_toplevels += other.aura_toplevels;
    }
}

/// The most random requests one case sends before its closing sync.
const MAX_REQUESTS: usize = 64;
/// The guest's `wl_registry`, which it binds and waits for with a
/// roundtrip on [`SETUP_CALLBACK`] before each case's requests.
const REGISTRY: u32 = 2;
const SETUP_CALLBACK: u32 = 3;

#[test]
fn guest_streams_cannot_crash_hang_or_leak() {
    in_own_process(
        "proxy::fuzz::guest_streams_cannot_crash_hang_or_leak",
        || {
            // `RUST_LOG` shows the proxy's view of a failing seed.
            install_logger();
            let cases = env_u64("SOMMELIER_FUZZ_CASES", DEFAULT_CASES);
            let first_seed = env_u64("SOMMELIER_FUZZ_SEED", 0);
            let protocols = Protocols::load();
            let runtime = runtime();
            let baseline = open_fds();
            let mut outcomes = BTreeMap::new();
            let mut counts = Counts::default();
            for seed in first_seed..first_seed + cases {
                let (outcome, case_counts) = check_case(&protocols, &runtime, seed, &baseline);
                *outcomes.entry(outcome).or_insert(0) += 1;
                counts += case_counts;
            }
            if cases >= COVERAGE_CASES {
                for (count, unreached) in counts.reached() {
                    assert!(count > 0, "{unreached}");
                }
            }
            println!("{cases} cases from seed {first_seed} passed: {outcomes:?}, {counts:?}");
        },
    );
}

/// Run the case of `seed`, which must leave the process with the
/// descriptors of `baseline`, and, one case in 25, again, and it must
/// replay exactly; returns how it ended and what it counted. A failure
/// prints the end of the case's trace and names its seed.
fn check_case(
    protocols: &Protocols,
    runtime: &tokio::runtime::Runtime,
    seed: u64,
    baseline: &[std::path::PathBuf],
) -> (Outcome, Counts) {
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut trace = Vec::new();
        let traced = (seed % 50 < 2).then_some(&mut trace);
        let (outcome, counts) = runtime.block_on(run_case(protocols, seed, traced));
        assert_eq!(open_fds(), baseline, "descriptors leaked");
        if seed % 50 < 2 {
            let mut again = Vec::new();
            runtime.block_on(run_case(protocols, seed, Some(&mut again)));
            let first = trace.iter().zip(&again).position(|(a, b)| a != b);
            assert!(
                again == trace,
                "the case did not replay: its traces first differ at line {first:?}"
            );
        }
        (outcome, counts)
    }));
    checked.unwrap_or_else(|_| {
        // The case replays exactly, so a second run shows its trace.
        let mut trace = Vec::new();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(run_case(protocols, seed, Some(&mut trace)))
        }));
        let shown = env_u64("SOMMELIER_FUZZ_TRACE_LINES", 60) as usize;
        for line in &trace[trace.len().saturating_sub(shown)..] {
            println!("    {line}");
        }
        panic!(
            "seed {seed} failed; rerun it alone with SOMMELIER_FUZZ_SEED={seed} \
             SOMMELIER_FUZZ_CASES=1, which replays it exactly"
        )
    })
}

/// The scheduled cases replay exactly: a seed fixes the timing too.
#[test]
fn a_fuzz_case_replays_exactly() {
    let protocols = Protocols::load();
    let runtime = runtime();
    for seed in [1, 17] {
        let [mut first, mut again] = [Vec::new(), Vec::new()];
        runtime.block_on(run_case(&protocols, seed, Some(&mut first)));
        runtime.block_on(run_case(&protocols, seed, Some(&mut again)));
        assert!(first.len() > 50, "a short trace: {first:?}");
        assert_eq!(first, again);
    }
}

/// `Client::run` serves the cases over real sockets, in whatever order the
/// kernel delivers their pieces, until the guest leaves, and closes every
/// descriptor.
#[test]
fn client_run_serves_guest_streams_over_sockets() {
    in_own_process(
        "proxy::fuzz::client_run_serves_guest_streams_over_sockets",
        || {
            install_logger();
            let first_seed = env_u64("SOMMELIER_FUZZ_SEED", 0);
            let cases = env_u64("SOMMELIER_FUZZ_CASES", DEFAULT_CASES).div_ceil(SOCKET_SHARE);
            let protocols = Protocols::load();
            let runtime = runtime();
            let mut advertised = BTreeMap::new();
            let baseline = open_fds();
            let mut outcomes = BTreeMap::new();
            for seed in first_seed..first_seed + cases {
                let config = Config::from_seed(seed);
                let advertised = advertised
                    .entry(config)
                    .or_insert_with(|| runtime.block_on(advertised_globals(&protocols, config)));
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let case = Case::generate(&protocols, advertised, seed);
                    let mut sends = vec![registry_roundtrip()];
                    sends.extend(case.sends);
                    take_proxy_errors();
                    let exchange = sockets::Exchange {
                        config,
                        seed,
                        globals: host_globals(&protocols, config),
                        sends,
                        framed: case.framed,
                        callback: case.callback,
                        destroys: case.host_destroys,
                        may_end: may_end(case.host_destroys),
                        checked: case.checked,
                    };
                    let events = sockets::exchange(exchange)
                        .expect("the proxy answers or closes the connection in time");
                    outcome(seed, &events, case.framed, case.callback, None)
                }))
                .unwrap_or_else(|_| {
                    panic!(
                        "socket seed {seed} failed; rerun it alone with \
                         SOMMELIER_FUZZ_SEED={seed} SOMMELIER_FUZZ_CASES=1, though the \
                         kernel decides its timing"
                    )
                });
                assert_eq!(
                    open_fds(),
                    baseline,
                    "socket seed {seed} leaked descriptors"
                );
                *outcomes.entry(outcome).or_insert(0) += 1;
            }
            println!("{cases} socket cases from seed {first_seed} passed: {outcomes:?}");
        },
    );
}

/// A current-thread runtime for the cases, created before a run counts
/// its descriptors: Tokio's first one opens a socket pair for the whole
/// process.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
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

/// Run `cases`, the body of test `test`, in a child copy of this test
/// binary that runs only it, so the process's descriptors belong to the
/// cases alone.
fn in_own_process(test: &str, cases: impl FnOnce()) {
    let stdout = crate::test_support::in_own_process(test, cases);
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
        let mut connection = TestConnection::default();
        connection.settings.gpu_accel = self.gpu_accel;
        connection.settings.xdg_decoration = self.xdg_decoration;
        if self.placement {
            connection.facts.placement =
                crate::placement_config::PlacementConfig::active_for_test(&[]);
        }
        connection.context()
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

/// A request, as the protocol XML declares it.
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
        let mut created = BTreeSet::new();
        for interface in &interfaces {
            let mut interface_requests = Vec::new();
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
                if let InterfaceItem::Request(_) = item {
                    interface_requests.push(signature);
                }
            }
            requests.insert(interface.name.clone(), interface_requests);
        }
        let globals = interfaces
            .iter()
            .filter(|interface| {
                interface.name != "wl_display" && !created.contains(&interface.name)
            })
            .map(|interface| (interface.name.clone(), interface.version))
            .collect();
        Self { requests, globals }
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

/// The globals the host of a case advertises under `config`: every one,
/// and one more output, which it removes at once
/// ([`crate::sim::display::Display::unplugging_last`]).
fn host_globals(protocols: &Protocols, config: Config) -> Vec<(String, u32)> {
    let mut globals: Vec<_> = protocols
        .globals
        .iter()
        .map(|(interface, version)| match interface.as_str() {
            "zcr_text_input_extension_v1" => (interface.clone(), config.text_input_extension),
            _ => (interface.clone(), *version),
        })
        .collect();
    let output = globals
        .iter()
        .find(|(interface, _)| interface == "wl_output");
    let output = output.expect("wl_output is a global").clone();
    globals.push(output);
    globals
}

/// The globals the proxy advertises to a guest under `config`, which a
/// case's guest binds.
async fn advertised_globals(protocols: &Protocols, config: Config) -> Vec<Global> {
    let mut world = World::new(config, host_globals(protocols, config), 0, None);
    world.send(vec![registry_roundtrip()], SETUP_CALLBACK);
    world.run().await;
    globals(world.received())
}

/// The globals `events` advertise on the guest's registry.
fn globals(events: &[(u32, AnyEvent)]) -> Vec<Global> {
    events
        .iter()
        .filter_map(|(sender, event)| match event {
            AnyEvent::WlRegistry(wl_registry::Event::Global {
                name,
                interface,
                version,
            }) if *sender == REGISTRY => Some(Global {
                name: *name,
                interface: interface.clone(),
                version: *version,
            }),
            _ => None,
        })
        .collect()
}

/// The guest's first submission: its registry, and a roundtrip on it, as
/// `wl_display_roundtrip` after `wl_display_get_registry`.
fn registry_roundtrip() -> Submission {
    let mut bytes = request(WL_DISPLAY_ID, wl_display::REQ_GET_REGISTRY, &[REGISTRY]).encode();
    bytes.extend(sync_request(SETUP_CALLBACK).encode());
    Submission {
        bytes,
        fds: Vec::new(),
        then_await: Some(SETUP_CALLBACK),
    }
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
    /// Each live surface's frame callbacks.
    frame_callbacks: BTreeMap<u32, Vec<u32>>,
    /// The frame callbacks of surfaces the guest destroyed, which the host
    /// destroys with their surface.
    cancelled: BTreeSet<u32>,
    next_id: u32,
}

impl<'a> Guest<'a> {
    fn new(protocols: &'a Protocols, globals: &'a [Global], rng: Rng) -> Self {
        let objects = BTreeMap::from([
            (WL_DISPLAY_ID, ("wl_display", 1)),
            (REGISTRY, ("wl_registry", 1)),
        ]);
        Self {
            protocols,
            globals,
            rng,
            objects,
            frame_callbacks: BTreeMap::new(),
            cancelled: BTreeSet::new(),
            next_id: SETUP_CALLBACK + 1,
        }
    }

    fn create(&mut self, interface: &'a str, version: u32) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.objects.insert(id, (interface, version));
        id
    }

    /// Create an object of `interface` under `id`, an object the guest
    /// destroyed and whose `delete_id` it has received.
    fn reuse(&mut self, id: u32, interface: &'a str, version: u32) -> u32 {
        self.objects.insert(id, (interface, version));
        id
    }

    fn destroy(&mut self, id: u32) {
        if self.objects.remove(&id).is_some() {
            let cancelled = self.frame_callbacks.remove(&id).unwrap_or_default();
            self.cancelled.extend(cancelled);
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
    /// Whether every request is whole and undamaged, so the guest knows
    /// which of its objects the host ends.
    checked: bool,
    /// The frame callbacks of surfaces the guest destroyed.
    cancelled: BTreeSet<u32>,
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
            checked: framed && mutations == 0,
            cancelled: guest.cancelled,
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

/// Run the case of `seed` on the scheduler and check its properties (see
/// the module documentation), its trace in `trace`, if any; returns how it
/// ended and what it counted.
async fn run_case(
    protocols: &Protocols,
    seed: u64,
    trace: Option<&mut Vec<String>>,
) -> (Outcome, Counts) {
    let config = Config::from_seed(seed);
    let mut world = World::new(config, host_globals(protocols, config), seed, trace);
    world.send(vec![registry_roundtrip()], SETUP_CALLBACK);
    world.run().await;
    let advertised = globals(world.received());
    let case = Case::generate(protocols, &advertised, seed);
    let framed = case.framed;
    take_proxy_errors();
    world.host_ends(case.host_destroys, may_end(case.host_destroys));
    world.send(case.sends, case.callback);
    world.run().await;
    let checked = case.checked.then_some(case.cancelled);
    let outcome = outcome(seed, world.received(), framed, case.callback, checked);
    (outcome, world.counts())
}

/// Which guest objects the proxy may end on its own, besides the frame
/// callbacks of a surface that ended: those whose host objects the host
/// ends, of `destroys`, if any (see [`host_may_destroy`]).
fn may_end(destroys: Option<&'static str>) -> MayEnd {
    Box::new(move |_, interface| host_may_destroy(destroys, interface))
}

/// How the case of `seed`, whose stream was `framed` and closed with the
/// sync of `callback`, ended, given the `events` the guest received: at
/// most one `wl_display.error`, its last event. A case whose guest knows
/// its requests, and so the frame callbacks the host `cancelled` with
/// their surfaces, and that ended healthy, got their `delete_id`s.
fn outcome(
    seed: u64,
    events: &[(u32, AnyEvent)],
    framed: bool,
    callback: u32,
    cancelled: Option<BTreeSet<u32>>,
) -> Outcome {
    let answered = events.iter().any(|(sender, event)| {
        *sender == callback
            && matches!(event, AnyEvent::WlCallback(wl_callback::Event::Done { .. }))
    });
    let is_error =
        |event: &AnyEvent| matches!(event, AnyEvent::WlDisplay(wl_display::Event::Error { .. }));
    let errors = events.iter().filter(|(_, event)| is_error(event)).count();
    let last_is_error = events.last().is_some_and(|(_, event)| is_error(event));
    let outcome = judge(seed, framed, answered, errors, last_is_error);
    if let Some(mut cancelled) = cancelled.filter(|_| outcome == Outcome::Healthy) {
        for (_, event) in events {
            if let AnyEvent::WlDisplay(wl_display::Event::DeleteId { id }) = event {
                cancelled.remove(id);
            }
        }
        assert!(
            cancelled.is_empty(),
            "frame callbacks {cancelled:?} of destroyed surfaces never got their delete_id"
        );
    }
    outcome
}

/// How a case of `seed` whose stream was `framed` ended, given whether the
/// proxy `answered` its closing sync and the `errors` it sent the guest:
/// at most one, its last event.
fn judge(seed: u64, framed: bool, answered: bool, errors: usize, last_is_error: bool) -> Outcome {
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
        !rejected || last_is_error,
        "the guest received events after its wl_display.error"
    );
    match (framed, answered, rejected) {
        (false, ..) => Outcome::Unframed,
        (true, _, true) => Outcome::Rejected,
        (true, true, false) => Outcome::Healthy,
        (true, false, false) => {
            panic!("the proxy neither answered the closing sync nor sent a wl_display.error")
        }
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

/// Whether a compositor that destroys live host objects of `destroyed` on
/// its own may destroy the guest's object of `interface`: one of those, or
/// the frame callback of a surface it destroyed.
fn host_may_destroy(destroyed: Option<&str>, interface: &str) -> bool {
    destroyed.is_some_and(|destroyed| {
        interface == guest_interface(destroyed)
            || (destroyed == "wl_surface" && interface == "wl_callback")
    })
}
