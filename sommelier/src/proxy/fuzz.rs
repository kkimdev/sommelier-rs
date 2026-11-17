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
//! descriptors), mutates some requests, and writes the stream in random
//! pieces over a socket to `Client::run`. The host peer is a compositor
//! that advertised every global (and one `wl_output` it then removed, so a
//! bind of it crosses the removal), answers each `wl_display.sync`, and
//! deletes each object the proxy destroys.
//!
//! Every case must:
//! - end within [`CASE_TIMEOUT`] without a panic;
//! - leave the process with as many open descriptors as before it;
//! - when its framing is intact, either answer the stream's closing
//!   `wl_display.sync` (the connection stayed healthy) or send the guest a
//!   `wl_display.error` before closing it;
//! - send the host only what a real compositor accepts: requests from live
//!   host objects with opcodes their interfaces have, fresh `new_id`s, and
//!   object arguments naming live objects of the declared interface. The
//!   fake compositor deletes each destroyed object at once, so the proxy's
//!   host ID reuse runs too.
//!
//! The cases run in a child process that runs nothing else, so the
//! descriptor count is exact while other tests run in parallel. A case's
//! input follows from its seed, but the proxy's timing (how the pieces
//! arrive, which side it serves first) does not, so a failing seed may need
//! a few replays. CONTRIBUTING.md says how to run more cases, from another
//! seed, and how to replay one.

use super::{Client, Direction};
use crate::connection::{WaylandConnection, MAX_FDS_PER_SEND};
use crate::protocols::wayland::{wl_callback, wl_display, wl_registry};
use crate::state::Context;
use crate::test_support::{frames, Rng};
use crate::wire::{Framing, MessageBuilder, MessageHeader, WireMessage, WL_DISPLAY_ID};
use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IoSlice, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use wayland_codegen::protocol::{EnumItem, Interface, InterfaceItem, MessageItem, ProtocolItem};

/// Cases per run unless `SOMMELIER_FUZZ_CASES` says otherwise.
const DEFAULT_CASES: u64 = 5000;
/// How long one case may take before it counts as a hang.
const CASE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the proxy may take to finish once it has closed the guest
/// connection.
const EXIT_GRACE: Duration = Duration::from_secs(5);
/// The most requests one case sends before its closing sync.
const MAX_REQUESTS: usize = 64;
/// The guest's `wl_registry`, bound before each case's requests.
const REGISTRY: u32 = 2;
/// Set in the child process that runs the cases.
const CHILD_ENV: &str = "SOMMELIER_FUZZ_CHILD";

#[test]
fn guest_streams_cannot_crash_hang_or_leak() {
    in_own_process(|| {
        // `RUST_LOG` shows the proxy's view of a failing seed.
        let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("off"))
            .is_test(true)
            .try_init();
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
                panic!("seed {seed} failed; rerun it alone with SOMMELIER_FUZZ_SEED={seed} SOMMELIER_FUZZ_CASES=1")
            });
            *outcomes.entry(outcome).or_insert(0) += 1;
        }
        let reused = HOST_IDS_REUSED.load(Ordering::Relaxed);
        assert!(
            reused > 0,
            "no case reused a host ID, so ID reuse went untested"
        );
        println!(
            "{cases} cases from seed {first_seed} passed: {outcomes:?}, {reused} host IDs reused"
        );
    });
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).map_or(default, |value| {
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name}={value} is not a number"))
    })
}

/// Run `cases` in a child copy of this test binary that runs only this
/// test, so the process's descriptors belong to the cases alone.
fn in_own_process(cases: impl FnOnce()) {
    if std::env::var_os(CHILD_ENV).is_some() {
        return cases();
    }
    let name = "proxy::fuzz::guest_streams_cannot_crash_hang_or_leak";
    let output = std::process::Command::new(std::env::current_exe().expect("test binary path"))
        .args([name, "--exact", "--test-threads=1", "--nocapture"])
        .env(CHILD_ENV, "1")
        .output()
        .expect("run the test binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "the fuzz cases failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
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
}

impl Config {
    fn from_seed(seed: u64) -> Self {
        Self {
            gpu_accel: seed & 1 != 0,
            xdg_decoration: seed & 2 != 0,
        }
    }
}

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

struct Request {
    args: Vec<Arg>,
    /// The object version that introduced the request.
    since: u32,
    destructor: bool,
}

/// The requests of every interface the proxy is built with, and the
/// globals a compositor can advertise.
struct Protocols {
    requests: BTreeMap<String, Vec<Request>>,
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
                if let InterfaceItem::Request(_) = item {
                    interface_requests.push(Request {
                        args,
                        since: message.since.unwrap_or(1),
                        destructor: message.msg_type.as_deref() == Some("destructor"),
                    });
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

/// The globals the proxy advertises to a guest under `config`.
fn discover_globals(protocols: &Arc<Protocols>, config: Config) -> Vec<Global> {
    let callback = REGISTRY + 1;
    let sync = sync_request(callback).encode();
    let events = exchange(protocols, config, vec![(sync, Vec::new())], true, callback)
        .expect("the proxy answers a lone sync in time");
    frames(&events)
        .filter(|(header, _)| {
            (header.sender_id, header.opcode) == (REGISTRY, wl_registry::EVT_GLOBAL)
        })
        .map(|(header, payload)| {
            let mut global = WireMessage::new(header.sender_id, header.opcode, payload, &[]);
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
}

impl Message {
    fn encode(&self) -> Vec<u8> {
        MessageBuilder {
            payload: self.payload.clone(),
            fds: Vec::new(),
        }
        .build_message(self.sender, self.opcode)
    }
}

fn sync_request(callback: u32) -> Message {
    let mut payload = MessageBuilder::new();
    payload.write_u32(callback);
    Message {
        sender: WL_DISPLAY_ID,
        opcode: wl_display::REQ_SYNC,
        payload: payload.payload,
        fds: Vec::new(),
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
    next_id: u32,
}

impl<'a> Guest<'a> {
    fn new(protocols: &'a Protocols, globals: &'a [Global], rng: Rng) -> Self {
        Self {
            protocols,
            globals,
            rng,
            objects: BTreeMap::from([
                (WL_DISPLAY_ID, ("wl_display", 1)),
                (REGISTRY, ("wl_registry", 1)),
            ]),
            next_id: REGISTRY + 1,
        }
    }

    fn create(&mut self, interface: &'a str, version: u32) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.objects.insert(id, (interface, version));
        id
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
            let mut message = Message {
                sender,
                opcode: opcode as u16,
                payload: Vec::new(),
                fds: Vec::new(),
            };
            for arg in &request.args {
                self.write_arg(arg, version, &mut message);
            }
            if request.destructor {
                self.objects.remove(&sender);
            }
            return message;
        }
        self.bind()
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
            payload: payload.payload,
            fds: Vec::new(),
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
        let request = |sender: u32, opcode: u16, words: &[u32]| {
            let mut payload = MessageBuilder::new();
            words.iter().for_each(|&word| payload.write_u32(word));
            Message {
                sender,
                opcode,
                payload: payload.payload,
                fds: Vec::new(),
            }
        };
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
            request(surface, wl_surface::REQ_COMMIT, &[]),
        ]
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
        let mut payload = MessageBuilder {
            payload: std::mem::take(&mut message.payload),
            fds: Vec::new(),
        };
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
            Arg::Str { .. } => payload.write_string(&self.string()),
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
        message.payload = payload.payload;
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

    fn string(&mut self) -> String {
        match self.rng.below(4) {
            0 => String::new(),
            1 => self.rng.pick(self.globals).interface.clone(),
            2 => "x".repeat(self.rng.below(300)),
            _ => (0..self.rng.below(16))
                .map(|_| char::from(b'a' + self.rng.below(26) as u8))
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
    sends: Vec<(Vec<u8>, Vec<OwnedFd>)>,
    /// Whether every message is still whole, so the closing sync is a
    /// message of its own.
    framed: bool,
    /// The guest callback of the closing `wl_display.sync`.
    callback: u32,
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
        let mut sends: Vec<(Vec<u8>, Vec<OwnedFd>)> = Vec::new();
        for message in messages {
            let bytes = message.encode();
            let full = sends
                .last()
                .is_none_or(|(_, fds)| fds.len() + message.fds.len() > MAX_FDS_PER_SEND);
            if full || rng.one_in(3) {
                sends.push(Default::default());
            }
            let (send_bytes, send_fds) = sends.last_mut().expect("a submission");
            send_fds.extend(message.fds);
            // Sometimes the rest of the message arrives in a later receive.
            let split = if bytes.len() > 1 && rng.one_in(4) {
                1 + rng.below(bytes.len() - 1)
            } else {
                bytes.len()
            };
            send_bytes.extend_from_slice(&bytes[..split]);
            if split < bytes.len() {
                sends.push((bytes[split..].to_vec(), Vec::new()));
            }
        }

        // Rarely, break the framing itself.
        let framed = !rng.one_in(10);
        if !framed {
            let index = rng.below(sends.len());
            let bytes = &mut sends[index].0;
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
        Self {
            sends,
            framed,
            callback,
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
    let events = exchange(protocols, config, case.sends, framed, case.callback)
        .expect("the proxy answers or closes the connection in time");
    let saw = |event: (u32, u16)| {
        frames(&events).any(|(header, _)| (header.sender_id, header.opcode) == event)
    };
    let answered = saw((case.callback, wl_callback::EVT_DONE));
    let rejected = saw((WL_DISPLAY_ID, wl_display::EVT_ERROR));
    match (framed, answered, rejected) {
        (false, ..) => Outcome::Unframed,
        (true, _, true) => Outcome::Rejected,
        (true, true, false) => Outcome::Healthy,
        (true, false, false) => {
            panic!("the proxy closed the guest without a wl_display.error")
        }
    }
}

/// Serve a guest that makes `sends` from one `Client` and return the events
/// it received, or `None` if the proxy neither answered the guest's closing
/// sync (`callback`) nor closed the connection within [`CASE_TIMEOUT`].
///
/// Panics if the proxy panics, does not finish after the guest leaves, or
/// sends the host a stream the wire cannot frame.
fn exchange(
    protocols: &Arc<Protocols>,
    config: Config,
    sends: Vec<(Vec<u8>, Vec<OwnedFd>)>,
    framed: bool,
    callback: u32,
) -> Option<Vec<u8>> {
    let deadline = Instant::now() + CASE_TIMEOUT;
    let (guest, proxy_guest) = UnixStream::pair().expect("guest socket pair");
    let (host, proxy_host) = UnixStream::pair().expect("host socket pair");

    let globals = protocols.globals.clone();
    let (finished, proxy_finished) = std::sync::mpsc::channel();
    let proxy = std::thread::spawn(move || {
        let context = Context::new_for_test(config.gpu_accel, config.xdg_decoration, Vec::new());
        serve(proxy_guest, proxy_host, context, &globals);
        let _ = finished.send(());
    });
    let compositor = {
        let protocols = Arc::clone(protocols);
        std::thread::spawn(move || compositor(host, &protocols))
    };
    let writer = {
        let guest = guest.try_clone().expect("clone the guest socket");
        std::thread::spawn(move || {
            send_all(&guest, sends);
            // A broken stream may hold a partial message that the closing
            // sync completes; end it rather than wait for an answer.
            if !framed {
                let _ = guest.shutdown(Shutdown::Write);
            }
        })
    };

    let events = read_until_closed(&guest, callback, deadline)?;
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
        client.ctx.shadow_table.map_id(WL_DISPLAY_ID, WL_DISPLAY_ID);
        client
            .ctx
            .shadow_table
            .track_interface(WL_DISPLAY_ID, "wl_display".to_string());
        let mut get_registry = MessageBuilder::new();
        get_registry.write_u32(REGISTRY);
        client
            .client_conn
            .read_buf
            .extend(get_registry.build_message(WL_DISPLAY_ID, wl_display::REQ_GET_REGISTRY));
        assert!(client.handle_msgs(Direction::ClientToHost).await);
        let host_registry = client
            .ctx
            .shadow_table
            .get_host_id(REGISTRY)
            .expect("the registry reaches the host");
        for (index, (interface, version)) in globals.iter().enumerate() {
            let mut global = MessageBuilder::new();
            global.write_u32(index as u32 + 1);
            global.write_string(interface);
            global.write_u32(*version);
            client
                .host_conn
                .read_buf
                .extend(global.build_message(host_registry, wl_registry::EVT_GLOBAL));
        }
        // An output unplugged before the guest's requests arrive. The guest
        // saw it advertised, so a bind of it crosses its removal and makes
        // an inert output.
        let unplugged = globals.len() as u32 + 1;
        let (_, output_version) = globals
            .iter()
            .find(|(interface, _)| interface == "wl_output")
            .expect("wl_output is a global");
        let mut global = MessageBuilder::new();
        global.write_u32(unplugged);
        global.write_string("wl_output");
        global.write_u32(*output_version);
        let mut remove = MessageBuilder::new();
        remove.write_u32(unplugged);
        for event in [
            global.build_message(host_registry, wl_registry::EVT_GLOBAL),
            remove.build_message(host_registry, wl_registry::EVT_GLOBAL_REMOVE),
        ] {
            client.host_conn.read_buf.extend(event);
        }
        while client.host_conn.has_complete_message() {
            assert!(client.handle_msgs(Direction::HostToClient).await);
        }
        client.run().await;
    });
}

/// Send each submission with its descriptors, until the proxy goes away.
fn send_all(socket: &UnixStream, sends: Vec<(Vec<u8>, Vec<OwnedFd>)>) {
    for (bytes, fds) in sends {
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
    }
}

/// Read the guest's events until the proxy closes the connection, or
/// `None` once `deadline` passes.
///
/// The guest stops sending once the closing sync (`callback`) is answered
/// or an error arrives, as a client would.
fn read_until_closed(guest: &UnixStream, callback: u32, deadline: Instant) -> Option<Vec<u8>> {
    let mut events = Vec::new();
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
        let finished = frames(&events).any(|(header, _)| {
            matches!(
                (header.sender_id, header.opcode),
                (id, wl_callback::EVT_DONE) if id == callback
            ) || (header.sender_id, header.opcode) == (WL_DISPLAY_ID, wl_display::EVT_ERROR)
        });
        if finished {
            let _ = guest.shutdown(Shutdown::Write);
        }
    }
}

/// Host object IDs the fake compositor saw created again after their
/// `delete_id`, across all cases: evidence that the proxy's ID reuse runs.
static HOST_IDS_REUSED: AtomicU64 = AtomicU64::new(0);

/// A compositor that answers every `wl_display.sync`, sends `delete_id`
/// after every destructor, and does nothing else, until the proxy closes
/// the connection.
///
/// Panics on what a real compositor would refuse: a header the wire cannot
/// frame, a request from an object the host does not have (never created,
/// or already destroyed), an opcode its interface lacks, a `new_id` that is
/// still in use, or an object argument naming an object the host does not
/// have or one of the wrong interface. Proxy-side ID translation bugs show
/// up here.
fn compositor(host: UnixStream, protocols: &Protocols) {
    let mut objects = BTreeMap::from([(WL_DISPLAY_ID, "wl_display".to_string())]);
    let mut deleted = BTreeSet::new();
    let mut requests = Vec::new();
    let mut parsed = 0;
    while receive(&host, &mut requests).expect("read host requests") > 0 {
        for (header, payload) in frames(&requests[parsed..]) {
            parsed += header.len;
            let (sender, opcode) = (header.sender_id, header.opcode);
            let interface = objects
                .get(&sender)
                .unwrap_or_else(|| panic!("a request from host object {sender}, which is gone"))
                .clone();
            let request = protocols.requests[&interface]
                .get(usize::from(opcode))
                .unwrap_or_else(|| panic!("{interface} has no request {opcode}"));
            let mut message = WireMessage::new(sender, opcode, payload, &[]);
            let word = |message: &mut WireMessage| message.read_u32().expect("a whole request");
            let mut created = Vec::new();
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
                        let actual = objects.get(&object);
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
                        let interface = match interface {
                            Some(interface) => interface.clone(),
                            None => {
                                let interface = message.read_string().expect("an interface");
                                word(&mut message);
                                interface
                            }
                        };
                        created.push((word(&mut message), interface));
                    }
                }
            }
            for (id, interface) in created {
                if deleted.remove(&id) {
                    HOST_IDS_REUSED.fetch_add(1, Ordering::Relaxed);
                }
                let previous = objects.insert(id, interface);
                assert!(
                    previous.is_none(),
                    "new host object {id} is still {previous:?}"
                );
            }
            // Objects this request ends, which the host deletes at once.
            let mut ended = Vec::new();
            let mut reply = Vec::new();
            if request.destructor {
                ended.push(sender);
            }
            if (sender, opcode) == (WL_DISPLAY_ID, wl_display::REQ_SYNC) {
                let callback = WireMessage::new(sender, opcode, payload, &[])
                    .read_u32()
                    .expect("a callback ID");
                let mut done = MessageBuilder::new();
                done.write_u32(0);
                // `done` destroys the callback.
                reply.push(done.build_message(callback, wl_callback::EVT_DONE));
                ended.push(callback);
            }
            for id in ended {
                objects.remove(&id);
                deleted.insert(id);
                reply.push(delete_id(id));
            }
            // The proxy may already have closed the connection.
            let _ = (&host).write_all(&reply.concat());
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

/// `wl_display.delete_id(id)`.
fn delete_id(id: u32) -> Vec<u8> {
    let mut event = MessageBuilder::new();
    event.write_u32(id);
    event.build_message(WL_DISPLAY_ID, wl_display::EVT_DELETE_ID)
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
