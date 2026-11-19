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

//! [`ImeCore::step`] (design 3.4): one arm per [`Input`], then a
//! reconcile of every text input, then removals. Each arm decides, which
//! includes encoding, before it changes anything (design 3.5).

use super::guest::{GuestTxn, TooLong};
use super::host::{self, ActivationStep};
use super::state::{Confirm, Deferral, DeleteDrop, LinkState, RegionDrop, TextInput};
use super::{
    BarrierToken, Effects, Env, HostIme, HostOut, HostTi, ImeCore, Input, KeyEffect, LeaseOutcome,
    SeatId, SurfaceId, TiId,
};
use crate::protocols::text_input_extension_unstable_v1::{
    zcr_extended_text_input_v1, zcr_text_input_extension_v1,
};
use crate::protocols::text_input_unstable_v1::{zwp_text_input_manager_v1, zwp_text_input_v1};
use crate::protocols::text_input_unstable_v3::zwp_text_input_v3;
use crate::state::V3Pending;
use crate::wire::{IndexedString, Outgoing};

/// The log target of the replayable trace (design 3.3): every input, every
/// answer of [`Env`] the step reads, and every effect. It carries the
/// user's text, so it is only ever at `trace`.
const TRACE: &str = "sommelier::ime";

/// What a guest commit sends the host besides its reconciled state: the
/// surrounding text, already encoded, and the cursor rectangle it states.
/// Exo keeps both with or without an input method, so each statement is
/// forwarded rather than reconciled.
struct GuestCommit {
    ti: TiId,
    surrounding_text: Option<Outgoing>,
    cursor_rect: Option<(i32, i32, i32, i32)>,
}

impl ImeCore {
    /// Handle one input. The returned effects are the whole of its
    /// output; the adapter applies them in order.
    pub fn step(&mut self, input: Input, env: &impl Env) -> Effects {
        log::trace!(target: TRACE, "in {input:?}");
        let env = &Traced(env);
        let mut fx = Effects::default();
        let mut commit = None;
        let mut ending = None;
        match input {
            Input::Create {
                ti,
                seat,
                host,
                manager,
            } => self.create(ti, seat, host, manager, env, &mut fx),
            Input::Enable(ti) => self.update_pending(ti, |p| *p = V3Pending::reset(true)),
            Input::Disable(ti) => self.update_pending(ti, |p| *p = V3Pending::reset(false)),
            Input::SurroundingText(ti, text, cursor, anchor) => {
                self.surrounding_text(ti, text, cursor, anchor)
            }
            Input::ContentType(ti, content_type) => {
                self.update_pending(ti, |p| p.content_type = Some(content_type))
            }
            Input::CursorRect(ti, rect) => self.update_pending(ti, |p| p.cursor_rect = Some(rect)),
            Input::Commit(ti) => commit = self.commit(ti, &mut fx),
            Input::Destroy { ti, host_deleted } => ending = self.destroy(ti, host_deleted, &mut fx),
            Input::HostLeave(ti) => self.host_leave(ti, &mut fx),
            Input::V1(ti, event) => self.host_event(ti, event, &mut fx),
            Input::Ext { ext, event } => match self.by_ext.get(&ext) {
                Some(&ti) => self.host_event(ti, event, &mut fx),
                None => log::debug!("Ignoring an event of retired extension {ext}"),
            },
            // W1 and W2: a token is one text input's drain or fence, or a
            // removed one's, which is ignored.
            Input::BarrierDone(token) => {
                self.inputs.values_mut().for_each(|t| t.barrier_done(token))
            }
            Input::FocusChanged { seat, surface } => self.focus_changed(seat, surface, &mut fx),
            Input::SurfaceDestroyed(surface) => self.surface_destroyed(surface, env, &mut fx),
            // F3: the reconcile below activates what the seat can now
            // carry.
            Input::SeatBound(seat) => log::debug!("Guest seat {} bound", seat.0),
            Input::Press { surface, resend } => self.press(surface, resend, &mut fx),
        }
        self.reconcile_all(commit, env, &mut fx);
        if let Some(ti) = ending {
            self.remove(ti, &mut fx);
        }
        log::trace!(target: TRACE, "out {fx:?}");
        fx
    }

    /// G1.
    fn create(
        &mut self,
        ti: TiId,
        seat: SeatId,
        host: HostTi,
        manager: u32,
        env: &impl Env,
        fx: &mut Effects,
    ) {
        debug_assert!(!self.inputs.contains_key(&ti), "{ti:?} exists already");
        fx.to_host.push(HostOut::Request(
            zwp_text_input_manager_v1::req::create_text_input(manager, host.v1),
        ));
        if let Some(ext) = host.ext {
            fx.to_host.push(HostOut::Request(
                zcr_text_input_extension_v1::req::get_extended_text_input(
                    ext.manager,
                    ext.id,
                    host.v1,
                ),
            ));
            self.by_ext.insert(ext.id, ti);
        }
        // A text input created after the seat's keyboard took its focus
        // still needs that `enter`, or the client would wait for one.
        let surface = env.keyboard_focus(seat);
        if let Some(surface) = surface {
            fx.to_guest
                .push(zwp_text_input_v3::evt::enter(ti.0, surface.0));
        }
        self.inputs.insert(ti, TextInput::new(seat, host, surface));
    }

    /// G3, G4, G7 and G8, which are ignored while unfocused.
    fn update_pending(&mut self, ti: TiId, update: impl FnOnce(&mut V3Pending)) {
        if let Some(pending) = self.inputs.get_mut(&ti).and_then(TextInput::pending_mut) {
            update(pending);
        }
    }

    /// G5.
    fn surrounding_text(&mut self, ti: TiId, text: IndexedString, cursor: i32, anchor: i32) {
        let Some(pending) = self.inputs.get_mut(&ti).and_then(TextInput::pending_mut) else {
            return;
        };
        // The cursor and anchor are byte offsets into the text, which
        // replacing an invalid sequence would move. Empty the field's text
        // instead, so that the input method does not go on editing text
        // the guest no longer has.
        let text = match text {
            IndexedString::Utf8(text) => text,
            IndexedString::NotUtf8(_) => {
                log::debug!("Emptying surrounding text that is not UTF-8");
                pending.surrounding_text = Some(Some((String::new(), 0, 0)));
                return;
            }
        };
        let boundary =
            |offset: i32| usize::try_from(offset).is_ok_and(|o| text.is_char_boundary(o));
        if !(boundary(cursor) && boundary(anchor)) {
            log::warn!(
                "Ignoring invalid surrounding text offsets: len={}, cursor={cursor}, anchor={anchor}",
                text.len()
            );
            return;
        }
        pending.surrounding_text = Some(Some((text, cursor, anchor)));
    }

    /// G9: count the commit, then apply it if the field is focused.
    /// Returns what it sends the host besides the reconcile.
    fn commit(&mut self, ti: TiId, fx: &mut Effects) -> Option<GuestCommit> {
        // One text input on a connection is enabled at a time (B7).
        let other_enabled = self
            .inputs
            .iter()
            .any(|(&other, t)| other != ti && t.is_enabled());
        let t = self.inputs.get_mut(&ti)?;
        t.count_commit();
        let pending = t.take_pending()?;
        // Encode the one request that can fail before changing anything.
        let surrounding_text = match &pending.surrounding_text {
            None => None,
            Some(text) => {
                let (text, cursor, anchor) =
                    text.as_ref().map_or(("", 0, 0), |(text, cursor, anchor)| {
                        (text, *cursor, *anchor)
                    });
                let request = zwp_text_input_v1::req::set_surrounding_text(
                    t.host().v1,
                    text,
                    cursor as u32,
                    anchor as u32,
                );
                match request {
                    Ok(request) => Some(request),
                    // G9b: the commit counted and its pending state is
                    // spent, but nothing else changes.
                    Err(error) => {
                        log::warn!("Dropping the commit of text input {}: {error}", ti.0);
                        fx.to_guest.extend(t.answer(ti));
                        return None;
                    }
                }
            }
        };
        let enable_conflict = other_enabled && pending.enabled == Some(true);
        if enable_conflict {
            log::warn!(
                "Ignoring text-input enable for guest {}: another text input is enabled",
                ti.0
            );
        }
        let cursor_rect = pending.cursor_rect;
        if t.apply_commit(pending, enable_conflict) {
            fx.keys.push(lease(ti, t, LeaseOutcome::End));
        }
        fx.to_guest.extend(t.answer(ti));
        Some(GuestCommit {
            ti,
            surrounding_text,
            cursor_rect,
        })
    }

    /// G10 and F6: the field ends with no `leave`, as the guest destroyed
    /// the text input. Returns it, to be removed after the reconcile has
    /// brought its link to inactive.
    fn destroy(&mut self, ti: TiId, host_deleted: bool, fx: &mut Effects) -> Option<TiId> {
        let t = self.inputs.get_mut(&ti)?;
        if host_deleted {
            t.host_deleted();
        }
        t.refocus(None);
        fx.keys.push(lease(ti, t, LeaseOutcome::End));
        Some(ti)
    }

    /// The end of G10: forget the text input and destroy its extension.
    fn remove(&mut self, ti: TiId, fx: &mut Effects) {
        let Some(ext) = self.inputs.remove(&ti).and_then(|t| t.host().ext) else {
            return;
        };
        self.by_ext.remove(&ext.id);
        fx.to_host.extend([
            HostOut::Request(zcr_extended_text_input_v1::req::destroy(ext.id)),
            HostOut::Retire(ext.id),
        ]);
    }

    /// H2 and H2'.
    fn host_leave(&mut self, ti: TiId, fx: &mut Effects) {
        let Some(t) = self.inputs.get_mut(&ti) else {
            return;
        };
        let Some(shown) = t.exo_detached() else {
            log::debug!("Text input {}: a leave answers our deactivate", ti.0);
            return;
        };
        log::info!("Exo detached the input method of text input {}", ti.0);
        if shown {
            let clear = GuestTxn::default().encode(ti, true, t.serial());
            fx.to_guest
                .extend(clear.expect("an empty preedit fits a message"));
        }
    }

    /// H4 to H9, X1 and X2.
    fn host_event(&mut self, ti: TiId, event: HostIme, fx: &mut Effects) {
        let Some(t) = self.inputs.get_mut(&ti) else {
            return;
        };
        let assembly = match event {
            HostIme::PreeditStyling {
                index,
                length,
                style,
            } => {
                // v3 can show only the selection style, as the cursor.
                if let Some(assembly) = t
                    .assembly_mut()
                    .filter(|_| style == zwp_text_input_v1::PREEDIT_STYLE_SELECTION)
                {
                    assembly.selection = Some((index, length));
                }
                return;
            }
            HostIme::PreeditCursor(index) => {
                if let Some(assembly) = t.assembly_mut() {
                    assembly.cursor = Some(index);
                }
                return;
            }
            HostIme::CursorPosition { index, anchor } => {
                // Exo sends it only before a commit_string.
                // H6: the old handlers recorded it for the next commit and
                // only logged it there.
                log::debug!("Cannot represent v1 cursor_position({index}, {anchor}) in v3");
                return;
            }
            // B3: a composition event takes the assembly first, whatever
            // becomes of it. The old handlers differ only where no client
            // can tell, since Exo sends styling and a cursor only right
            // before their preedit: a dropped commit took only its
            // cursor_position, and a dropped delete or confirmation took
            // nothing.
            HostIme::Preedit { .. }
            | HostIme::Commit { .. }
            | HostIme::Delete { .. }
            | HostIme::PreeditRegion { .. }
            | HostIme::Confirm => t.take_assembly(),
        };
        let count = t.serial().get();
        let Some(cur) = t.current(ti) else {
            log::debug!("Ignoring a stale composition event of text input {}", ti.0);
            return;
        };
        let planned = match event {
            HostIme::Preedit {
                serial,
                text,
                commit,
            } => {
                log_host_serial(serial, count);
                // The assembly's offsets index the text, which replacing
                // an invalid sequence would move.
                let IndexedString::Utf8(text) = text else {
                    log::debug!("Ignoring a host preedit that is not UTF-8");
                    return;
                };
                cur.preedit(text, commit, assembly)
            }
            HostIme::Commit { serial, text } => {
                log_host_serial(serial, count);
                cur.host_commit(text)
            }
            HostIme::Delete { index, length } => match cur.delete(index, length) {
                Ok(planned) => planned,
                Err(drop) => return log_delete_drop(ti, t, index, length, drop),
            },
            HostIme::PreeditRegion { index, length } => match cur.region(index, length, assembly) {
                Ok(planned) => planned,
                Err(RegionDrop::Uncovered) => {
                    log::debug!(
                        "Ignoring a preedit region {index}+{length} of text input {} that does \
                         not cover the region it shows",
                        ti.0
                    );
                    return;
                }
                Err(drop) => {
                    log::warn!(
                        "Ignoring a preedit region {index}+{length} of text input {}: {drop:?}",
                        ti.0
                    );
                    return;
                }
            },
            HostIme::Confirm => match cur.confirm() {
                Confirm::Commit(planned) => planned,
                Confirm::RecoverHeldKey(recover) => return fx.keys.push(recover),
            },
            HostIme::PreeditStyling { .. }
            | HostIme::PreeditCursor(_)
            | HostIme::CursorPosition { .. } => unreachable!("handled above"),
        };
        match planned.apply() {
            Ok((events, lease)) => {
                fx.to_guest.extend(events);
                fx.keys.push(lease);
            }
            Err(TooLong) => log::warn!(
                "Dropping a host event of text input {}: its text does not fit a message",
                ti.0
            ),
        }
    }

    /// F1, per text input of the seat, in `TiId` order.
    fn focus_changed(&mut self, seat: SeatId, surface: Option<SurfaceId>, fx: &mut Effects) {
        for (&ti, t) in &mut self.inputs {
            if t.seat() == seat {
                let old = t.refocus(surface);
                push_focus_events(ti, old, surface, fx);
            }
        }
    }

    /// F2: a text input focused on the destroyed surface takes the seat's
    /// focus again, and tells the guest while the surface is still valid.
    fn surface_destroyed(&mut self, surface: SurfaceId, env: &impl Env, fx: &mut Effects) {
        for (&ti, t) in &mut self.inputs {
            if t.focused_surface() == Some(surface) {
                let focus = env.keyboard_focus(t.seat());
                let old = t.refocus(focus);
                push_focus_events(ti, old, focus, fx);
            }
        }
    }

    /// F4: a press on the surface of a current composition commits it,
    /// resets the host behind a fence, and then re-sends the press.
    fn press(&mut self, surface: SurfaceId, resend: Outgoing, fx: &mut Effects) {
        for (&ti, t) in &mut self.inputs {
            if t.focused_surface() != Some(surface) {
                continue;
            }
            let Some(mut cur) = t.current(ti).filter(|cur| cur.shown().is_some()) else {
                continue;
            };
            // F4 takes the assembly, whatever becomes of the commit.
            cur.take_assembly();
            let fence = BarrierToken(self.next_barrier);
            let planned = cur.press_commit(fence);
            match planned.apply() {
                Ok((events, lease)) => {
                    self.next_barrier += 1;
                    log::debug!("A press ended the composition of text input {}", ti.0);
                    fx.to_guest.extend(events);
                    fx.to_guest.push(resend);
                    fx.to_host.extend([
                        HostOut::Request(zwp_text_input_v1::req::reset(t.host().v1)),
                        HostOut::Barrier(fence),
                    ]);
                    fx.keys.push(lease);
                }
                Err(TooLong) => {
                    log::warn!("Dropping a press's commit of text input {}: too long", ti.0);
                    fx.forward = true;
                }
            }
            // One text input on a connection is enabled at a time.
            return;
        }
        fx.forward = true;
    }

    /// Phase 2: bring every text input's link to its target, with the
    /// requests of a guest commit in their place.
    fn reconcile_all(&mut self, mut commit: Option<GuestCommit>, env: &impl Env, fx: &mut Effects) {
        for (&ti, t) in &mut self.inputs {
            let target = host::target(t, env);
            let steps = host::reconcile(&target, t.link());
            // R2: an activation the target needs and the link cannot take
            // yet.
            let deferral = match (steps.activation, target.surface, t.link()) {
                (None, Some(_), LinkState::Draining { .. }) => Some(Deferral::Draining),
                (None, Some(_), LinkState::Detached) => Some(Deferral::NoSeat),
                _ => None,
            };
            let host = t.host();
            match steps.activation {
                Some(ActivationStep::Activate { seat, surface }) => {
                    log::info!("Activating text input {} on host surface {surface}", ti.0);
                    fx.to_host
                        .push(HostOut::Request(zwp_text_input_v1::req::activate(
                            host.v1, seat, surface,
                        )));
                    t.activate_sent();
                }
                Some(ActivationStep::Deactivate { seat }) => {
                    log::info!("Deactivating text input {}", ti.0);
                    let barrier = BarrierToken(self.next_barrier);
                    self.next_barrier += 1;
                    fx.to_host.extend([
                        HostOut::Request(zwp_text_input_v1::req::deactivate(host.v1, seat)),
                        HostOut::Barrier(barrier),
                    ]);
                    t.deactivate_sent(barrier);
                }
                Some(ActivationStep::Abandon) => {
                    log::debug!("Text input {} lost its host seat while active", ti.0);
                    t.abandoned();
                }
                None => {}
            }
            // R2, logged when it starts.
            if t.deferral_changed(deferral) {
                match deferral {
                    Some(Deferral::Draining) => log::debug!(
                        "Deferring text input {} activation until the previous one drains",
                        ti.0
                    ),
                    Some(Deferral::NoSeat) => {
                        let level = match t.first_seat_warning() {
                            true => log::Level::Warn,
                            false => log::Level::Debug,
                        };
                        log::log!(
                            level,
                            "Cannot activate text input {}: guest seat {} has no live host seat",
                            ti.0,
                            t.seat().0
                        );
                    }
                    None => {}
                }
            }
            let (guest_commit, surrounding_text, cursor_rect) =
                match commit.take_if(|commit| commit.ti == ti) {
                    Some(c) => (true, c.surrounding_text, c.cursor_rect),
                    None => (false, None, None),
                };
            fx.to_host.extend(surrounding_text.map(HostOut::Request));
            if let Some(content_type) = steps.content_type {
                let ext = host.ext.map(|e| (e.id, e.version));
                let requests = host::content_type_requests(host.v1, ext, content_type);
                fx.to_host
                    .extend(requests.into_iter().map(HostOut::Request));
                t.content_type_sent(content_type);
            }
            if let Some((x, y, width, height)) = cursor_rect {
                fx.to_host.push(HostOut::Request(
                    zwp_text_input_v1::req::set_cursor_rectangle(host.v1, x, y, width, height),
                ));
            }
            if guest_commit || steps.content_type.is_some() {
                fx.to_host
                    .push(HostOut::Request(zwp_text_input_v1::req::commit_state(
                        host.v1,
                        t.serial().get(),
                    )));
            }
        }
    }
}

fn lease(ti: TiId, t: &TextInput, outcome: LeaseOutcome) -> KeyEffect {
    KeyEffect::Lease {
        ti,
        seat: t.seat(),
        outcome,
    }
}

/// The guest's `leave` of the old surface and `enter` of the new, each if
/// the focus changed.
fn push_focus_events(ti: TiId, old: Option<SurfaceId>, new: Option<SurfaceId>, fx: &mut Effects) {
    if old == new {
        return;
    }
    fx.to_guest.extend(
        old.map(|surface| zwp_text_input_v3::evt::leave(ti.0, surface.0))
            .into_iter()
            .chain(new.map(|surface| zwp_text_input_v3::evt::enter(ti.0, surface.0))),
    );
}

/// Exo numbers its v1 events itself; v3 needs the guest's commit count.
fn log_host_serial(serial: u32, count: u32) {
    if serial != count {
        log::debug!("Host event references guest serial {serial}, current serial is {count}");
    }
}

/// H9's drops. The first of a range around no cursor warns, since only
/// the log shows the lost delete.
fn log_delete_drop(ti: TiId, t: &mut TextInput, index: i32, length: u32, drop: DeleteDrop) {
    let level = match drop {
        DeleteDrop::NotAroundCursor if t.first_delete_range_warning() => log::Level::Warn,
        _ => log::Level::Debug,
    };
    log::log!(
        level,
        "Ignoring v1 delete range {index}+{length} of text input {}: {drop:?}",
        ti.0
    );
}

/// The [`Env`] of one step, which logs each answer it gives, so that the
/// trace replays the step.
struct Traced<'a, E>(&'a E);

impl<E: Env> Env for Traced<'_, E> {
    fn host_surface(&self, surface: SurfaceId) -> Option<u32> {
        let answer = self.0.host_surface(surface);
        log::trace!(target: TRACE, "env host_surface({}) = {answer:?}", surface.0);
        answer
    }

    fn host_seat(&self, seat: SeatId) -> Option<u32> {
        let answer = self.0.host_seat(seat);
        log::trace!(target: TRACE, "env host_seat({}) = {answer:?}", seat.0);
        answer
    }

    fn keyboard_focus(&self, seat: SeatId) -> Option<SurfaceId> {
        let answer = self.0.keyboard_focus(seat);
        log::trace!(target: TRACE, "env keyboard_focus({}) = {answer:?}", seat.0);
        answer
    }
}
