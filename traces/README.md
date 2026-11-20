# Recorded host traces

Each `*.trace` file here is the wire of one client connection, recorded
by a proxy talking to a real Chromebook's Exo, and a regression test of
the proxy, which `every_recorded_trace_replays` (in
`sommelier/src/proxy/replay.rs`) replays. The IME core's Exo mirror and
the DST's Exo model both come from one reading of Chromium; a trace
holds the mirror to what Exo really sent.

The format is described once, in `sommelier/src/trace.rs`: text, a header
and the connection's facts, then one message a line, in hex, in the
order the proxy handled and sent them. Of the descriptors, only a
keymap's text is recorded: the keyboard layout, which decides what the
proxy makes of each key.

## What a trace holds

**A trace holds every message the app and the host exchanged**, which
includes:

- everything typed while it was recorded, and what the input method
  composed;
- the text of the field typed into, as the app reports it to the input
  method (the surrounding text), so the contents of a document or field
  that was open;
- window titles and application IDs, clipboard and drag MIME types, and
  each output's make, model and geometry;
- the keyboard layout.

So record in a fresh, empty field, as the sample GUI below gives, never
in an app with a document open, and type only the scenario: never a
password or anything private. Before you share a trace or commit it,
read it decoded, every message with its strings:

```bash
SOMMELIER_TEST_TRACE="$PWD/traces/held-space-after-ga.trace" \
  cargo test --locked -p sommelier --bins print_a_trace -- --ignored --nocapture
```

## What a replay checks

The replay feeds the proxy what each peer sent, in the batches the
recorded proxy handled, and fails unless:

- the proxy keeps the protocol with both peers and holds no state for an
  ended object, as in every `Session` test;
- no Exo mirror diverges: every event of Exo's agrees with the text the
  mirror predicts;
- the IME core drops none of Exo's deletes or preedit regions because
  its mirror cannot measure them;
- the proxy handles every message as it did, and sends each peer the
  same bytes, with as many descriptors in each message. Which descriptor
  a message carries is not compared, since a trace holds none.

A trace pins the proxy's output too, so a change that alters what the
proxy sends in a recorded scenario fails its replay. If the change is
right, record the scenario again; never edit a trace by hand.

The replay stands in for what a trace does not hold: a keymap is the
text the trace recorded, which must be as long as its event says, and
any other descriptor an empty memfd; and a VirtWL channel is a stand-in
that allocates memfds and pipes. It cannot stand in for a render node, window placement, host
accelerators or `--gpu-accel`, so a trace recorded with any of them is
refused, with the facts it states. The steps below record without them.

## Recording a trace on a Chromebook

Record with a **separate** proxy on a spare socket, never the Sommelier
service your apps use (`wayland-2`; see
[Manual verification](../docs/INVARIANTS.md#manual-verification)), and
with one app that connects to it alone. In the Crostini terminal, from a
checkout of this repository:

1. Build the proxy, and the sample GUI if you will type into it:

   ```bash
   cargo build --locked --release -p sommelier -p sommelier-test-gui
   ```

2. Start the recording proxy on `wayland-9`, with recording on, no render
   node, no placement bindings, no host accelerators and no wheel steps,
   and leave it running. The trace directory must be local (the proxy
   writes it as it serves), and a trace ends at 256 MiB:

   ```bash
   mkdir -m 700 -p ~/sommelier-traces
   env -u SOMMELIER_ACCELERATORS \
     SOMMELIER_TRACE_DIR="$HOME/sommelier-traces" \
     SOMMELIER_WHEEL_FROM_TOUCHPAD=off \
     SOMMELIER_DRM_DEVICE=/nonexistent \
     XDG_CONFIG_HOME="$(mktemp -d)" \
     RUST_LOG=info \
     ./target/release/sommelier --virtio-wl /dev/wl0 wayland-9
   ```

   It logs that it records every client's traffic. Do not pass
   `--gpu-accel`.

3. In a second terminal, start one app on `wayland-9` that draws in
   software (the proxy has no render node) and starts a process of its
   own, not a window of an app already running:

   ```bash
   WAYLAND_DISPLAY=wayland-9 LIBGL_ALWAYS_SOFTWARE=1 \
     ./target/release/sommelier-test-gui
   ```

   A GTK3 app works too (`WAYLAND_DISPLAY=wayland-9 GDK_BACKEND=wayland
   gedit --standalone`), but open no document in it. The proxy logs the
   file it records the app in: `~/sommelier-traces/<time>-<pid>-<n>.trace`.

4. In the empty field, type the scenario, and only that: for example,
   with the Korean input method, type 가 and hold Space. Keep it short;
   one scenario a trace.

5. Close the app, so its connection ends and its trace is complete, then
   stop the proxy with Ctrl-C.

6. Name the trace for what it shows, put it here, and add comment lines
   at its top: the scenario, the input method and the ChromeOS version
   (`chrome://version`).

   ```bash
   cp ~/sommelier-traces/<time>-<pid>-<n>.trace traces/held-space-after-ga.trace
   sed -i '1i # Typed 가, then held Space for a second; Korean input method.\n# ChromeOS <version>' \
     traces/held-space-after-ga.trace
   cargo test --locked -p sommelier --bins proxy::replay
   ```

   Then read it decoded (see [What a trace holds](#what-a-trace-holds))
   before you commit it. A trace that does not replay is a finding: the proxy or the mirror
   met something it does not expect from Exo. Keep it out of the corpus
   until a fix makes it replay, and report it with the failure.

7. Delete `~/sommelier-traces` once you have what you need.

## Citing a trace

A fact of [docs/IME.md](../docs/IME.md) that a trace shows cites it by
file name in a "Trace:" or "Traces:" clause, as a test is cited in a
"Test:" one. `scripts/check-cited-tests.py` checks that each cited trace
is a file here, which `every_recorded_trace_replays` replays.
[docs/IME.md 5.2](../docs/IME.md#52-unknowns-and-manual-host-checks)
lists the scenarios that would settle an unknown.
