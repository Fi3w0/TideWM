use std::collections::HashMap;

use smithay::reexports::calloop::timer::{TimeoutAction, Timer};

use super::{engine_name, IpcSeenState, Smallvil};

impl Smallvil {
    /// Broadcasts one state-change event to every matching IPC subscriber.
    /// The fast path (no subscribers connected) is a single `is_empty`
    /// check and return -- this is called from focus/workspace/map/unmap
    /// hot paths and must stay cheap when nothing is listening.
    ///
    /// For real subscribers, the per-event JSON line is snapshotted once
    /// (the borrow-checker-friendly shared-borrow path through
    /// `IpcEvent::to_json_line`), then appended to each subscriber's
    /// `pending` deque with an inline `try_flush` so a healthy subscriber
    /// gets sub-millisecond latency without waiting on the retry timer.
    /// A subscriber whose `pending` exceeds `SUBSCRIBER_PENDING_CAP` after
    /// the attempt is retired via `remove_ipc_subscriber` -- bounded
    /// memory wins over best-effort delivery to a wedged client.
    /// Emits `output-changed`, `mode-changed`, `engine-changed` and
    /// `scroll-changed` by comparing a small fingerprint of that state with
    /// the last one reported. Runs once per event-loop dispatch, so every
    /// path that changes the state is covered without hooks at each site.
    /// Skipped entirely while nobody listens.
    pub(crate) fn emit_state_change_events(&mut self) {
        const NAMES: [&str; 4] = [
            "output-changed",
            "mode-changed",
            "engine-changed",
            "scroll-changed",
        ];
        let listening =
            !self.ipc_subscribers.is_empty() || NAMES.iter().any(|name| self.has_lua_handler(name));
        if !listening {
            self.ipc_seen = None;
            return;
        }
        let mut now = self.ipc_seen_state();
        // A drag or swipe moves the strip on every motion event; report
        // Scroll once, with the final state, when the interaction ends.
        let interacting = self.compositor_gesture.is_some()
            || self
                .seat
                .get_pointer()
                .is_some_and(|pointer| pointer.is_grabbed());
        if interacting {
            if let Some(before) = &self.ipc_seen {
                now.scroll.clone_from(&before.scroll);
            }
        }
        let Some(before) = self.ipc_seen.take() else {
            self.ipc_seen = Some(now);
            return;
        };
        use crate::ipc::IpcEvent;
        for (name, fingerprint) in &now.outputs {
            match before.outputs.get(name) {
                None => self.emit_ipc_event(IpcEvent::OutputChanged {
                    output: name.clone(),
                    change: "added",
                }),
                Some(old) if old != fingerprint => self.emit_ipc_event(IpcEvent::OutputChanged {
                    output: name.clone(),
                    change: "changed",
                }),
                _ => {}
            }
        }
        for name in before
            .outputs
            .keys()
            .filter(|name| !now.outputs.contains_key(*name))
        {
            self.emit_ipc_event(IpcEvent::OutputChanged {
                output: name.clone(),
                change: "removed",
            });
        }
        if before.mode != now.mode {
            self.emit_ipc_event(IpcEvent::ModeChanged {
                mode: now.mode.clone(),
            });
        }
        if before.engine != now.engine {
            self.emit_ipc_event(IpcEvent::EngineChanged {
                from: before.engine,
                to: now.engine,
            });
        }
        for (name, fingerprint) in &now.scroll {
            if before.scroll.get(name) != Some(fingerprint) {
                self.emit_ipc_event(IpcEvent::ScrollChanged {
                    output: name.clone(),
                });
            }
        }
        self.ipc_seen = Some(now);
    }

    fn ipc_seen_state(&self) -> IpcSeenState {
        use std::hash::{Hash, Hasher};
        let fingerprint = |write: &dyn Fn(&mut std::collections::hash_map::DefaultHasher)| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            write(&mut hasher);
            hasher.finish()
        };
        let outputs = self
            .space
            .outputs()
            .map(|output| {
                let print = fingerprint(&|h| {
                    let loc = output.current_location();
                    (loc.x, loc.y).hash(h);
                    output
                        .current_mode()
                        .map(|m| (m.size.w, m.size.h, m.refresh))
                        .hash(h);
                    output.current_scale().fractional_scale().to_bits().hash(h);
                    format!("{:?}", output.current_transform()).hash(h);
                });
                (output.name(), print)
            })
            .collect();
        let scrolling =
            self.spatial.engine.projection() == Some(crate::config::WorkspaceProjection::Scroll);
        let scroll = if scrolling {
            self.space
                .outputs()
                .map(|output| {
                    let name = output.name();
                    let workspace = self.layout.active_workspace(&name);
                    let print = fingerprint(&|h| {
                        workspace.hash(h);
                        self.layout
                            .scroll
                            .offset(&name, self.layout.scroll_key(workspace))
                            .to_bits()
                            .hash(h);
                        for column in self.layout.scroll_columns(&name, workspace) {
                            column.width.to_bits().hash(h);
                            for surface in &column.windows {
                                self.foreign_toplevel_numeric_ids.get(surface).hash(h);
                            }
                        }
                    });
                    (name, print)
                })
                .collect()
        } else {
            HashMap::new()
        };
        IpcSeenState {
            outputs,
            mode: self.active_mode.clone(),
            engine: engine_name(self.spatial.engine),
            scroll,
        }
    }

    pub(crate) fn emit_ipc_event(&mut self, event: crate::ipc::IpcEvent) {
        // Wave `on "event"` handlers run regardless of subscribers.
        self.run_lua_handlers(&event);
        if self.ipc_subscribers.is_empty() {
            return;
        }
        let payload = event.to_json_line(self);
        let mut to_drop: Vec<usize> = Vec::new();
        for (id, sub) in self.ipc_subscribers.iter_mut() {
            if !crate::ipc::event_matches(&sub.filter, &event) {
                continue;
            }
            sub.pending.extend(&payload);
            if !sub.try_flush() {
                // Peer closed between events. Don't keep queuing; drop on
                // the next sweep unless `remove_ipc_subscriber` already
                // grabbed it (the read-side EOF watcher may have beaten us
                // to the entry). Marking here makes the next flush's
                // `remove_ipc_subscriber` retire the entry.
                to_drop.push(*id);
                continue;
            }
            if sub.pending.len() > crate::ipc::SUBSCRIBER_PENDING_CAP {
                tracing::warn!(
                    id,
                    pending = sub.pending.len(),
                    cap = crate::ipc::SUBSCRIBER_PENDING_CAP,
                    "IPC subscriber exceeded pending cap; dropping"
                );
                to_drop.push(*id);
            }
        }
        for id in to_drop {
            self.remove_ipc_subscriber(id);
        }
        self.arm_ipc_flush_timer();
    }

    /// Retry path for subscribers whose kernel write buffer was full at
    /// `emit_ipc_event` time. Also retires any subscriber flagged for removal
    /// mid-emit (peer-closed or cap-exceeded).
    pub(crate) fn flush_ipc_subscribers(&mut self) {
        if self.ipc_subscribers.is_empty() {
            return;
        }
        let mut to_drop: Vec<usize> = Vec::new();
        for (id, sub) in self.ipc_subscribers.iter_mut() {
            if !sub.try_flush() {
                to_drop.push(*id);
                continue;
            }
            if sub.pending.len() > crate::ipc::SUBSCRIBER_PENDING_CAP {
                to_drop.push(*id);
            }
        }
        for id in to_drop {
            self.remove_ipc_subscriber(id);
        }
    }

    /// Arms the subscriber retry timer only while at least one socket has
    /// unwritten bytes. Healthy subscribers flush inline and never reach this
    /// path, keeping the compositor event loop quiescent when IPC is idle.
    pub(crate) fn arm_ipc_flush_timer(&mut self) {
        if self.ipc_flush_token.is_some()
            || !self
                .ipc_subscribers
                .values()
                .any(|subscriber| !subscriber.pending.is_empty())
        {
            return;
        }

        let loop_handle = self.loop_handle.clone();
        match loop_handle.insert_source(
            Timer::from_duration(crate::ipc::FLUSH_INTERVAL),
            |_, _, state: &mut Smallvil| {
                state.flush_ipc_subscribers();
                if state
                    .ipc_subscribers
                    .values()
                    .any(|subscriber| !subscriber.pending.is_empty())
                {
                    TimeoutAction::ToDuration(crate::ipc::FLUSH_INTERVAL)
                } else {
                    state.ipc_flush_token = None;
                    TimeoutAction::Drop
                }
            },
        ) {
            Ok(token) => self.ipc_flush_token = Some(token),
            Err(err) => tracing::warn!(%err, "Failed to arm IPC subscriber flush timer"),
        }
    }

    /// Tears down a subscriber: removes its entry from `ipc_subscribers`
    /// (dropping the write-side FD) and retires its read-side EOF source
    /// via `loop_handle.remove`. Safe to call from inside any calloop
    /// callback, not just the EOF source itself -- calloop's
    /// `loop_handle.remove` is documented as idempotent against
    /// already-removed tokens, and cross-source removal from within
    /// another source's callback is the supported pattern (interior
    /// `RefCell` borrow, brief).
    fn remove_ipc_subscriber(&mut self, id: usize) {
        if let Some(mut sub) = self.ipc_subscribers.remove(&id) {
            if let Some(token) = sub.read_token.take() {
                self.loop_handle.remove(token);
            }
            drop(sub);
        }
    }
}
