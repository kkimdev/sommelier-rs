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

use super::exo_view::{Frame, Observation, Unmeasured};
use super::guest::{GuestTxn, TooLong};
use super::host::{self, ActivationStep};
use super::state::{Confirm, Deferral, DeleteDrop, LinkState, RegionDrop, TextInput, V3Pending};
use super::{
    BarrierToken, Edit, Effects, Env, HostIme, HostOut, HostTi, ImeCore, Input, KeyEffect,
    LeaseOutcome, SeatId, SurfaceId, TiId, V1Pieces,
};
use crate::connection::MAX_BYTES_PER_SEND;
use crate::protocols::text_input_extension_unstable_v1::{
    zcr_extended_text_input_v1, zcr_text_input_extension_v1,
};
use crate::protocols::text_input_unstable_v1::{zwp_text_input_manager_v1, zwp_text_input_v1};
use crate::protocols::text_input_unstable_v3::zwp_text_input_v3;
use crate::wire::{IndexedString, Outgoing};

/// The log target of the replayable trace (design 3.3): every input, every
/// answer of [`Env`] the step reads, and every effect. It carries the
/// user's text, so it is only ever at `trace`.
const TRACE: &str = "sommelier::ime";

/// The length of a `wl_display.sync`: a header and its `new_id`.
const SYNC_LEN: usize = crate::wire::MessageHeader::LEN + 4;

/// A report the core forwards: the text, cursor and anchor, and whether
/// the guest stated them, or the empty stand-in for a field that states
/// none.
type Report = ((String, u32, u32), bool);

/// What a guest commit sends the host besides its reconciled state: the
/// surrounding text, already encoded, with what it reports, and the cursor
/// rectangle it states. Exo keeps both with or without an input method,
/// so each statement is forwarded rather than reconciled.
struct GuestCommit {
    ti: TiId,
    surrounding_text: Option<(Outgoing, Report)>,
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
        let mut handled = None;
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
            Input::Commit(ti) => {
                commit = self.commit(ti, &mut fx);
                handled = Some(ti);
            }
            Input::Destroy { ti, host_deleted } => ending = self.destroy(ti, host_deleted, &mut fx),
            Input::HostLeave(ti) => self.host_leave(ti, &mut fx),
            Input::V1(ti, event) => self.host_event(ti, event, &mut fx),
            Input::Ext { ext, event } => match self.by_ext(ext) {
                Some(ti) => self.host_event(ti, HostIme::Edit(event), &mut fx),
                None => log::debug!("Ignoring an event of retired extension {ext}"),
            },
            // W1 and W2: a token is one text input's drain or fence, or a
            // removed one's, which is ignored.
            Input::BarrierDone(token) => self
                .inputs
                .iter_mut()
                .for_each(|(&ti, t)| t.barrier_done(ti, token)),
            Input::FocusChanged {
                seat,
                surface,
                keyboard,
            } => self.focus_changed(seat, surface, keyboard, &mut fx),
            Input::SurfaceDestroyed(surface) => self.surface_destroyed(surface, env, &mut fx),
            // F3: the reconcile below activates what the seat can now
            // carry.
            Input::SeatBound(seat) => log::debug!("Guest seat {} bound", seat.0),
            Input::Press { surface, resend } => self.press(surface, resend, &mut fx),
            Input::GuestKey(seat) => self
                .inputs
                .values_mut()
                .filter(|t| t.seat() == seat)
                .for_each(|t| t.exo_mut().guest_edited()),
            Input::HeldKeyRecovered { ti, pair } => self.held_key_recovered(ti, pair, &mut fx),
        }
        self.reconcile_all(commit, env, &mut fx);
        if let Some(t) = handled.and_then(|ti| self.inputs.get_mut(&ti)) {
            t.exo_mut().commit_handled();
        }
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
        fx.to_host.push(HostOut::Request(
            zwp_text_input_manager_v1::req::create_text_input(manager, host.v1),
        ));
        fx.to_host.push(HostOut::Request(
            zcr_text_input_extension_v1::req::get_extended_text_input(
                host.ext.manager,
                host.ext.id,
                host.v1,
            ),
        ));
        // A text input created after the seat's keyboard took its focus
        // still needs that `enter`, or the client would wait for one.
        let surface = env.keyboard_focus(seat);
        if let Some(surface) = surface {
            fx.to_guest
                .push(zwp_text_input_v3::evt::enter(ti.0, surface.0));
        }
        self.insert(ti, seat, host, surface);
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
        if pending.surrounding_text.is_none() {
            t.exo_mut().unchanged();
        }
        // Encode the one request that can fail before changing anything.
        let request = match &pending.surrounding_text {
            None => None,
            Some(text) => {
                let (text, cursor, anchor) =
                    text.as_ref().map_or(("", 0, 0), |(text, cursor, anchor)| {
                        (text.as_str(), *cursor, *anchor)
                    });
                let (cursor, anchor) = (cursor as u32, anchor as u32);
                let request =
                    zwp_text_input_v1::req::set_surrounding_text(t.host().v1, text, cursor, anchor);
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
        if t.apply_commit(&pending, enable_conflict) {
            fx.keys.push(lease(ti, t, LeaseOutcome::End));
        }
        fx.to_guest.extend(t.answer(ti));
        // The report takes the text the guest stated, or the empty
        // stand-in.
        let surrounding_text = request.map(|request| {
            let stated = text_stated(&pending.surrounding_text);
            let (text, cursor, anchor) = pending.surrounding_text.flatten().unwrap_or_default();
            (request, ((text, cursor as u32, anchor as u32), stated))
        });
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
        let Some(ext) = self.inputs.remove(&ti).map(|t| t.host().ext) else {
            return;
        };
        fx.to_host.push(HostOut::Destroy {
            request: zcr_extended_text_input_v1::req::destroy(ext.id),
            id: ext.id,
        });
    }

    /// H2 and H2'.
    fn host_leave(&mut self, ti: TiId, fx: &mut Effects) {
        let Some(t) = self.inputs.get_mut(&ti) else {
            return;
        };
        // Exo's detach confirmed before its `leave` (E6), which committed
        // the preedit the guest shows. One it still shows that
        // confirmation did not commit, so the guest commits it here.
        commit_shown(ti, t, fx);
        let Some(shown) = t.exo_detached() else {
            log::debug!("Text input {}: a leave answers our deactivate", ti.0);
            return;
        };
        log::info!("Exo detached the input method of text input {}", ti.0);
        fx.keys
            .push(KeyEffect::InputMethodDetached { seat: t.seat() });
        // Only a preedit whose commit does not fit a message is left.
        if shown {
            let clear = GuestTxn::default().encode(ti, true, t.serial());
            fx.to_guest
                .extend(clear.expect("an empty preedit fits a message"));
        }
    }

    /// H4 to H9, X1 and X2. A piece waits for its edit, which takes it. The
    /// Exo mirror takes every edit first, before the gate, as each changed
    /// Exo's text; then the guest is in step with Exo after it only if its
    /// transaction reached the guest and left the two alike (3.2).
    fn host_event(&mut self, ti: TiId, event: HostIme, fx: &mut Effects) {
        let Some(t) = self.inputs.get_mut(&ti) else {
            return;
        };
        let edit = match event {
            HostIme::Piece(piece) => return t.record_piece(piece),
            HostIme::Edit(edit) => edit,
        };
        let pieces = t.take_pieces();
        let observation = t.observe(ti, &edit, &pieces);
        composition_event(ti, t, edit, &pieces, observation, fx);
    }

    /// X2': the guest gets the recovered key's `pair`, then G9c's answer,
    /// if the text input still passes the gate, which the keyboard concern
    /// checked before it recovered the key.
    fn held_key_recovered(&mut self, ti: TiId, pair: Vec<Outgoing>, fx: &mut Effects) {
        let Some(t) = self.inputs.get_mut(&ti).filter(|t| t.is_current()) else {
            log::debug!("Text input {} no longer admits its recovered key", ti.0);
            return;
        };
        // The key may edit the guest's text.
        t.exo_mut().guest_edited();
        fx.to_guest.extend(pair);
        fx.to_guest.extend(t.answer(ti));
    }

    /// F1, per text input of the seat, in `TiId` order. The field the
    /// focus leaves commits the preedit the guest shows, before the
    /// keyboard events that moved the focus and its own `leave`.
    fn focus_changed(
        &mut self,
        seat: SeatId,
        surface: Option<SurfaceId>,
        keyboard: Vec<Outgoing>,
        fx: &mut Effects,
    ) {
        for (&ti, t) in self.inputs.iter_mut().filter(|(_, t)| t.seat() == seat) {
            commit_shown(ti, t, fx);
        }
        fx.to_guest.extend(keyboard);
        for (&ti, t) in self.inputs.iter_mut().filter(|(_, t)| t.seat() == seat) {
            let old = t.refocus(surface);
            push_focus_events(ti, old, surface, fx);
        }
    }

    /// F2: a text input focused on the destroyed surface takes the seat's
    /// focus again, and tells the guest while the surface is still valid.
    /// It commits nothing, unlike F1: the guest destroyed the window the
    /// text would go into.
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
            // The press may move the guest's cursor.
            t.exo_mut().guest_edited();
            let Some(cur) = t.current(ti).filter(|cur| cur.shown().is_some()) else {
                continue;
            };
            let fence = BarrierToken(self.next_barrier);
            match cur.press_commit(fence).apply() {
                Ok((events, lease)) => {
                    self.next_barrier += 1;
                    log::debug!("A press ended the composition of text input {}", ti.0);
                    fx.to_guest.extend(events);
                    fx.to_guest.push(resend);
                    // Glued, as the mirror applies the reset at its fence.
                    fx.to_host.push(HostOut::Glued {
                        request: zwp_text_input_v1::req::reset(t.host().v1),
                        barrier: fence,
                    });
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
            // A report applies at the barrier glued to it, which places it
            // among Exo's events for the mirror. One the guest stated that
            // Exo could only drop its composition for is withheld (5); the
            // rest of the commit still goes out.
            if let Some((request, (report, stated))) = surrounding_text {
                match stated.then(|| t.withholding(&report)).flatten() {
                    Some(withheld) => {
                        log::debug!(
                            "Withholding the surrounding text of text input {}: Exo holds it \
                             with the composition the guest shows",
                            ti.0
                        );
                        t.exo_mut().withhold(withheld);
                    }
                    None => {
                        let barrier = BarrierToken(self.next_barrier);
                        self.next_barrier += 1;
                        let one_read = request.0.len() + SYNC_LEN <= MAX_BYTES_PER_SEND;
                        fx.to_host.push(HostOut::Glued { request, barrier });
                        t.exo_mut().report_sent(barrier, report, one_read, stated);
                    }
                }
            }
            if let Some(content_type) = steps.content_type {
                let requests = host::content_type_requests(host.ext.id, content_type);
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

/// H7 to H9, X1 and X2 through the gate: plan the edit's transaction, with
/// the pieces Exo sent before it, and send it. A delete and a region are
/// measured in the frame of `observation`, Exo's text before the edit, or
/// dropped. Every path finishes the observation with what reached the
/// guest.
fn composition_event(
    ti: TiId,
    t: &mut TextInput,
    edit: Edit,
    pieces: &V1Pieces,
    observation: Observation,
    fx: &mut Effects,
) {
    let count = t.serial().get();
    let Some(cur) = t.current(ti) else {
        log::debug!("Ignoring a stale composition event of text input {}", ti.0);
        return t.stale(observation);
    };
    // A region dropped, whose empty range commits the shown preedit.
    let mut dropped = None;
    let planned = match edit {
        Edit::Preedit { serial, text, .. } => {
            log_host_serial(serial, count);
            // The pieces' offsets index the text, which replacing an
            // invalid sequence would move.
            let IndexedString::Utf8(text) = text else {
                log::debug!("Ignoring a host preedit that is not UTF-8");
                return cur.drop_edit(observation);
            };
            cur.preedit(text, pieces, observation)
        }
        Edit::Commit { serial, text } => {
            log_host_serial(serial, count);
            cur.host_commit(text, observation)
        }
        Edit::Delete { index, length } => {
            let lengths = observation
                .measuring()
                .map_err(DeleteDrop::Unmeasured)
                .and_then(Frame::delete);
            match lengths {
                Ok(lengths) => {
                    let txn = GuestTxn {
                        delete: Some(lengths),
                        ..GuestTxn::default()
                    };
                    cur.plan(txn, LeaseOutcome::End, observation)
                }
                Err(DeleteDrop::Empty) => return cur.edits_nothing(observation),
                Err(drop) => {
                    log_delete_drop(ti, index, length, drop);
                    // Exo edited its text, unless the range was empty.
                    if length > 0 {
                        fx.keys.push(cur.lease(LeaseOutcome::End));
                    }
                    return cur.drop_edit(observation);
                }
            }
        }
        Edit::PreeditRegion { index, length } => {
            let txn = observation
                .measuring()
                .map_err(RegionDrop::Unmeasured)
                .and_then(|frame| frame.region(pieces));
            match txn {
                Ok(txn) => cur.plan(txn, LeaseOutcome::End, observation),
                // Exo ends its composition at an empty region, measured or
                // not, and leaves it as text: the guest commits what it
                // shows, which at worst doubles text the next preedit
                // would otherwise replace.
                Err(drop) if length == 0 && cur.shown().is_some() => {
                    dropped = Some((index, length, drop));
                    cur.commit_composition(observation)
                }
                Err(drop) => {
                    log_region_drop(ti, index, length, drop);
                    // Exo edited its composition all the same.
                    fx.keys.push(cur.lease(LeaseOutcome::End));
                    return cur.drop_edit(observation);
                }
            }
        }
        Edit::Confirm { .. } => match cur.confirm(observation) {
            Confirm::Commit(planned) => planned,
            Confirm::RecoverHeldKey(recover) => return fx.keys.push(recover),
        },
    };
    let applied = planned.apply();
    if let Some((index, length, drop)) = dropped {
        log_region_drop(ti, index, length, drop);
    }
    match applied {
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

/// The core drops a host `what`, a delete or a region, as the Exo mirror
/// cannot measure it, for `why`. The event did not reach the guest, which
/// is then out of step with Exo until its next report.
fn log_unmeasured(ti: TiId, what: &str, why: Unmeasured) {
    #[cfg(test)]
    DROPS.with(|drops| {
        let mut counted = drops.get();
        counted.count(why);
        drops.set(counted);
    });
    log::debug!("Dropping Exo's {what} of text input {}: {why:?}", ti.0);
}

/// The deletes and regions the core dropped as the Exo mirror could not
/// measure them, by cause, which the DST counts.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Drops {
    pub(crate) unsure: u64,
    pub(crate) unseen: u64,
    pub(crate) crossing: u64,
    pub(crate) out_of_step: u64,
}

#[cfg(test)]
impl Drops {
    fn count(&mut self, why: Unmeasured) {
        *match why {
            Unmeasured::Unsure => &mut self.unsure,
            Unmeasured::Unseen => &mut self.unseen,
            Unmeasured::Crossing => &mut self.crossing,
            Unmeasured::OutOfStep => &mut self.out_of_step,
        } += 1;
    }

    /// Every drop.
    pub(crate) fn total(&self) -> u64 {
        self.unsure + self.unseen + self.crossing + self.out_of_step
    }
}

#[cfg(test)]
impl std::ops::AddAssign for Drops {
    fn add_assign(&mut self, other: Self) {
        self.unsure += other.unsure;
        self.unseen += other.unseen;
        self.crossing += other.crossing;
        self.out_of_step += other.out_of_step;
    }
}

#[cfg(test)]
thread_local! {
    /// This thread's drops: a DST case runs its proxy on its own thread,
    /// one case at a time.
    static DROPS: std::cell::Cell<Drops> = std::cell::Cell::default();
}

/// This thread's drops since the last call.
#[cfg(test)]
pub(crate) fn take_drops_for_test() -> Drops {
    DROPS.take()
}

/// Whether a pending surrounding text is one the guest stated, not the
/// stand-in of an enable or a disable.
fn text_stated(text: &Option<Option<(String, i32, i32)>>) -> bool {
    matches!(text, Some(Some(_)))
}

/// F1 and H2: a field that loses the focus or its input method commits
/// the preedit the guest shows, if its composition is current, as mutter
/// and ibus do at a focus-out and as Exo's detach confirms it (E6).
fn commit_shown(ti: TiId, t: &mut TextInput, fx: &mut Effects) {
    let Some(cur) = t.current(ti).filter(|cur| cur.shown().is_some()) else {
        return;
    };
    match cur.commit_shown().apply() {
        Ok((events, lease)) => {
            fx.to_guest.extend(events);
            fx.keys.push(lease);
        }
        Err(TooLong) => log::warn!(
            "Dropping the commit of text input {}'s preedit: too long",
            ti.0
        ),
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

/// X1's drops. One around no cursor is one Exo could not have sent, which
/// warns.
fn log_region_drop(ti: TiId, index: i32, length: u32, drop: RegionDrop) {
    match drop {
        RegionDrop::Unmeasured(why) => log_unmeasured(ti, "region", why),
        RegionDrop::Uncovered => log::debug!(
            "Ignoring a preedit region {index}+{length} of text input {}: {drop:?}",
            ti.0
        ),
        RegionDrop::NotAroundCursor => log::warn!(
            "Ignoring a preedit region {index}+{length} of text input {}: {drop:?}",
            ti.0
        ),
    }
}

/// H9's drops. A range around no cursor is one Exo could not have sent,
/// which the mirror's divergence already warned of.
fn log_delete_drop(ti: TiId, index: i32, length: u32, drop: DeleteDrop) {
    if let DeleteDrop::Unmeasured(why) = drop {
        return log_unmeasured(ti, "delete", why);
    }
    log::debug!(
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
