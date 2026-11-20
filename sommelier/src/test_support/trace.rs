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

//! Reading a wire trace, whose format `crate::trace` owns, printing one,
//! and recording one in memory.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::rc::Rc;

use super::glued_messages;
use super::wire_peer::{Peer, Role};
use crate::protocols::{AnyEvent, AnyRequest};
use crate::trace::{Stream, FACTS, HEADER, KEYMAP};
use crate::wire::Outgoing;

/// A wire trace, read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Trace {
    /// The connection's facts, as `crate::trace::Facts` writes them.
    pub(crate) facts: String,
    /// The records, in order.
    pub(crate) records: Vec<Record>,
}

/// One message of a trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) stream: Stream,
    pub(crate) message: Vec<u8>,
    /// How many descriptors it carried.
    pub(crate) fds: usize,
    /// The text of the keymap a host `wl_keyboard.keymap` carried.
    pub(crate) keymap: Option<Vec<u8>>,
}

impl Trace {
    /// Read `text`, a whole trace.
    ///
    /// # Errors
    ///
    /// Which line is not one of a trace of this version, and why.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        if !text.is_empty() && !text.ends_with('\n') {
            return Err("the trace ends in a torn record".to_string());
        }
        let mut lines = text
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.starts_with('#'));
        match lines.next() {
            Some((_, HEADER)) => {}
            other => {
                return Err(format!(
                    "the trace does not start with {HEADER:?}: {other:?}"
                ))
            }
        }
        let facts = match lines.next() {
            Some((_, line)) if line.starts_with(FACTS) => line[FACTS.len()..].to_string(),
            other => return Err(format!("the trace states no facts: {other:?}")),
        };
        let mut records: Vec<Record> = Vec::new();
        for (number, line) in lines {
            let at = |why: String| format!("line {}: {why}", number + 1);
            let Some(hex) = line
                .strip_prefix(KEYMAP)
                .and_then(|rest| rest.strip_prefix(' '))
            else {
                records.push(Record::parse(line).map_err(at)?);
                continue;
            };
            match records.last_mut() {
                Some(
                    record @ Record {
                        stream: Stream::FromHost,
                        fds: 1,
                        keymap: None,
                        ..
                    },
                ) => record.keymap = Some(bytes(hex).map_err(at)?),
                _ => return Err(at("a keymap follows no host event".to_string())),
            }
        }
        Ok(Self { facts, records })
    }

    /// The value of fact `name`, as the trace states it.
    pub(crate) fn fact(&self, name: &str) -> Option<&str> {
        self.facts
            .split(' ')
            .find_map(|fact| fact.strip_prefix(name)?.strip_prefix('='))
    }

    /// The records of `stream`, in order.
    pub(crate) fn of(&self, stream: Stream) -> Vec<&Record> {
        self.records
            .iter()
            .filter(|record| record.stream == stream)
            .collect()
    }

    /// The trace as text to read: a line for each message, numbered as
    /// its record, decoded by the generated decoders as the peer that sent
    /// or got it follows its objects (`wire_peer`), with every string it
    /// holds; and the size of each keymap. Reading it is how to tell what
    /// a trace holds before it is shared.
    pub(crate) fn decoded(&self) -> Vec<String> {
        let stand_ins = |count| -> VecDeque<OwnedFd> {
            (0..count)
                .map(|_| {
                    std::fs::File::open("/dev/null")
                        .expect("open /dev/null")
                        .into()
                })
                .collect()
        };
        let (mut guest, mut host) = (Peer::new(Role::Client), Peer::new(Role::Server));
        let mut lines = Vec::new();
        for (at, record) in self.records.iter().enumerate() {
            let tag = record.stream.tag();
            let mut fds = stand_ins(record.fds);
            let mut line = |text: String| lines.push(format!("{at} {tag} {text}"));
            for message in glued_messages(&record.message) {
                let decoded = match record.stream {
                    Stream::FromGuest => {
                        let decoded = guest.decode::<AnyRequest>(message, &mut fds);
                        guest.send(&Outgoing::from_parts(
                            message.to_vec(),
                            stand_ins(record.fds).into(),
                        ));
                        decoded
                            .map(|(sender, request)| format!("{sender} {request:?}"))
                            .map_err(|error| format!("does not decode: {error}"))
                    }
                    Stream::FromHost => {
                        let decoded = host.decode::<AnyEvent>(message, &mut fds);
                        host.send(&Outgoing::from_parts(
                            message.to_vec(),
                            stand_ins(record.fds).into(),
                        ));
                        decoded
                            .map(|(sender, event)| format!("{sender} {event:?}"))
                            .map_err(|error| format!("does not decode: {error}"))
                    }
                    Stream::ToGuest => guest
                        .receive::<AnyEvent>(message, &mut fds)
                        .map(|(sender, event)| format!("{sender} {event:?}"))
                        .map_err(|violation| format!("breaks the protocol: {violation}")),
                    Stream::ToHost => host
                        .receive::<AnyRequest>(message, &mut fds)
                        .map(|(sender, request)| format!("{sender} {request:?}"))
                        .map_err(|violation| format!("breaks the protocol: {violation}")),
                };
                line(decoded.unwrap_or_else(|why| format!("{message:02x?}: {why}")));
            }
            if let Some(keymap) = &record.keymap {
                line(format!("{KEYMAP} a keymap of {} bytes", keymap.len()));
            }
        }
        lines
    }
}

impl Record {
    fn parse(line: &str) -> Result<Self, String> {
        let mut fields = line.split(' ');
        let (Some(tag), Some(fds), Some(hex), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(format!("{line:?} is not a tag, a count and a message"));
        };
        let stream = Stream::ALL
            .into_iter()
            .find(|stream| stream.tag() == tag)
            .ok_or_else(|| format!("no stream is tagged {tag:?}"))?;
        let fds = fds
            .parse()
            .map_err(|_| format!("{fds:?} counts no descriptors"))?;
        Ok(Self {
            stream,
            message: bytes(hex)?,
            fds,
            keymap: None,
        })
    }
}

/// The bytes `hex` spells.
fn bytes(hex: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err(format!("{hex:?} has half a byte"));
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| {
            hex.get(at..at + 2)
                .and_then(|byte| u8::from_str_radix(byte, 16).ok())
                .ok_or_else(|| format!("{hex:?} is not hex"))
        })
        .collect()
}

/// A trace written to memory: a `crate::trace::Recorder` writes it, and
/// its clones read what it wrote.
#[derive(Clone, Debug, Default)]
pub(crate) struct Tape(Rc<RefCell<Vec<u8>>>);

impl Tape {
    /// What was written so far.
    pub(crate) fn text(&self) -> String {
        String::from_utf8(self.0.borrow().clone()).expect("a trace is text")
    }
}

impl Write for Tape {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Context;
    use crate::trace::{Facts, Recorder};

    /// What a recorder writes reads back as it was recorded, comments
    /// aside, every byte value, descriptor count and keymap included.
    #[test]
    fn a_trace_reads_back_what_was_recorded() {
        let tape = Tape::default();
        let facts = Facts::of(&Context::new_for_test(false, false, Vec::new()));
        let mut recorder = Recorder::new(Box::new(tape.clone()), &facts).unwrap();
        let records = [
            (Stream::FromGuest, (0..=255).collect::<Vec<u8>>(), 0),
            (Stream::ToHost, vec![1, 0, 0, 0, 0, 0, 12, 0, 2, 0, 0, 0], 2),
            (Stream::FromHost, vec![0xff; 8], 1),
            (Stream::ToGuest, Vec::new(), 0),
        ];
        for (stream, message, fds) in &records {
            recorder.record(*stream, message, *fds);
            if *stream == Stream::FromHost {
                recorder.keymap(b"keymap\0");
            }
        }
        recorder.flush();

        let text = tape.text().replacen('\n', "\n# a scenario\n", 2);
        let trace = Trace::parse(&text).unwrap();
        assert_eq!(trace.facts, facts.to_string());
        assert_eq!(trace.fact("host"), Some("socket"));
        assert_eq!(trace.fact("vm-id"), Some("termina"));
        assert_eq!(trace.fact("vm"), None);
        assert_eq!(trace.of(Stream::ToHost), [&trace.records[1]]);
        let keymaps: Vec<_> = trace
            .records
            .iter()
            .map(|record| record.keymap.as_deref())
            .collect();
        assert_eq!(keymaps, [None, None, Some(&b"keymap\0"[..]), None]);
        let read: Vec<_> = trace
            .records
            .into_iter()
            .map(|record| (record.stream, record.message, record.fds))
            .collect();
        assert_eq!(read, records);
    }

    /// A trace of another format or version, a line no recorder writes,
    /// or a torn last record is refused, with the line.
    #[test]
    fn a_trace_that_is_not_one_is_refused() {
        let trace = |records: &str| format!("{HEADER}\n{FACTS}host=socket\n{records}");
        assert!(Trace::parse(&trace("g> 0 00ff\n")).is_ok());
        for (text, why) in [
            ("sommelier-trace 2\n".to_string(), "does not start"),
            (format!("{HEADER}\ng> 0 00\n"), "no facts"),
            (trace("x> 0 00\n"), "line 3: no stream"),
            (trace("g> one 00\n"), "line 3: \"one\" counts"),
            (trace("g> 0 0\n"), "line 3: \"0\" has half"),
            (trace("g> 0 zz\n"), "line 3: \"zz\" is not hex"),
            (trace("g> 0\n"), "line 3: \"g> 0\" is not"),
            (trace("g> 0 00 00\n"), "line 3: \"g> 0 00 00\" is not"),
            (trace("g> 0 00"), "torn record"),
            (trace("km 00\n"), "line 3: a keymap follows no host event"),
            (
                trace("g> 0 00\nkm 00\n"),
                "line 4: a keymap follows no host event",
            ),
            (
                trace("h> 1 00\nkm 00\nkm 00\n"),
                "line 5: a keymap follows no",
            ),
            (trace("h> 1 00\nkm 0\n"), "line 4: \"0\" has half"),
        ] {
            let error = Trace::parse(&text).unwrap_err();
            assert!(error.contains(why), "{text:?}: {error}");
        }
    }

    /// Prints the trace `SOMMELIER_TEST_TRACE` names, decoded
    /// ([`Trace::decoded`]), so that its owner can read what it holds before
    /// sharing it (`traces/README.md`).
    #[test]
    #[ignore = "prints the trace SOMMELIER_TEST_TRACE names"]
    fn print_a_trace() {
        let path =
            std::env::var("SOMMELIER_TEST_TRACE").expect("SOMMELIER_TEST_TRACE names a trace");
        let text = std::fs::read_to_string(&path).expect("read the trace");
        let trace = Trace::parse(&text).unwrap_or_else(|why| panic!("{path}: {why}"));
        println!("facts {}", trace.facts);
        for line in trace.decoded() {
            println!("{line}");
        }
    }
}
