//! Android hooks: how periodic work is driven, the power policy and its wake locks
//! (spec §5.2, §5.3). The policy decision is the pure [`wake_lock_for`]; [`Power`] holds
//! the locks it decides on through the host's [`WakeLockSink`].

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use mesh_gateway::Activity;
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

/// How periodic work is scheduled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Periodic {
    /// The host runs its own timers.
    #[default]
    Timers,
    /// The embedding platform also drives periodic work: catch-up runs on
    /// [`Daemon::wake`](crate::Daemon::wake) and on incoming traffic after
    /// [`CATCH_UP_AFTER`] of silence. The node's own timers keep running.
    Driven,
}

/// Silence after which incoming traffic triggers a catch-up (`Periodic::Driven`).
pub const CATCH_UP_AFTER: Duration = Duration::from_secs(60);

/// Holds and releases the platform's wake locks, as the host tells it. One lock per tag;
/// `acquire` on a tag already held refreshes its timeout; `release` releases it whatever
/// the number of `acquire`s. `max_ms == u64::MAX` means no timeout. Both are called on
/// async tasks, so they must not block.
pub trait WakeLockSink: Send + Sync {
    fn acquire(&self, tag: &str, max_ms: u64);
    fn release(&self, tag: &str);
}

/// `meshd.toml` `[android] power`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerPolicy {
    #[default]
    Balanced,
    AlwaysOn,
    Saver,
}

/// The power state the platform reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerState {
    pub policy: PowerPolicy,
    pub charging: bool,
    pub screen_on: bool,
}

/// What a wake lock would be taken for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockActivity {
    Idle,
    /// Serving an incoming request.
    Request,
    /// A catch-up ([`Daemon::wake`](crate::Daemon::wake)).
    CatchUp,
    /// The 15-minute wall-clock alarm.
    Alarm {
        periodic_wake_s: u32,
    },
}

/// Why the platform calls [`Daemon::wake`](crate::Daemon::wake).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeReason {
    Alarm,
    Network,
    Boot,
    Incoming,
}

pub(crate) const TAG_ALWAYS: &str = "always";
pub(crate) const TAG_CHARGING: &str = "charging";
pub(crate) const TAG_REQUEST: &str = "request";
pub(crate) const TAG_CATCHUP: &str = "catchup";
pub(crate) const TAG_ALARM: &str = "alarm";

/// How long the request lock is kept after the last request ends.
pub(crate) const REQUEST_LINGER: Duration = Duration::from_secs(30);
/// The longest a service request may run (`mesh_proto` `MAX_TIMEOUT_MS`): the request
/// lock's timeout is this plus [`REQUEST_LINGER`].
pub(crate) const REQUEST_TIMEOUT_MS: u64 = 300_000;
const CATCHUP_MAX_MS: u64 = 60_000;
const SAVER_ALARM_MS: u64 = 10_000;

/// The wake lock (tag, timeout in ms) to hold for `activity` in `state`, if any.
pub fn wake_lock_for(
    state: &PowerState,
    activity: LockActivity,
    request_timeout_ms: u64,
) -> Option<(&'static str, u64)> {
    match state.policy {
        PowerPolicy::AlwaysOn => Some((TAG_ALWAYS, u64::MAX)),
        PowerPolicy::Balanced if state.charging => Some((TAG_CHARGING, u64::MAX)),
        PowerPolicy::Balanced => match activity {
            LockActivity::Request => Some((
                TAG_REQUEST,
                request_timeout_ms.saturating_add(REQUEST_LINGER.as_millis() as u64),
            )),
            LockActivity::CatchUp => Some((TAG_CATCHUP, CATCHUP_MAX_MS)),
            LockActivity::Alarm { periodic_wake_s } if periodic_wake_s > 0 => {
                Some((TAG_ALARM, u64::from(periodic_wake_s) * 1000))
            }
            LockActivity::Alarm { .. } | LockActivity::Idle => None,
        },
        PowerPolicy::Saver => match activity {
            LockActivity::Alarm { .. } => Some((TAG_ALARM, SAVER_ALARM_MS)),
            _ => None,
        },
    }
}

/// Whether inbound `httpfwd` and the incoming-traffic catch-up are paused.
pub fn saver_paused(state: &PowerState) -> bool {
    state.policy == PowerPolicy::Saver && !state.screen_on
}

/// The lock held for as long as the host runs, whatever it does: `"always"` or
/// `"charging"`.
fn standing_lock(state: &PowerState) -> Option<(&'static str, u64)> {
    wake_lock_for(state, LockActivity::Idle, 0)
        .filter(|(tag, _)| *tag == TAG_ALWAYS || *tag == TAG_CHARGING)
}

/// The power state and the wake locks held through the sink. Without a sink it only
/// keeps the state.
pub(crate) struct Power {
    sink: Option<Arc<dyn WakeLockSink>>,
    rt: Handle,
    st: Mutex<PowerSt>,
}

struct PowerSt {
    state: PowerState,
    /// `"always"` / `"charging"`, while held.
    standing: Option<&'static str>,
    /// Inbound streams being served.
    in_flight: u32,
    request_held: bool,
    /// Bumped whenever a pending request release is scheduled or cancelled.
    linger_gen: u64,
    linger: Option<JoinHandle<()>>,
    alarm_gen: u64,
    alarm: Option<JoinHandle<()>>,
    alarm_held: bool,
    /// After [`Power::close`] nothing is acquired any more.
    closed: bool,
}

/// Releases a lock when dropped.
pub(crate) struct LockGuard {
    sink: Arc<dyn WakeLockSink>,
    tag: &'static str,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        self.sink.release(self.tag);
    }
}

impl Power {
    pub(crate) fn new(sink: Option<Arc<dyn WakeLockSink>>, rt: Handle, state: PowerState) -> Self {
        Power {
            sink,
            rt,
            st: Mutex::new(PowerSt {
                state,
                standing: None,
                in_flight: 0,
                request_held: false,
                linger_gen: 0,
                linger: None,
                alarm_gen: 0,
                alarm: None,
                alarm_held: false,
                closed: false,
            }),
        }
    }

    fn st(&self) -> MutexGuard<'_, PowerSt> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn has_sink(&self) -> bool {
        self.sink.is_some()
    }

    pub(crate) fn state(&self) -> PowerState {
        self.st().state
    }

    /// Records `state` and takes or lets go of the standing lock to match it.
    pub(crate) fn set_state(&self, state: PowerState) {
        let mut st = self.st();
        st.state = state;
        self.reconcile(&mut st);
    }

    /// Takes the standing lock the current state calls for (after start).
    pub(crate) fn apply_standing(&self) {
        let mut st = self.st();
        self.reconcile(&mut st);
    }

    fn reconcile(&self, st: &mut PowerSt) {
        let Some(sink) = &self.sink else { return };
        if st.closed {
            return;
        }
        let want = standing_lock(&st.state);
        if want.map(|(t, _)| t) == st.standing {
            return;
        }
        if let Some(old) = st.standing.take() {
            sink.release(old);
        }
        if let Some((tag, max)) = want {
            sink.acquire(tag, max);
            st.standing = Some(tag);
        }
    }

    /// The `"catchup"` lock for the duration of a wake, if the policy calls for one.
    pub(crate) fn catch_up_lock(&self) -> Option<LockGuard> {
        let sink = self.sink.as_ref()?;
        let st = self.st();
        if st.closed {
            return None;
        }
        match wake_lock_for(&st.state, LockActivity::CatchUp, REQUEST_TIMEOUT_MS) {
            Some((tag, max)) if tag == TAG_CATCHUP => {
                sink.acquire(tag, max);
                Some(LockGuard {
                    sink: sink.clone(),
                    tag,
                })
            }
            _ => None,
        }
    }

    /// The `"alarm"` lock, held for its full timeout from now (a later alarm restarts it).
    pub(crate) fn alarm(self: &Arc<Self>, periodic_wake_s: u32) {
        let Some(sink) = &self.sink else { return };
        let mut st = self.st();
        if st.closed {
            return;
        }
        let activity = LockActivity::Alarm { periodic_wake_s };
        let Some((tag, max)) =
            wake_lock_for(&st.state, activity, REQUEST_TIMEOUT_MS).filter(|(t, _)| *t == TAG_ALARM)
        else {
            return;
        };
        sink.acquire(tag, max);
        st.alarm_held = true;
        st.alarm_gen += 1;
        if let Some(t) = st.alarm.take() {
            t.abort();
        }
        let generation = st.alarm_gen;
        let me = Arc::downgrade(self);
        st.alarm = Some(self.rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(max)).await;
            if let Some(p) = me.upgrade() {
                let mut st = p.st();
                if st.alarm_gen == generation && st.alarm_held {
                    st.alarm_held = false;
                    st.alarm = None;
                    if let Some(sink) = &p.sink {
                        sink.release(TAG_ALARM);
                    }
                }
            }
        }));
    }

    /// An inbound stream started or ended (the `httpfwd` slot's activity callback).
    pub(crate) fn activity(self: &Arc<Self>, a: Activity) {
        let Some(sink) = &self.sink else { return };
        let mut st = self.st();
        match a {
            Activity::Start => {
                st.in_flight += 1;
                if st.in_flight != 1 || st.closed {
                    return;
                }
                // A release pending from an earlier request no longer applies.
                st.linger_gen += 1;
                if let Some(t) = st.linger.take() {
                    t.abort();
                }
                let lock = wake_lock_for(&st.state, LockActivity::Request, REQUEST_TIMEOUT_MS);
                if let Some((tag, max)) = lock
                    && tag == TAG_REQUEST
                {
                    sink.acquire(tag, max);
                    st.request_held = true;
                }
            }
            Activity::End => {
                st.in_flight = st.in_flight.saturating_sub(1);
                if st.in_flight != 0 || !st.request_held || st.closed {
                    return;
                }
                st.linger_gen += 1;
                let generation = st.linger_gen;
                let me = Arc::downgrade(self);
                st.linger = Some(self.rt.spawn(async move {
                    tokio::time::sleep(REQUEST_LINGER).await;
                    if let Some(p) = me.upgrade() {
                        let mut st = p.st();
                        if st.linger_gen == generation && st.in_flight == 0 && st.request_held {
                            st.request_held = false;
                            st.linger = None;
                            if let Some(sink) = &p.sink {
                                sink.release(TAG_REQUEST);
                            }
                        }
                    }
                }));
            }
        }
    }

    /// Inbound streams being served.
    #[cfg(feature = "test-hooks")]
    pub(crate) fn in_flight(&self) -> u32 {
        self.st().in_flight
    }

    /// Cancels pending releases and releases every lock held. Nothing is acquired after.
    /// Idempotent.
    pub(crate) fn close(&self) {
        let mut st = self.st();
        if st.closed {
            return;
        }
        st.closed = true;
        st.linger_gen += 1;
        st.alarm_gen += 1;
        for t in [st.linger.take(), st.alarm.take()].into_iter().flatten() {
            t.abort();
        }
        let Some(sink) = &self.sink else { return };
        if std::mem::take(&mut st.request_held) {
            sink.release(TAG_REQUEST);
        }
        if std::mem::take(&mut st.alarm_held) {
            sink.release(TAG_ALARM);
        }
        if let Some(tag) = st.standing.take() {
            sink.release(tag);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: [PowerPolicy; 3] = [
        PowerPolicy::Balanced,
        PowerPolicy::AlwaysOn,
        PowerPolicy::Saver,
    ];
    const ACTIVITIES: [LockActivity; 5] = [
        LockActivity::Idle,
        LockActivity::Request,
        LockActivity::CatchUp,
        LockActivity::Alarm { periodic_wake_s: 0 },
        LockActivity::Alarm {
            periodic_wake_s: 45,
        },
    ];

    /// The table of spec §5.3, written out row by row.
    fn expected(s: &PowerState, a: LockActivity) -> Option<(&'static str, u64)> {
        use LockActivity as A;
        use PowerPolicy as Pol;
        match (s.policy, s.charging, a) {
            (Pol::AlwaysOn, _, _) => Some(("always", u64::MAX)),
            (Pol::Balanced, true, _) => Some(("charging", u64::MAX)),
            (Pol::Balanced, false, A::Request) => Some(("request", 7_000 + 30_000)),
            (Pol::Balanced, false, A::CatchUp) => Some(("catchup", 60_000)),
            (
                Pol::Balanced,
                false,
                A::Alarm {
                    periodic_wake_s: 45,
                },
            ) => Some(("alarm", 45_000)),
            (Pol::Balanced, false, A::Alarm { periodic_wake_s: 0 }) => None,
            (Pol::Balanced, false, A::Idle) => None,
            (Pol::Saver, _, A::Alarm { .. }) => Some(("alarm", 10_000)),
            (Pol::Saver, _, _) => None,
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn power_decision_table() {
        let mut rows = 0;
        for policy in P {
            for charging in [false, true] {
                for screen_on in [false, true] {
                    let s = PowerState {
                        policy,
                        charging,
                        screen_on,
                    };
                    for a in ACTIVITIES {
                        assert_eq!(wake_lock_for(&s, a, 7_000), expected(&s, a), "{s:?} {a:?}");
                        rows += 1;
                    }
                }
            }
        }
        assert_eq!(rows, 3 * 2 * 2 * 5);
        // A huge request timeout saturates rather than overflowing.
        let s = PowerState {
            policy: PowerPolicy::Balanced,
            charging: false,
            screen_on: true,
        };
        assert_eq!(
            wake_lock_for(&s, LockActivity::Request, u64::MAX),
            Some(("request", u64::MAX))
        );
    }

    #[test]
    fn saver_paused_only_screen_off() {
        for policy in P {
            for charging in [false, true] {
                for screen_on in [false, true] {
                    let s = PowerState {
                        policy,
                        charging,
                        screen_on,
                    };
                    let want = policy == PowerPolicy::Saver && !screen_on;
                    assert_eq!(saver_paused(&s), want, "{s:?}");
                }
            }
        }
    }

    #[derive(Default)]
    struct Rec(Mutex<Vec<String>>);

    impl WakeLockSink for Rec {
        fn acquire(&self, tag: &str, max_ms: u64) {
            self.0.lock().unwrap().push(format!("+{tag}:{max_ms}"));
        }
        fn release(&self, tag: &str) {
            self.0.lock().unwrap().push(format!("-{tag}"));
        }
    }

    impl Rec {
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut self.0.lock().unwrap())
        }
    }

    fn balanced() -> PowerState {
        PowerState {
            policy: PowerPolicy::Balanced,
            charging: false,
            screen_on: true,
        }
    }

    fn power(state: PowerState) -> (Arc<Power>, Arc<Rec>) {
        let rec = Arc::new(Rec::default());
        let sink: Arc<dyn WakeLockSink> = rec.clone();
        (
            Arc::new(Power::new(Some(sink), Handle::current(), state)),
            rec,
        )
    }

    /// Lets spawned tasks run at the current (paused) time.
    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn request_lock_lingers_and_is_refreshed() {
        let (p, rec) = power(balanced());
        p.activity(Activity::Start);
        p.activity(Activity::Start);
        assert_eq!(rec.take(), ["+request:330000"]);
        p.activity(Activity::End);
        p.activity(Activity::End);
        tokio::time::sleep(Duration::from_secs(20)).await;
        settle().await;
        assert!(rec.take().is_empty(), "released before the linger");
        // A new request within the linger cancels the pending release.
        p.activity(Activity::Start);
        assert_eq!(rec.take(), ["+request:330000"]);
        p.activity(Activity::End);
        tokio::time::sleep(Duration::from_secs(29)).await;
        settle().await;
        assert!(rec.take().is_empty(), "the first linger released it");
        tokio::time::sleep(Duration::from_secs(2)).await;
        settle().await;
        assert_eq!(rec.take(), ["-request"]);
    }

    #[tokio::test(start_paused = true)]
    async fn close_cancels_the_linger_and_releases_everything() {
        let (p, rec) = power(balanced());
        p.activity(Activity::Start);
        p.activity(Activity::End);
        p.set_state(PowerState {
            charging: true,
            ..balanced()
        });
        p.alarm(0); // charging: covered by the standing lock
        assert_eq!(
            rec.take(),
            ["+request:330000", "+charging:18446744073709551615"]
        );
        p.close();
        let mut got = rec.take();
        got.sort();
        assert_eq!(got, ["-charging", "-request"]);
        tokio::time::sleep(Duration::from_secs(60)).await;
        settle().await;
        p.activity(Activity::Start);
        p.set_state(PowerState {
            policy: PowerPolicy::AlwaysOn,
            ..balanced()
        });
        assert!(p.catch_up_lock().is_none());
        assert!(rec.take().is_empty(), "nothing after close");
        p.close();
    }

    #[tokio::test(start_paused = true)]
    async fn standing_and_timed_locks() {
        let (p, rec) = power(balanced());
        p.apply_standing();
        assert!(rec.take().is_empty());
        p.set_state(PowerState {
            policy: PowerPolicy::AlwaysOn,
            ..balanced()
        });
        p.set_state(PowerState {
            policy: PowerPolicy::AlwaysOn,
            screen_on: false,
            ..balanced()
        });
        p.activity(Activity::Start); // "always" covers it
        p.activity(Activity::End);
        assert!(p.catch_up_lock().is_none());
        p.set_state(PowerState {
            charging: true,
            ..balanced()
        });
        p.set_state(balanced());
        assert_eq!(
            rec.take(),
            [
                "+always:18446744073709551615",
                "-always",
                "+charging:18446744073709551615",
                "-charging"
            ]
        );
        drop(p.catch_up_lock());
        assert_eq!(rec.take(), ["+catchup:60000", "-catchup"]);
        p.alarm(0);
        assert!(rec.take().is_empty());
        p.alarm(5);
        tokio::time::sleep(Duration::from_secs(4)).await;
        settle().await;
        assert_eq!(rec.take(), ["+alarm:5000"]);
        tokio::time::sleep(Duration::from_secs(2)).await;
        settle().await;
        assert_eq!(rec.take(), ["-alarm"]);
        p.close();
        assert!(rec.take().is_empty());
    }
}
