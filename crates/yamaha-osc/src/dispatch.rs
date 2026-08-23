//! The single point through which every console write passes.
//!
//! Two jobs:
//!
//! * **Pacing.** The console absorbs roughly one command per 5 ms. QLab can fire a 1D Fade at up
//!   to 120 messages a second, and several fading channels multiply that, so something has to
//!   decide what actually goes on the wire.
//! * **Coalescing.** When several moves for the same fader are waiting, only the newest matters —
//!   so an over-driven fade degrades into a slightly coarser ramp instead of building an unbounded
//!   backlog. Discrete actions (mutes, scene recalls, labels) are never coalesced, because
//!   dropping one would be a show error rather than a cosmetic one.
//!
//! Because coalescing is keyed by target, the pending-level map is bounded by the number of
//! addressable targets on the console — it cannot grow without limit no matter how fast OSC
//! arrives.
//!
//! Bridge-driven fades feed their steps back into this same queue rather than writing directly, so
//! there is exactly one writer and "last writer wins" is a rule about one queue, not a race
//! between two.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::MissedTickBehavior;
use tracing::{debug, trace, warn};

use yamaha_rcp::{Mixer, Strip};

use crate::fade;
use crate::router::{Action, FadeSpec, Source, Switch, Target};

/// Counters for `yamaha-osc monitor` and for spotting an over-driven fade in rehearsal.
#[derive(Debug, Default)]
pub struct Stats {
    /// Actions accepted from OSC.
    pub queued: AtomicU64,
    /// Commands actually written to the console.
    pub sent: AtomicU64,
    /// Level moves superseded by a newer value before they were sent.
    pub coalesced: AtomicU64,
    /// Commands the console rejected, or that failed in transit.
    pub failed: AtomicU64,
}

impl Stats {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// A one-line summary for the monitor output.
    pub fn summary(&self) -> String {
        format!(
            "queued {} · sent {} · coalesced {} · failed {}",
            self.queued.load(Ordering::Relaxed),
            self.sent.load(Ordering::Relaxed),
            self.coalesced.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
        )
    }
}

/// Tuning for the dispatcher.
#[derive(Clone, Copy, Debug)]
pub struct DispatchConfig {
    /// Minimum spacing between commands on the wire.
    pub min_command_interval: Duration,
    /// Step interval for bridge-driven fades.
    pub fade_tick: Duration,
    /// Whether to coalesce superseded level moves.
    pub coalesce: bool,
    /// Inbound queue capacity.
    pub queue_depth: usize,
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            min_command_interval: Duration::from_millis(5),
            fade_tick: Duration::from_millis(33),
            coalesce: true,
            queue_depth: 1024,
        }
    }
}

/// Handle to a running dispatcher.
#[derive(Debug)]
pub struct Dispatcher {
    tx: mpsc::Sender<Action>,
    stats: Arc<Stats>,
    task: JoinHandle<()>,
}

impl Dispatcher {
    /// Start a dispatcher writing to `mixer`.
    ///
    /// The mixer is switched to unconfirmed writes: waiting for each `OK` would put a network
    /// round trip in the cue path, and a rejected command is logged rather than returned because
    /// there is nobody left to return it to.
    pub fn spawn(mixer: Mixer, config: DispatchConfig) -> Self {
        let (tx, rx) = mpsc::channel(config.queue_depth);
        let stats = Arc::new(Stats::default());

        let worker = Worker {
            mixer: mixer.confirm_writes(false),
            config,
            stats: Arc::clone(&stats),
            self_tx: tx.clone(),
            queue: Queue::new(config.coalesce),
            fades: HashMap::new(),
        };

        Self {
            tx,
            stats,
            task: tokio::spawn(worker.run(rx)),
        }
    }

    /// Submit an action. Returns `false` if the queue is full or the dispatcher has stopped.
    pub async fn submit(&self, action: Action) -> bool {
        self.tx.send(action).await.is_ok()
    }

    /// Counters for monitoring.
    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }

    /// Stop the dispatcher, abandoning anything still queued.
    pub fn shutdown(&self) {
        self.task.abort();
    }
}

/// The command queue: discrete actions in arrival order, plus one pending level per target.
///
/// Split out from the worker so its ordering and coalescing rules can be tested directly, with no
/// console, no clock and no tasks involved.
#[derive(Debug)]
struct Queue {
    coalesce: bool,
    /// Discrete actions, in arrival order. Never dropped.
    pending: VecDeque<Action>,
    /// Level targets awaiting a write, in the order they were first queued.
    levels: VecDeque<Target>,
    /// The newest value for each queued target.
    level_values: HashMap<Target, i32>,
}

impl Queue {
    fn new(coalesce: bool) -> Self {
        Self {
            coalesce,
            pending: VecDeque::new(),
            levels: VecDeque::new(),
            level_values: HashMap::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.levels.is_empty()
    }

    fn clear(&mut self) {
        self.pending.clear();
        self.levels.clear();
        self.level_values.clear();
    }

    fn push_discrete(&mut self, action: Action) {
        self.pending.push_back(action);
    }

    /// Queue a level, replacing any value already queued for the same target.
    ///
    /// Returns `true` if an older value was superseded. A coalesced target keeps its original
    /// place in line, so a fade's steps stay interleaved with other traffic in the order they
    /// arrived rather than jumping to the back on every update.
    fn push_level(&mut self, target: Target, raw: i32) -> bool {
        if !self.coalesce {
            // With coalescing off every move is delivered, so levels join the ordered queue
            // alongside discrete actions. The per-target map cannot represent a backlog for one
            // target, which is the whole reason it collapses them in the first place.
            self.pending.push_back(Action::SetLevel {
                target,
                raw,
                source: Source::Direct,
            });
            return false;
        }

        let superseded = self.level_values.insert(target, raw).is_some();
        if !superseded {
            self.levels.push_back(target);
        }
        superseded
    }

    /// Discrete actions first: a mute or a scene recall should not wait behind a fade's steps.
    fn next(&mut self) -> Option<Action> {
        if let Some(action) = self.pending.pop_front() {
            return Some(action);
        }
        let target = self.levels.pop_front()?;
        let raw = self.level_values.remove(&target)?;
        Some(Action::SetLevel {
            target,
            raw,
            source: Source::Direct,
        })
    }
}

struct Worker {
    mixer: Mixer,
    config: DispatchConfig,
    stats: Arc<Stats>,
    /// Used by fade tasks to feed their steps back into this queue.
    self_tx: mpsc::Sender<Action>,
    queue: Queue,
    /// Running bridge fades, so a newer write to the same target can cancel one.
    fades: HashMap<Target, AbortHandle>,
}

impl Worker {
    async fn run(mut self, mut rx: mpsc::Receiver<Action>) {
        let mut ticker = tokio::time::interval(self.config.min_command_interval);
        // Delay rather than Burst: after an idle gap we must not fire a backlog of "missed" ticks
        // all at once, which would defeat the pacing exactly when a cue arrives.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            // Absorb everything already waiting, so coalescing sees the whole burst at once.
            while let Ok(action) = rx.try_recv() {
                self.enqueue(action);
            }

            if self.is_idle() {
                match rx.recv().await {
                    Some(action) => {
                        self.enqueue(action);
                        continue;
                    }
                    None => {
                        debug!("dispatcher input closed; stopping");
                        return;
                    }
                }
            }

            // After an idle period this returns immediately, so a lone cue is not delayed.
            ticker.tick().await;

            if let Some(action) = self.next() {
                self.apply(action).await;
            }
        }
    }

    fn is_idle(&self) -> bool {
        self.queue.is_empty()
    }

    fn enqueue(&mut self, action: Action) {
        Stats::bump(&self.stats.queued);

        match action {
            Action::Panic => self.panic_all(),

            Action::StartFade(spec) => self.start_fade(spec),

            Action::SetLevel {
                target,
                raw,
                source,
            } => {
                // A direct write wins over a running fade on the same target; a fade's own steps
                // obviously must not cancel it.
                if source == Source::Direct {
                    self.cancel_fade(target);
                }
                if self.queue.push_level(target, raw) {
                    Stats::bump(&self.stats.coalesced);
                }
            }

            // Reload is handled by the supervisor, not here, but it arrives through the same
            // channel so the ordering with surrounding cues is preserved.
            other => self.queue.push_discrete(other),
        }
    }

    fn next(&mut self) -> Option<Action> {
        self.queue.next()
    }

    async fn apply(&mut self, action: Action) {
        let result = match &action {
            Action::SetLevel { target, raw, .. } => match *target {
                Target::Fader { strip, index } => {
                    self.mixer.set_fader_level(strip, index, *raw).await
                }
                Target::Send { strip, index, dest } => {
                    self.mixer.set_send_level(strip, index, dest, *raw).await
                }
            },
            Action::SetSwitch { switch, on } => match *switch {
                Switch::Strip { strip, index } => self.mixer.set_on(strip, index, *on).await,
                Switch::Send { strip, index, dest } => {
                    self.mixer.set_send_on(strip, index, dest, *on).await
                }
                Switch::MuteMaster { group } => self.mixer.set_mute_master(group, *on).await,
            },
            Action::SetLabel { strip, index, name } => {
                self.mixer.set_label(*strip, *index, name).await
            }
            Action::SetColor {
                strip,
                index,
                color,
            } => self.mixer.set_color(*strip, *index, *color).await,
            Action::RecallScene { list, scene } => {
                self.mixer.recall_scene(*list, *scene as u8).await
            }
            // Handled at enqueue time or by the supervisor; nothing to write.
            Action::Panic | Action::StartFade(_) | Action::Reload => Ok(()),
        };

        match result {
            Ok(()) => {
                Stats::bump(&self.stats.sent);
                trace!(?action, "dispatched");
            }
            Err(e) => {
                Stats::bump(&self.stats.failed);
                warn!(?action, error = %e, "console command failed");
            }
        }
    }

    fn start_fade(&mut self, spec: FadeSpec) {
        self.cancel_fade(spec.target);

        let mixer = self.mixer.clone();
        let tx = self.self_tx.clone();
        let tick = self.config.fade_tick;

        let handle = tokio::spawn(async move {
            // Without an explicit start level, read the console's current one. One round trip
            // (~1 ms on a wired LAN) before the ramp begins, in exchange for a fade that never
            // jumps on its first step.
            let from = match spec.from {
                Some(from) => from,
                None => match read_level(&mixer, spec.target).await {
                    Ok(level) => level,
                    Err(e) => {
                        warn!(target = ?spec.target, error = %e,
                              "could not read current level; starting fade from the target");
                        spec.to
                    }
                },
            };

            let values = fade::plan(from, spec.to, spec.duration, tick, spec.curve);
            let mut ticker = tokio::time::interval(tick);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            ticker.tick().await; // fires immediately

            for (i, raw) in values.iter().enumerate() {
                if i > 0 {
                    ticker.tick().await;
                }
                let sent = tx
                    .send(Action::SetLevel {
                        target: spec.target,
                        raw: *raw,
                        source: Source::Fade,
                    })
                    .await
                    .is_ok();
                if !sent {
                    return;
                }
            }
        });

        self.fades.insert(spec.target, handle.abort_handle());
    }

    /// Stop any running fade on `target`. A cancelled fade leaves the fader where its last step
    /// put it; the write that cancelled it then takes over.
    fn cancel_fade(&mut self, target: Target) {
        if let Some(handle) = self.fades.remove(&target) {
            handle.abort();
            trace!(?target, "fade superseded");
        }
    }

    /// Take everything audible to -∞.
    ///
    /// Ordered so sound actually stops as fast as the console can be told: the stereo and mono
    /// masters first (two commands, ~10 ms), then DCAs, then individual strips. Running fades are
    /// cancelled and the queue is cleared first, so nothing that was already in flight can push a
    /// fader back up afterwards.
    fn panic_all(&mut self) {
        warn!("panic: taking every fader to -inf");

        for (_, handle) in self.fades.drain() {
            handle.abort();
        }
        self.queue.clear();

        let params = self.mixer.params();
        let order = [
            Strip::Stereo,
            Strip::Mono,
            Strip::Dca,
            Strip::Input,
            Strip::StereoInput,
            Strip::FxReturn,
            Strip::Mix,
            Strip::Matrix,
        ];

        for strip in order {
            let Ok(info) = params.get(&strip.path("Fader/Level")) else {
                continue;
            };
            for index in 0..info.instances() {
                self.queue.push_discrete(Action::SetLevel {
                    target: Target::Fader { strip, index },
                    raw: yamaha_rcp::NEG_INF,
                    source: Source::Direct,
                });
            }
        }
    }
}

/// Read a target's current level, whichever kind of target it is.
async fn read_level(mixer: &Mixer, target: Target) -> Result<i32, yamaha_rcp::Error> {
    match target {
        Target::Fader { strip, index } => mixer.fader_level(strip, index).await,
        Target::Send { strip, index, dest } => mixer.send_level(strip, index, dest).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yamaha_rcp::{LabelColor, SceneList};

    fn fader(index: u32) -> Target {
        Target::Fader {
            strip: Strip::Input,
            index,
        }
    }

    fn level(target: Target, raw: i32) -> Action {
        Action::SetLevel {
            target,
            raw,
            source: Source::Direct,
        }
    }

    fn mute(index: u32) -> Action {
        Action::SetSwitch {
            switch: Switch::Strip {
                strip: Strip::Input,
                index,
            },
            on: false,
        }
    }

    fn drain(queue: &mut Queue) -> Vec<Action> {
        std::iter::from_fn(|| queue.next()).collect()
    }

    /// The point of coalescing: a burst of moves for one fader collapses to its newest value.
    #[test]
    fn a_burst_for_one_target_collapses_to_the_newest_value() {
        let mut q = Queue::new(true);
        for raw in [-100, -200, -300, -400] {
            q.push_level(fader(0), raw);
        }
        assert_eq!(drain(&mut q), [level(fader(0), -400)]);
    }

    #[test]
    fn coalescing_reports_when_it_superseded_something() {
        let mut q = Queue::new(true);
        assert!(
            !q.push_level(fader(0), -100),
            "first value is not a supersede"
        );
        assert!(q.push_level(fader(0), -200), "second value supersedes");
        assert!(
            !q.push_level(fader(1), -200),
            "a different target is its own entry"
        );
    }

    #[test]
    fn different_targets_are_never_collapsed_into_each_other() {
        let mut q = Queue::new(true);
        q.push_level(fader(0), -100);
        q.push_level(fader(1), -200);
        q.push_level(
            Target::Send {
                strip: Strip::Input,
                index: 0,
                dest: 0,
            },
            -300,
        );
        assert_eq!(drain(&mut q).len(), 3);
    }

    /// A coalesced target keeps its original place, so a fade's steps stay interleaved with other
    /// traffic in arrival order instead of jumping to the back of the queue on every update.
    #[test]
    fn coalescing_preserves_the_original_position_in_the_queue() {
        let mut q = Queue::new(true);
        q.push_level(fader(0), -100);
        q.push_level(fader(1), -100);
        q.push_level(fader(0), -999); // supersedes, must not move to the back

        assert_eq!(
            drain(&mut q),
            [level(fader(0), -999), level(fader(1), -100)]
        );
    }

    #[test]
    fn coalescing_can_be_turned_off_for_debugging() {
        let mut q = Queue::new(false);
        for raw in [-100, -200, -300] {
            q.push_level(fader(0), raw);
        }
        let sent = drain(&mut q);
        assert_eq!(
            sent.len(),
            3,
            "every move should reach the console: {sent:?}"
        );
        assert_eq!(
            sent.last().unwrap(),
            &level(fader(0), -300),
            "the final value must still be the last one sent"
        );
    }

    /// Dropping a mute or a scene recall would be a show error, not a cosmetic one.
    #[test]
    fn discrete_actions_are_never_coalesced_away() {
        let mut q = Queue::new(true);
        q.push_discrete(mute(0));
        q.push_discrete(mute(0));
        q.push_discrete(Action::RecallScene {
            list: SceneList::A,
            scene: 1,
        });
        q.push_discrete(Action::RecallScene {
            list: SceneList::A,
            scene: 2,
        });
        assert_eq!(drain(&mut q).len(), 4);
    }

    #[test]
    fn discrete_actions_go_out_ahead_of_queued_level_moves() {
        let mut q = Queue::new(true);
        q.push_level(fader(0), -100);
        q.push_level(fader(1), -100);
        q.push_discrete(mute(5));

        let sent = drain(&mut q);
        assert_eq!(sent[0], mute(5), "a mute should not wait behind fade steps");
        assert_eq!(sent.len(), 3);
    }

    #[test]
    fn discrete_actions_keep_their_arrival_order_among_themselves() {
        let mut q = Queue::new(true);
        q.push_discrete(mute(1));
        q.push_discrete(Action::SetColor {
            strip: Strip::Input,
            index: 0,
            color: LabelColor::Red,
        });
        q.push_discrete(mute(2));

        let sent = drain(&mut q);
        assert_eq!(sent[0], mute(1));
        assert!(matches!(sent[1], Action::SetColor { .. }));
        assert_eq!(sent[2], mute(2));
    }

    #[test]
    fn an_empty_queue_yields_nothing() {
        let mut q = Queue::new(true);
        assert!(q.is_empty());
        assert_eq!(q.next(), None);
        q.push_level(fader(0), -100);
        assert!(!q.is_empty());
        assert!(q.next().is_some());
        assert!(q.is_empty());
    }

    #[test]
    fn clearing_drops_everything() {
        let mut q = Queue::new(true);
        q.push_level(fader(0), -100);
        q.push_discrete(mute(1));
        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.next(), None);
    }

    /// Coalescing is keyed by target, so the pending-level map is bounded by the console's
    /// addressable targets no matter how fast OSC arrives.
    #[test]
    fn the_level_queue_is_bounded_by_the_number_of_targets() {
        let mut q = Queue::new(true);
        for round in 0..1000 {
            for index in 0..8 {
                q.push_level(fader(index), -round);
            }
        }
        assert_eq!(q.levels.len(), 8);
        assert_eq!(q.level_values.len(), 8);
    }

    #[test]
    fn stats_summarise_readably() {
        let stats = Stats::default();
        Stats::bump(&stats.queued);
        Stats::bump(&stats.sent);
        assert_eq!(
            stats.summary(),
            "queued 1 · sent 1 · coalesced 0 · failed 0"
        );
    }

    #[test]
    fn a_send_target_addresses_its_destination_as_the_sub_index() {
        let target = Target::Send {
            strip: Strip::Input,
            index: 3,
            dest: 5,
        };
        assert_eq!(target.level_path(), "MIXER:Current/InCh/ToMix/Level");
        assert_eq!(target.index(), 3);
        assert_eq!(target.sub(), 5);
    }
}
