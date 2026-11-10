use std::collections::HashMap;

use smithay::reexports::calloop::{
    timer::{TimeoutAction, Timer},
    RegistrationToken,
};

use super::{engine_name, IpcSeenState, Smallvil};

/// IPC subscriber bookkeeping, grouped out of `Smallvil` (plan step 8a).
pub(crate) struct IpcState {
    /// Long-lived IPC subscribe connections, keyed by a process-unique id
    /// assigned at subscribe time. Empty in the steady state where no
    /// widget is connected, which makes `emit_ipc_event` a cheap
    /// early-return on every state change. See `ipc::IpcSubscriber` for
    /// the per-connection fields and lifecycle.
    pub(crate) subscribers: HashMap<usize, crate::ipc::IpcSubscriber>,
    /// Last state `emit_state_change_events` reported, `None` until a
    /// subscriber or handler first needs it.
    pub(super) seen: Option<IpcSeenState>,
    /// One-shot retry timer for subscriber bytes that did not fit in the
    /// kernel socket buffer. `None` during the normal fully-drained state, so
    /// IPC adds no periodic wakeup to an idle compositor.
    pub(crate) flush_token: Option<RegistrationToken>,
    /// A `pick-window` request waiting for the next click.
    pub(crate) pick: Option<crate::ipc::WindowPick>,
    /// Next subscriber id. Monotonic across the process lifetime rather
    /// than recycled, so a stale reference inside an in-flight calloop
    /// callback can't accidentally target a fresh subscriber that reused
    /// the same id.
    pub(crate) next_subscriber_id: usize,
}

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
            !self.ipc.subscribers.is_empty() || NAMES.iter().any(|name| self.has_lua_handler(name));
        if !listening {
            self.ipc.seen = None;
            return;
        }
        let mut now = self.ipc_seen_state();
        // A drag or swipe moves the strip on every motion event; report
        // Scroll once, with the final state, when the interaction ends.
        let interacting = self.input.compositor_gesture.is_some()
            || self
                .seat
                .get_pointer()
                .is_some_and(|pointer| pointer.is_grabbed());
        if interacting {
            if let Some(before) = &self.ipc.seen {
                now.scroll.clone_from(&before.scroll);
            }
        }
        let Some(before) = self.ipc.seen.take() else {
            self.ipc.seen = Some(now);
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
        self.ipc.seen = Some(now);
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
                    let workspace = self.spatial.workspaces.layout.active_workspace(&name);
                    let print = fingerprint(&|h| {
                        workspace.hash(h);
                        self.spatial
                            .workspaces
                            .layout
                            .scroll
                            .offset(&name, self.spatial.workspaces.layout.scroll_key(workspace))
                            .to_bits()
                            .hash(h);
                        for column in self
                            .spatial
                            .workspaces
                            .layout
                            .scroll_columns(&name, workspace)
                        {
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
            mode: self.input.active_mode.clone(),
            engine: engine_name(self.spatial.engine),
            scroll,
        }
    }

    pub(crate) fn emit_ipc_event(&mut self, event: crate::ipc::IpcEvent) {
        // Wave `on "event"` handlers run regardless of subscribers.
        self.run_lua_handlers(&event);
        if self.ipc.subscribers.is_empty() {
            return;
        }
        let payload = event.to_json_line(self);
        let mut to_drop: Vec<usize> = Vec::new();
        for (id, sub) in self.ipc.subscribers.iter_mut() {
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
        if self.ipc.subscribers.is_empty() {
            return;
        }
        let mut to_drop: Vec<usize> = Vec::new();
        for (id, sub) in self.ipc.subscribers.iter_mut() {
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
        if self.ipc.flush_token.is_some()
            || !self
                .ipc
                .subscribers
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
                    .ipc
                    .subscribers
                    .values()
                    .any(|subscriber| !subscriber.pending.is_empty())
                {
                    TimeoutAction::ToDuration(crate::ipc::FLUSH_INTERVAL)
                } else {
                    state.ipc.flush_token = None;
                    TimeoutAction::Drop
                }
            },
        ) {
            Ok(token) => self.ipc.flush_token = Some(token),
            Err(err) => tracing::warn!(%err, "Failed to arm IPC subscriber flush timer"),
        }
    }

    /// Starts waiting for the click that answers a `pick-window` request.
    pub(crate) fn begin_window_pick(
        &mut self,
        stream: std::os::unix::net::UnixStream,
        lease: crate::ipc::ConnectionLease,
        timeout: Option<std::time::Duration>,
    ) {
        // A newer request wins; the older one is told why.
        self.finish_window_pick(Err("replaced by a newer pick-window request"));
        self.ipc.pick = Some(crate::ipc::WindowPick {
            stream,
            _connection_lease: lease,
            timer: None,
        });
        if !matches!(self.session_lock, super::SessionLock::Unlocked) {
            self.finish_window_pick(Err("the session is locked"));
            return;
        }
        let timeout = timeout.unwrap_or(crate::ipc::PICK_DEFAULT_TIMEOUT).clamp(
            std::time::Duration::from_secs(1),
            crate::ipc::PICK_MAX_TIMEOUT,
        );
        let timer = self.loop_handle.insert_source(
            Timer::from_duration(timeout),
            |_, _, state: &mut Smallvil| {
                if let Some(pick) = state.ipc.pick.as_mut() {
                    pick.timer = None;
                }
                state.finish_window_pick(Err("timed out waiting for a click"));
                TimeoutAction::Drop
            },
        );
        match (timer, self.ipc.pick.as_mut()) {
            (Ok(token), Some(pick)) => pick.timer = Some(token),
            _ => self.finish_window_pick(Err("could not start the pick timer")),
        }
        self.request_redraw();
    }

    /// Whether a pick is waiting and its client is still connected. A client
    /// that went away must not swallow the user's next click.
    pub(crate) fn window_pick_waiting(&mut self) -> bool {
        use std::os::fd::AsRawFd;
        let Some(pick) = self.ipc.pick.as_ref() else {
            return false;
        };
        let mut poll = libc::pollfd {
            fd: pick.stream.as_raw_fd(),
            events: 0,
            revents: 0,
        };
        // Safety: one valid pollfd for an owned live fd, nonblocking timeout.
        let gone = unsafe { libc::poll(&mut poll, 1, 0) } < 0
            || poll.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0;
        if gone {
            self.finish_window_pick(Err("client disconnected"));
        }
        !gone
    }

    /// The press that ends a pick: the left button answers with the window
    /// at `position` (null over empty space), any other button cancels.
    pub(crate) fn answer_window_pick(
        &mut self,
        button: u32,
        position: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) {
        const BTN_LEFT: u32 = 0x110;
        if button != BTN_LEFT {
            self.finish_window_pick(Err("cancelled"));
            return;
        }
        let picked = self
            .window_under(position)
            .and_then(|(window, _)| crate::ipc::window_entry(self, &window));
        self.finish_window_pick(Ok(picked.unwrap_or(serde_json::Value::Null)));
    }

    /// Answers the waiting pick, if any, and ends it.
    pub(crate) fn finish_window_pick(&mut self, result: Result<serde_json::Value, &str>) {
        use std::io::Write;
        let Some(mut pick) = self.ipc.pick.take() else {
            return;
        };
        if let Some(token) = pick.timer.take() {
            self.loop_handle.remove(token);
        }
        let reply = match result {
            Ok(data) => serde_json::json!({ "ok": true, "data": data }),
            Err(error) => serde_json::json!({ "ok": false, "error": error }),
        };
        let mut line = serde_json::to_vec(&reply).unwrap_or_default();
        line.push(b'\n');
        // One short line into an empty socket buffer; a client that cannot
        // take it has gone, and there is nobody to retry for.
        let _ = pick.stream.set_nonblocking(true);
        if let Err(error) = pick.stream.write_all(&line) {
            tracing::debug!(%error, "pick-window reply not delivered");
        }
        self.request_redraw();
    }

    /// Tears down a subscriber: removes its entry from `ipc.subscribers`
    /// (dropping the write-side FD) and retires its read-side EOF source
    /// via `loop_handle.remove`. Safe to call from inside any calloop
    /// callback, not just the EOF source itself -- calloop's
    /// `loop_handle.remove` is documented as idempotent against
    /// already-removed tokens, and cross-source removal from within
    /// another source's callback is the supported pattern (interior
    /// `RefCell` borrow, brief).
    fn remove_ipc_subscriber(&mut self, id: usize) {
        if let Some(mut sub) = self.ipc.subscribers.remove(&id) {
            if let Some(token) = sub.read_token.take() {
                self.loop_handle.remove(token);
            }
            drop(sub);
        }
    }
}
