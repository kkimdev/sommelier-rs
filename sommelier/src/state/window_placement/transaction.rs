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

//! Reducer-owned lifecycle for one self-parent placement transaction.
//!
//! This module intentionally contains no Wayland IDs or wire I/O.  It is the
//! only place that decides whether a placement is waiting for a guest commit,
//! a host resize acknowledgement, a parent barrier, or the final cleanup
//! barrier.  The protocol handlers translate wire events into the methods below
//! and serialize the resulting effect; they do not mutate phase fields.

/// Screen-space rectangle used by the placement reducer.
pub(super) type PlacementRect = (i32, i32, i32, i32);
/// Screen-space point used by the placement reducer.
pub(super) type PlacementPoint = (i32, i32);

/// Token for a proxy-generated XDG configure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ConfigureToken {
    pub(super) xdg_surface_id: u32,
    pub(super) serial: u32,
}

/// Host resize information retained when the host responds before the guest
/// has acknowledged the synthetic configure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HostResizeAck {
    pub(super) size: (i32, i32),
    pub(super) origin: PlacementPoint,
}

/// Explicit lifecycle of one placement transaction.
///
/// Every active variant carries the immutable target and a monotonically
/// increasing generation.  Late callbacks can therefore be rejected by the
/// owner before they mutate a newer operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum SelfParentPhase {
    /// No placement transaction is active.
    #[default]
    Idle,
    /// The guest and host must agree on the requested size before moving.
    ResizePending {
        generation: u64,
        target: PlacementRect,
        surface_id: u32,
        expected_size: Option<(i32, i32)>,
        configure_surface_id: Option<u32>,
        configure_serial: Option<u32>,
        client_ack_seen: bool,
        client_commit_seen: bool,
        host_resize_ack: Option<HostResizeAck>,
    },
    /// The self-parent request and its first barrier are in flight.
    MovePending {
        generation: u64,
        target: PlacementRect,
        surface_id: u32,
        origin_acknowledged: bool,
    },
    /// NULL-parent cleanup and the final origin acknowledgement are pending.
    CleanupPending {
        generation: u64,
        target: PlacementRect,
        surface_id: u32,
        origin_acknowledged: bool,
        cleanup_barrier_pending: bool,
    },
}

/// Result of feeding a host configure into the reducer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostResizeResult {
    /// The configure is unrelated to the active transaction.
    Ignored,
    /// The size is plausible, but the guest gate is not complete yet.
    Buffered,
    /// The host accepted the requested size and the move phase may be queued.
    Accepted { origin: PlacementPoint },
}

/// Result of feeding an origin event into the reducer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OriginResult {
    /// The origin was unrelated or an intermediate animation coordinate.
    Ignored,
    /// The origin is authoritative for the active transaction.
    Accepted { transaction_complete: bool },
}

/// Result of a reducer event that may open the move phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResizeGateResult {
    /// The gate is still waiting for an event.
    Waiting,
    /// A buffered host acknowledgement can now be applied.
    Ready { origin: PlacementPoint },
}

/// State and reducer for one live Aura toplevel.
#[derive(Debug, Default)]
pub(super) struct PlacementTransaction {
    phase: SelfParentPhase,
    deferred_target: Option<PlacementRect>,
    last_completed_target: Option<PlacementRect>,
    completed_origin_guard: Option<PlacementPoint>,
    next_generation: u64,
}

impl PlacementTransaction {
    /// Return the current phase without exposing mutable state.
    pub(super) const fn phase(&self) -> SelfParentPhase {
        self.phase
    }

    /// Return the active transaction target, if any.
    pub(super) const fn active_target(&self) -> Option<PlacementRect> {
        match self.phase {
            SelfParentPhase::Idle => None,
            SelfParentPhase::ResizePending { target, .. }
            | SelfParentPhase::MovePending { target, .. }
            | SelfParentPhase::CleanupPending { target, .. } => Some(target),
        }
    }

    /// Return the Aura surface owned by the active transaction.
    pub(super) const fn active_surface(&self) -> Option<u32> {
        match self.phase {
            SelfParentPhase::Idle => None,
            SelfParentPhase::ResizePending { surface_id, .. }
            | SelfParentPhase::MovePending { surface_id, .. }
            | SelfParentPhase::CleanupPending { surface_id, .. } => Some(surface_id),
        }
    }

    /// Return the target retained for the next transaction.
    pub(super) const fn deferred_target(&self) -> Option<PlacementRect> {
        self.deferred_target
    }

    /// Whether the guest has completed the active synthetic configure
    /// handshake. This is intentionally independent from the host resize
    /// acknowledgement: either side may arrive first.
    pub(super) const fn client_gate_complete(&self) -> bool {
        matches!(
            self.phase,
            SelfParentPhase::ResizePending {
                client_ack_seen: true,
                client_commit_seen: true,
                ..
            }
        )
    }

    /// Return the last target that reached a host origin acknowledgement.
    pub(super) const fn last_completed_target(&self) -> Option<PlacementRect> {
        self.last_completed_target
    }

    /// Start a new resize generation.
    pub(super) fn begin_resize(
        &mut self,
        target: PlacementRect,
        surface_id: u32,
        expected_size: (i32, i32),
    ) -> u64 {
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = self.next_generation;
        self.phase = SelfParentPhase::ResizePending {
            generation,
            target,
            surface_id,
            expected_size: Some(expected_size),
            configure_surface_id: None,
            configure_serial: None,
            client_ack_seen: false,
            client_commit_seen: false,
            host_resize_ack: None,
        };
        self.deferred_target = None;
        self.completed_origin_guard = None;
        generation
    }

    /// Replace the queued target while an operation is in flight.
    pub(super) fn defer_target(&mut self, target: PlacementRect) -> bool {
        if self.active_target().is_none() {
            return false;
        }
        if self.active_target() == Some(target) {
            self.deferred_target = None;
        } else {
            self.deferred_target = Some(target);
        }
        true
    }

    /// Bind a guest configure token to the active resize generation.
    pub(super) fn bind_configure(&mut self, token: ConfigureToken) -> bool {
        let SelfParentPhase::ResizePending {
            generation,
            target,
            surface_id,
            expected_size,
            host_resize_ack,
            ..
        } = self.phase
        else {
            return false;
        };
        self.phase = SelfParentPhase::ResizePending {
            generation,
            target,
            surface_id,
            expected_size,
            configure_surface_id: Some(token.xdg_surface_id),
            configure_serial: Some(token.serial),
            client_ack_seen: false,
            client_commit_seen: false,
            host_resize_ack,
        };
        true
    }

    /// Model the client side of the handshake for protocol-independent state
    /// tests and legacy callers. The wire adapter always follows this with
    /// `bind_configure`, which resets the gate until the real guest events
    /// arrive.
    pub(super) fn assume_client_ready(&mut self) -> bool {
        let SelfParentPhase::ResizePending {
            generation,
            target,
            surface_id,
            expected_size,
            host_resize_ack,
            ..
        } = self.phase
        else {
            return false;
        };
        self.phase = SelfParentPhase::ResizePending {
            generation,
            target,
            surface_id,
            expected_size,
            configure_surface_id: None,
            configure_serial: None,
            client_ack_seen: true,
            client_commit_seen: true,
            host_resize_ack,
        };
        true
    }

    /// Consume a matching guest XDG configure acknowledgement.
    pub(super) fn note_client_ack(&mut self, xdg_surface_id: u32, serial: u32) -> ResizeGateResult {
        let SelfParentPhase::ResizePending {
            generation,
            target,
            surface_id,
            expected_size,
            configure_surface_id: Some(configure_surface_id),
            configure_serial: Some(configure_serial),
            client_commit_seen,
            host_resize_ack,
            ..
        } = self.phase
        else {
            return ResizeGateResult::Waiting;
        };
        if configure_surface_id != xdg_surface_id || configure_serial != serial {
            return ResizeGateResult::Waiting;
        }
        self.phase = SelfParentPhase::ResizePending {
            generation,
            target,
            surface_id,
            expected_size,
            configure_surface_id: Some(configure_surface_id),
            configure_serial: Some(configure_serial),
            client_ack_seen: true,
            client_commit_seen,
            host_resize_ack,
        };
        self.try_open_resize()
    }

    /// Record the guest commit associated with the active configure.
    pub(super) fn note_client_commit(&mut self) -> ResizeGateResult {
        let SelfParentPhase::ResizePending {
            generation,
            target,
            surface_id,
            expected_size,
            configure_surface_id,
            configure_serial,
            client_ack_seen: true,
            host_resize_ack,
            ..
        } = self.phase
        else {
            return ResizeGateResult::Waiting;
        };
        self.phase = SelfParentPhase::ResizePending {
            generation,
            target,
            surface_id,
            expected_size,
            configure_surface_id,
            configure_serial,
            client_ack_seen: true,
            client_commit_seen: true,
            host_resize_ack,
        };
        self.try_open_resize()
    }

    /// Accept or buffer one host resize configure.
    pub(super) fn note_host_resize(
        &mut self,
        reported_size: (i32, i32),
        origin: PlacementPoint,
        max_adjustment: i32,
    ) -> HostResizeResult {
        if let SelfParentPhase::ResizePending {
            expected_size: None,
            host_resize_ack: Some(ack),
            ..
        } = self.phase
        {
            // Repeated Aura configures for an already accepted logical size
            // are idempotent. They must not reopen or regress the transaction.
            return HostResizeResult::Accepted { origin: ack.origin };
        }
        let SelfParentPhase::ResizePending {
            generation,
            target,
            surface_id,
            expected_size: Some(expected_size),
            configure_serial,
            configure_surface_id,
            client_ack_seen,
            client_commit_seen,
            ..
        } = self.phase
        else {
            return HostResizeResult::Ignored;
        };
        if !is_acceptable_size(expected_size, reported_size, max_adjustment) {
            return HostResizeResult::Ignored;
        }
        let host_resize_ack = HostResizeAck {
            size: reported_size,
            origin,
        };
        self.phase = SelfParentPhase::ResizePending {
            generation,
            target,
            surface_id,
            expected_size: Some(expected_size),
            configure_surface_id,
            configure_serial,
            client_ack_seen,
            client_commit_seen,
            host_resize_ack: Some(host_resize_ack),
        };
        if client_ack_seen && client_commit_seen {
            self.finish_resize(host_resize_ack)
        } else {
            HostResizeResult::Buffered
        }
    }

    /// Return the origin when both client events and a host configure are
    /// present. This is also called after a late guest ack/commit.
    fn try_open_resize(&mut self) -> ResizeGateResult {
        let SelfParentPhase::ResizePending {
            client_ack_seen: true,
            client_commit_seen: true,
            host_resize_ack: Some(host_resize_ack),
            ..
        } = self.phase
        else {
            return ResizeGateResult::Waiting;
        };
        match self.finish_resize(host_resize_ack) {
            HostResizeResult::Accepted { origin } => ResizeGateResult::Ready { origin },
            HostResizeResult::Ignored | HostResizeResult::Buffered => ResizeGateResult::Waiting,
        }
    }

    fn finish_resize(&mut self, host_resize_ack: HostResizeAck) -> HostResizeResult {
        let SelfParentPhase::ResizePending {
            generation,
            target,
            surface_id,
            configure_surface_id,
            configure_serial,
            ..
        } = self.phase
        else {
            return HostResizeResult::Ignored;
        };
        self.phase = SelfParentPhase::ResizePending {
            generation,
            target,
            surface_id,
            expected_size: None,
            configure_surface_id,
            configure_serial,
            client_ack_seen: true,
            client_commit_seen: true,
            host_resize_ack: Some(host_resize_ack),
        };
        HostResizeResult::Accepted {
            origin: host_resize_ack.origin,
        }
    }

    /// Return the generation of the active transaction.
    pub(super) const fn active_generation(&self) -> Option<u64> {
        match self.phase {
            SelfParentPhase::Idle => None,
            SelfParentPhase::ResizePending { generation, .. }
            | SelfParentPhase::MovePending { generation, .. }
            | SelfParentPhase::CleanupPending { generation, .. } => Some(generation),
        }
    }

    /// Mark the self-parent request as queued.
    pub(super) fn mark_move_queued(
        &mut self,
        origin: PlacementPoint,
        target: PlacementRect,
    ) -> bool {
        let SelfParentPhase::ResizePending {
            generation,
            target: active_target,
            surface_id,
            expected_size: None,
            ..
        } = self.phase
        else {
            return false;
        };
        if active_target != target {
            return false;
        }
        self.phase = SelfParentPhase::MovePending {
            generation,
            target,
            surface_id,
            origin_acknowledged: origin == (target.0, target.1),
        };
        true
    }

    /// Test-only compatibility helper for modelling an already queued move
    /// without serializing a Wayland request.
    #[cfg(test)]
    pub(super) fn assume_move_pending(&mut self, target: PlacementRect, surface_id: u32) {
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        self.phase = SelfParentPhase::MovePending {
            generation: self.next_generation,
            target,
            surface_id,
            origin_acknowledged: false,
        };
    }

    /// Move to cleanup after the self-parent barrier completes.
    pub(super) fn begin_cleanup(&mut self) -> bool {
        let SelfParentPhase::MovePending {
            generation,
            target,
            surface_id,
            origin_acknowledged,
        } = self.phase
        else {
            return false;
        };
        self.phase = SelfParentPhase::CleanupPending {
            generation,
            target,
            surface_id,
            origin_acknowledged,
            cleanup_barrier_pending: true,
        };
        true
    }

    /// Mark the follow-up cleanup barrier as complete.
    pub(super) fn complete_cleanup_barrier(&mut self) -> bool {
        let SelfParentPhase::CleanupPending {
            generation,
            target,
            surface_id,
            origin_acknowledged,
            cleanup_barrier_pending: true,
        } = self.phase
        else {
            return false;
        };
        self.phase = SelfParentPhase::CleanupPending {
            generation,
            target,
            surface_id,
            origin_acknowledged,
            cleanup_barrier_pending: false,
        };
        true
    }

    /// Settle the target origin after the nullable-unparent cleanup barrier.
    ///
    /// Some ChromeOS hosts process the self-parent request and its NULL-parent
    /// cleanup without emitting a final `origin_change`.  The barrier is
    /// ordered after the parent request, so the requested target is the only
    /// safe origin available at this point.  This method is deliberately
    /// restricted to `CleanupPending` with a completed cleanup barrier; a
    /// stale callback cannot settle a resize or a newer generation.
    pub(super) fn settle_origin_after_cleanup_barrier(&mut self) -> bool {
        let SelfParentPhase::CleanupPending {
            generation,
            target,
            surface_id,
            origin_acknowledged: false,
            cleanup_barrier_pending: false,
        } = self.phase
        else {
            return false;
        };
        self.phase = SelfParentPhase::CleanupPending {
            generation,
            target,
            surface_id,
            origin_acknowledged: true,
            cleanup_barrier_pending: false,
        };
        true
    }

    /// Abort a resize phase before any move or cleanup barrier was queued.
    ///
    /// This is used when the wire adapter loses the role between plan
    /// preparation and publication. Leaving `ResizePending` alive in that
    /// case would make every later shortcut look like a deferred duplicate
    /// even though the client never received the configure.
    pub(super) fn abort_resize(&mut self) -> bool {
        if !matches!(self.phase, SelfParentPhase::ResizePending { .. }) {
            return false;
        }
        self.phase = SelfParentPhase::Idle;
        self.deferred_target = None;
        true
    }

    /// Feed a host origin event into the transaction.
    ///
    /// Once a self-parent cleanup has completed, the host may continue to
    /// emit animation/focus `origin_change` events from the old parent
    /// generation. Keep the settled target as the authoritative baseline
    /// until the next placement generation starts; accepting one matching
    /// coordinate and clearing the guard would allow a later animation frame
    /// to rebase the next shortcut.
    pub(super) fn note_origin(&mut self, origin: PlacementPoint) -> OriginResult {
        match self.phase {
            SelfParentPhase::Idle => {
                if self.completed_origin_guard.is_some() {
                    return OriginResult::Ignored;
                }
                OriginResult::Accepted {
                    transaction_complete: false,
                }
            }
            SelfParentPhase::ResizePending {
                expected_size: None,
                ..
            } => {
                // A matching host size acknowledgement does not make the
                // origin in that configure authoritative. Exo may report an
                // animation/widget coordinate while it applies the resize.
                // The owner keeps the last settled origin as the baseline and
                // only accepts a new origin after the parent phase is queued.
                OriginResult::Ignored
            }
            SelfParentPhase::ResizePending { .. } => OriginResult::Ignored,
            SelfParentPhase::MovePending {
                target,
                generation,
                surface_id,
                ..
            } => {
                if origin != (target.0, target.1) {
                    return OriginResult::Ignored;
                }
                self.phase = SelfParentPhase::MovePending {
                    target,
                    generation,
                    surface_id,
                    origin_acknowledged: true,
                };
                OriginResult::Accepted {
                    transaction_complete: false,
                }
            }
            SelfParentPhase::CleanupPending {
                target,
                generation,
                surface_id,
                cleanup_barrier_pending,
                ..
            } => {
                if origin != (target.0, target.1) {
                    return OriginResult::Ignored;
                }
                self.phase = SelfParentPhase::CleanupPending {
                    target,
                    generation,
                    surface_id,
                    origin_acknowledged: true,
                    cleanup_barrier_pending,
                };
                OriginResult::Accepted {
                    transaction_complete: !cleanup_barrier_pending,
                }
            }
        }
    }

    /// Return whether the active operation has observed its target origin.
    pub(super) const fn origin_settled(&self) -> bool {
        matches!(
            self.phase,
            SelfParentPhase::MovePending {
                origin_acknowledged: true,
                ..
            } | SelfParentPhase::CleanupPending {
                origin_acknowledged: true,
                ..
            }
        )
    }

    /// Complete cleanup when both host acknowledgements have arrived.
    pub(super) fn complete(&mut self) -> bool {
        let SelfParentPhase::CleanupPending {
            target,
            origin_acknowledged: true,
            cleanup_barrier_pending: false,
            ..
        } = self.phase
        else {
            return false;
        };
        if self.deferred_target.is_some() {
            return false;
        }
        self.last_completed_target = Some(target);
        self.completed_origin_guard = Some((target.0, target.1));
        self.phase = SelfParentPhase::Idle;
        true
    }

    /// Abort cleanup without pretending that an origin was observed.
    pub(super) fn abort_cleanup(&mut self) -> bool {
        if !matches!(self.phase, SelfParentPhase::CleanupPending { .. }) {
            return false;
        }
        // Cleanup failure is not a successful placement. Do not publish the
        // target as completed: callers must be able to retry the same
        // rectangle, and no post-completion origin guard is justified when
        // the host may never have applied the move.
        self.deferred_target = None;
        self.phase = SelfParentPhase::Idle;
        true
    }

    /// Promote a deferred target after cleanup has fully converged.
    pub(super) fn promote_deferred(
        &mut self,
        target: PlacementRect,
        expected_size: Option<(i32, i32)>,
    ) -> bool {
        let surface_id = match self.phase {
            SelfParentPhase::ResizePending {
                surface_id,
                expected_size: None,
                ..
            }
            | SelfParentPhase::CleanupPending {
                surface_id,
                origin_acknowledged: true,
                cleanup_barrier_pending: false,
                ..
            } => surface_id,
            _ => return false,
        };
        if self.deferred_target != Some(target) {
            return false;
        }
        self.deferred_target = None;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = self.next_generation;
        self.phase = SelfParentPhase::ResizePending {
            generation,
            target,
            surface_id,
            expected_size,
            configure_surface_id: None,
            configure_serial: None,
            // Protocol-independent callers model the guest handshake
            // implicitly. The wire adapter binds a real configure token next
            // and resets both bits until the guest events arrive.
            client_ack_seen: true,
            client_commit_seen: true,
            host_resize_ack: None,
        };
        true
    }
}

fn is_acceptable_size(
    expected_size: (i32, i32),
    reported_size: (i32, i32),
    max_adjustment: i32,
) -> bool {
    if expected_size == reported_size {
        return true;
    }
    let width_delta = expected_size.0.saturating_sub(reported_size.0);
    let height_delta = expected_size.1.saturating_sub(reported_size.1);
    reported_size.0 > 0
        && reported_size.1 > 0
        && width_delta >= 0
        && height_delta >= 0
        && width_delta <= max_adjustment
        && height_delta <= max_adjustment
}

#[cfg(test)]
mod tests {
    use super::{
        ConfigureToken, HostResizeResult, OriginResult, PlacementTransaction, ResizeGateResult,
        SelfParentPhase,
    };

    const TARGET: (i32, i32, i32, i32) = (0, 0, 1920, 1080);

    #[test]
    fn host_first_ack_is_buffered_then_released_by_guest_ack_and_commit() {
        let mut transaction = PlacementTransaction::default();
        assert_eq!(transaction.begin_resize(TARGET, 44, (1920, 1080)), 1);
        assert!(transaction.bind_configure(ConfigureToken {
            xdg_surface_id: 11,
            serial: 77,
        }));
        assert_eq!(
            transaction.note_host_resize((1920, 1080), (100, 200), 256),
            HostResizeResult::Buffered
        );
        assert_eq!(
            transaction.note_client_ack(11, 77),
            ResizeGateResult::Waiting
        );
        assert_eq!(
            transaction.note_client_commit(),
            ResizeGateResult::Ready { origin: (100, 200) }
        );
        assert!(!transaction.origin_settled());
    }

    #[test]
    fn stale_ack_and_origin_cannot_advance_a_new_generation() {
        let mut transaction = PlacementTransaction::default();
        assert_eq!(transaction.begin_resize(TARGET, 44, (1920, 1080)), 1);
        assert!(transaction.bind_configure(ConfigureToken {
            xdg_surface_id: 11,
            serial: 77,
        }));
        assert_eq!(
            transaction.note_client_ack(11, 76),
            ResizeGateResult::Waiting
        );
        assert_eq!(
            transaction.note_host_resize((800, 600), (100, 200), 256),
            HostResizeResult::Ignored
        );
        assert_eq!(transaction.active_generation(), Some(1));
    }

    #[test]
    fn commit_before_matching_ack_is_not_used_as_the_resize_commit() {
        let mut transaction = PlacementTransaction::default();
        transaction.begin_resize(TARGET, 44, (1920, 1080));
        assert!(transaction.bind_configure(ConfigureToken {
            xdg_surface_id: 11,
            serial: 77,
        }));
        assert_eq!(
            transaction.note_client_commit(),
            ResizeGateResult::Waiting,
            "a commit before ack_configure may belong to an older client state"
        );
        assert_eq!(
            transaction.note_host_resize((1920, 1080), (100, 200), 256),
            HostResizeResult::Buffered
        );
        assert_eq!(
            transaction.note_client_ack(11, 77),
            ResizeGateResult::Waiting,
            "the early commit must not satisfy the configure handshake"
        );
        assert_eq!(
            transaction.note_client_commit(),
            ResizeGateResult::Ready { origin: (100, 200) }
        );
    }

    #[test]
    fn duplicate_target_is_coalesced_and_cleanup_waits_for_both_events() {
        let mut transaction = PlacementTransaction::default();
        transaction.begin_resize(TARGET, 44, (1920, 1080));
        assert!(transaction.bind_configure(ConfigureToken {
            xdg_surface_id: 11,
            serial: 77,
        }));
        assert_eq!(
            transaction.note_host_resize((1920, 1080), (100, 200), 256),
            HostResizeResult::Buffered
        );
        assert_eq!(
            transaction.note_client_ack(11, 77),
            ResizeGateResult::Waiting
        );
        assert_eq!(
            transaction.note_client_commit(),
            ResizeGateResult::Ready { origin: (100, 200) }
        );
        assert!(transaction.mark_move_queued((100, 200), TARGET));
        assert!(transaction.begin_cleanup());
        assert_eq!(transaction.note_origin((400, 500)), OriginResult::Ignored);
        assert_eq!(
            transaction.note_origin((0, 0)),
            OriginResult::Accepted {
                transaction_complete: false
            }
        );
        assert!(transaction.complete_cleanup_barrier());
        assert_eq!(
            transaction.note_origin((0, 0)),
            OriginResult::Accepted {
                transaction_complete: true
            }
        );
        assert!(transaction.complete());
    }

    #[test]
    fn cleanup_barrier_can_settle_when_host_omits_origin_change() {
        let mut transaction = PlacementTransaction::default();
        transaction.begin_resize(TARGET, 44, (1920, 1080));
        assert!(transaction.assume_client_ready());
        assert_eq!(
            transaction.note_host_resize((1920, 1080), (100, 200), 256),
            HostResizeResult::Accepted { origin: (100, 200) }
        );
        assert!(transaction.mark_move_queued((100, 200), TARGET));
        assert!(transaction.begin_cleanup());
        assert!(transaction.complete_cleanup_barrier());
        assert!(transaction.settle_origin_after_cleanup_barrier());
        assert!(transaction.origin_settled());
        assert_eq!(
            transaction.note_origin((400, 500)),
            OriginResult::Ignored,
            "a later focus coordinate must not rebase the settled target"
        );
        assert!(transaction.complete());
    }

    #[test]
    fn cleanup_barrier_settlement_requires_the_active_completed_generation() {
        let mut transaction = PlacementTransaction::default();
        transaction.begin_resize(TARGET, 44, (1920, 1080));
        assert!(transaction.assume_client_ready());
        assert_eq!(
            transaction.note_host_resize((1920, 1080), (100, 200), 256),
            HostResizeResult::Accepted { origin: (100, 200) }
        );
        assert!(transaction.mark_move_queued((100, 200), TARGET));
        assert!(transaction.begin_cleanup());
        assert!(
            !transaction.settle_origin_after_cleanup_barrier(),
            "the cleanup barrier must be acknowledged before fallback settlement"
        );
        assert!(transaction.complete_cleanup_barrier());
        assert!(transaction.settle_origin_after_cleanup_barrier());
        assert!(
            !transaction.settle_origin_after_cleanup_barrier(),
            "a completed fallback must not be applied twice"
        );
    }

    #[test]
    fn completed_origin_guard_survives_matching_then_late_animation_origin() {
        let mut transaction = PlacementTransaction::default();
        transaction.begin_resize(TARGET, 44, (1920, 1080));
        assert!(transaction.assume_client_ready());
        assert_eq!(
            transaction.note_host_resize((1920, 1080), (100, 200), 256),
            HostResizeResult::Accepted { origin: (100, 200) }
        );
        assert!(transaction.mark_move_queued((100, 200), TARGET));
        assert!(transaction.begin_cleanup());
        assert!(transaction.complete_cleanup_barrier());
        assert!(transaction.settle_origin_after_cleanup_barrier());
        assert!(transaction.complete());

        // A matching late event is not evidence that host animation has ended.
        // A later frame must remain unable to rebase the next shortcut.
        assert_eq!(transaction.note_origin((0, 0)), OriginResult::Ignored);
        assert_eq!(transaction.note_origin((785, 541)), OriginResult::Ignored);
    }

    #[test]
    fn starting_a_new_generation_releases_the_completed_origin_guard() {
        let mut transaction = PlacementTransaction::default();
        transaction.begin_resize(TARGET, 44, (1920, 1080));
        assert!(transaction.assume_client_ready());
        assert_eq!(
            transaction.note_host_resize((1920, 1080), (100, 200), 256),
            HostResizeResult::Accepted { origin: (100, 200) }
        );
        assert!(transaction.mark_move_queued((100, 200), TARGET));
        assert!(transaction.begin_cleanup());
        assert!(transaction.complete_cleanup_barrier());
        assert!(transaction.settle_origin_after_cleanup_barrier());
        assert!(transaction.complete());

        let next_target = (1920, 0, 1920, 1080);
        transaction.begin_resize(next_target, 44, (1920, 1080));
        assert_eq!(
            transaction.note_origin((785, 541)),
            OriginResult::Ignored,
            "active resize phases still reject untrusted animation origins"
        );
        assert!(transaction.abort_resize());
        assert_eq!(
            transaction.note_origin((785, 541)),
            OriginResult::Accepted {
                transaction_complete: false
            },
            "the next generation explicitly releases the old post-cleanup guard"
        );
    }

    #[test]
    fn aborted_cleanup_does_not_mark_target_as_completed_or_block_retry() {
        let mut transaction = PlacementTransaction::default();
        transaction.assume_move_pending(TARGET, 44);
        assert!(transaction.begin_cleanup());
        assert!(transaction.abort_cleanup());
        assert_eq!(transaction.phase(), SelfParentPhase::Idle);
        assert_eq!(
            transaction.last_completed_target(),
            None,
            "a failed cleanup is not a completed placement"
        );
        assert_eq!(
            transaction.note_origin((785, 541)),
            OriginResult::Accepted {
                transaction_complete: false
            },
            "failed cleanup must not leave a permanent origin guard"
        );
        assert_eq!(transaction.begin_resize(TARGET, 44, (1920, 1080)), 2);
    }
}
