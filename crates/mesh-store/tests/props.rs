//! Property tests (spec §6): replicas converge regardless of delivery order, duplicates,
//! partial catch-ups, compaction and removal retraction (within the settle period).
//!
//! Writers are stores that only perform local writes (to produce signed ops); readers only
//! receive ops. The reference "union view" is a reader that applied every op in
//! `(origin, seq)` order. `PROPTEST_CASES` sets the number of cases (default 256).

mod common;

use common::{AcceptAll, node, store};
use mesh_hlc::ManualClock;
use mesh_proto::{NodeId, SignedOp, VersionVector};
use mesh_store::{RegEntry, SETTLE_MS, Store, SyncItem};
use proptest::prelude::*;
use proptest::sample::Index;
use std::path::Path;

const T0: u64 = 1_700_000_000_000;
/// "ab" extends "a": catches a namespace scan bleeding into another namespace.
const NSS: [&str; 2] = ["a", "ab"];
/// "kk" extends "k": catches prefix / start-after mistakes in `list_keys`.
const KEYS: [&str; 4] = ["k", "k2", "kk", "l"];
const SETS: [&str; 2] = ["m", "mm"];
/// "xy" extends "x": member rows `member ‖ origin` interleave.
const MEMBERS: [&[u8]; 3] = [b"x", b"xy", b"y"];
/// Node ids of readers start here (writers are 1..=N).
const READER_BASE: u8 = 100;
/// Script length bound. The brief allows up to 40; 16 keeps 10 000 cases near 15 minutes
/// on Windows, where every commit fsyncs (40 took 25 min). Shrunk counterexamples are
/// small anyway.
const MAX_ACTIONS: usize = 16;
/// Bound on full-exchange rounds; convergence must happen well before this.
const MAX_ROUNDS: usize = 32;

type S = Store<ManualClock>;

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256)
}

// ---------------------------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Write {
    Set,
    Delete,
    /// `set_ordered(order = [order])`, a tombstone if `tomb`.
    Ordered {
        order: u8,
        tomb: bool,
    },
    Add {
        set: usize,
        member: usize,
    },
}

#[derive(Clone, Debug)]
struct Action {
    writer: usize,
    ns: usize,
    key: usize,
    write: Write,
    /// Milliseconds the writer's clock advances before this write (0 keeps HLC ties).
    tick: u64,
}

fn write_strategy() -> impl Strategy<Value = Write> {
    prop_oneof![
        3 => Just(Write::Set),
        1 => Just(Write::Delete),
        2 => (0u8..4, any::<bool>()).prop_map(|(order, tomb)| Write::Ordered { order, tomb }),
        2 => (0..SETS.len(), 0..MEMBERS.len()).prop_map(|(set, member)| Write::Add { set, member }),
    ]
}

fn action_strategy(writers: usize) -> impl Strategy<Value = Action> {
    (
        0..writers,
        0..NSS.len(),
        0..KEYS.len(),
        write_strategy(),
        prop_oneof![3 => Just(0u64), 1 => 1u64..3],
    )
        .prop_map(|(writer, ns, key, write, tick)| Action {
            writer,
            ns,
            key,
            write,
            tick,
        })
}

/// Writer clock offsets from `T0`: all < 1 s apart, repeats give equal HLC wall times.
fn offsets_strategy(writers: usize) -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(prop::sample::select(vec![0u64, 0, 1, 2, 700]), writers)
}

/// N writers (offsets) and a script of local actions.
fn script_strategy(
    writers: std::ops::RangeInclusive<usize>,
) -> impl Strategy<Value = (Vec<u64>, Vec<Action>)> {
    writers.prop_flat_map(|n| {
        (
            offsets_strategy(n),
            prop::collection::vec(action_strategy(n), 1..=MAX_ACTIONS),
        )
    })
}

/// Runs `script` on fresh writer stores (node ids `1..=offsets.len()`, each with its own
/// clock at `T0 + offset`) and returns the ops in script order.
fn run_writers(dir: &Path, offsets: &[u64], script: &[Action]) -> Vec<SignedOp> {
    let clocks: Vec<ManualClock> = offsets.iter().map(|o| ManualClock::new(T0 + o)).collect();
    let writers: Vec<S> = clocks
        .iter()
        .enumerate()
        .map(|(i, c)| store(dir, i as u8 + 1, c))
        .collect();
    script
        .iter()
        .enumerate()
        .map(|(i, a)| {
            clocks[a.writer].advance(a.tick);
            let w = &writers[a.writer];
            let (ns, key) = (NSS[a.ns], KEYS[a.key]);
            let value = format!("w{}#{i}", a.writer + 1).into_bytes();
            match a.write {
                Write::Set => w.set(ns, key, &value),
                Write::Delete => w.delete(ns, key),
                Write::Ordered { order, tomb } => {
                    w.set_ordered(ns, key, &[order], (!tomb).then_some(value.as_slice()))
                }
                Write::Add { set, member } => w.set_add(ns, SETS[set], MEMBERS[member]),
            }
            .expect("local write")
        })
        .collect()
}

/// `ops` sorted by `(origin, seq)`.
fn in_order(ops: &[SignedOp]) -> Vec<SignedOp> {
    let mut v = ops.to_vec();
    v.sort_by_key(|o| (o.body.origin, o.body.seq));
    v
}

// ---------------------------------------------------------------------------------------
// Readers
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Step {
    /// Deliver `ops[i]` with `apply`.
    Op(usize),
    /// Pull at most `limit` items from a peer reader (`peer` picks it, see `peer_of`).
    Pull { peer: usize, limit: usize },
    /// Advance this reader's clock by `SETTLE_MS + 1`, then `compact()`.
    Compact,
    /// Apply removal number `i` of the property.
    Remove(usize),
}

struct ScheduleSpec {
    /// Deliver only a random prefix of the permutation (the rest arrives by pulls).
    subset: bool,
    pulls: std::ops::RangeInclusive<usize>,
    compacts: std::ops::RangeInclusive<usize>,
    removals: usize,
}

/// A reader's delivery sequence: a random permutation of `0..n_ops` (or a prefix of one)
/// with up to 20 % duplicates, interleaved with pulls, compactions and removals at random
/// positions.
fn schedule_strategy(n_ops: usize, spec: ScheduleSpec) -> impl Strategy<Value = Vec<Step>> {
    let perm = Just((0..n_ops).collect::<Vec<_>>()).prop_shuffle();
    let keep = any::<Index>();
    let dups = prop::collection::vec((0..n_ops, any::<Index>()), 0..=n_ops / 5);
    let pulls = prop::collection::vec((0usize..3, 1usize..8, any::<Index>()), spec.pulls);
    let compacts = prop::collection::vec(any::<Index>(), spec.compacts);
    let removals = prop::collection::vec(any::<Index>(), spec.removals);
    let subset = spec.subset;
    (perm, keep, dups, pulls, compacts, removals).prop_map(
        move |(perm, keep, dups, pulls, compacts, removals)| {
            let mut perm = perm;
            if subset {
                perm.truncate(keep.index(n_ops + 1));
            }
            let mut steps: Vec<Step> = perm.into_iter().map(Step::Op).collect();
            for (i, at) in dups {
                let at = at.index(steps.len() + 1);
                steps.insert(at, Step::Op(i));
            }
            for (peer, limit, at) in pulls {
                let at = at.index(steps.len() + 1);
                steps.insert(at, Step::Pull { peer, limit });
            }
            for at in compacts {
                let at = at.index(steps.len() + 1);
                steps.insert(at, Step::Compact);
            }
            for (i, at) in removals.into_iter().enumerate() {
                let at = at.index(steps.len() + 1);
                steps.insert(at, Step::Remove(i));
            }
            steps
        },
    )
}

struct Reader {
    store: S,
    clock: ManualClock,
}

fn readers(dir: &Path, m: usize) -> Vec<Reader> {
    (0..m)
        .map(|i| {
            let clock = ManualClock::new(T0);
            Reader {
                store: store(dir, READER_BASE + i as u8, &clock),
                clock,
            }
        })
        .collect()
}

/// The peer that reader `r` of `m` pulls from for `Step::Pull { peer }` (never `r`).
fn peer_of(r: usize, peer: usize, m: usize) -> usize {
    (r + 1 + peer % (m - 1)) % m
}

/// Applies catch-up items in order (`Op` → `apply`, `Covered` → `apply_covered`).
fn feed(to: &S, items: Vec<SyncItem>) {
    for item in items {
        match item {
            SyncItem::Op(o) => {
                to.apply(o, &AcceptAll).expect("apply");
            }
            SyncItem::Covered { origin, upto } => {
                to.apply_covered(origin, upto, &AcceptAll)
                    .expect("apply_covered");
            }
        }
    }
}

/// One catch-up of `to` from `from`, at most `limit` items. Returns how many moved.
fn pull(to: &S, from: &S, limit: usize) -> usize {
    let items = from
        .diff_since(&to.version_vector().expect("vv"), limit)
        .expect("diff_since");
    let n = items.len();
    feed(to, items);
    n
}

/// Pulls between every ordered pair until a round transfers nothing, then retries
/// `pending` everywhere. Returns false if that did not settle within `MAX_ROUNDS`.
fn full_exchange(rs: &[&S]) -> bool {
    let mut settled = false;
    for _ in 0..MAX_ROUNDS {
        let mut moved = 0;
        for (i, to) in rs.iter().enumerate() {
            for (j, from) in rs.iter().enumerate() {
                if i != j {
                    moved += pull(to, from, usize::MAX);
                }
            }
        }
        if moved == 0 {
            settled = true;
            break;
        }
    }
    for s in rs {
        s.retry_pending(&AcceptAll).expect("retry_pending");
    }
    settled
}

/// Runs the readers' schedules interleaved (step `i` of every reader, then step `i + 1`).
/// `remove(r, i)` performs removal `i` on reader `r`.
fn run_schedules(
    rs: &[Reader],
    schedules: &[Vec<Step>],
    ops: &[SignedOp],
    pull_from: &dyn Fn(usize, usize) -> usize,
    remove: &dyn Fn(&S, usize),
) {
    let longest = schedules.iter().map(Vec::len).max().unwrap_or(0);
    for i in 0..longest {
        for (r, sched) in schedules.iter().enumerate() {
            let Some(step) = sched.get(i) else { continue };
            let me = &rs[r];
            match *step {
                Step::Op(k) => {
                    me.store.apply(ops[k].clone(), &AcceptAll).expect("apply");
                }
                Step::Pull { peer, limit } => {
                    let p = pull_from(r, peer);
                    pull(&me.store, &rs[p].store, limit);
                }
                Step::Compact => {
                    me.clock.advance(SETTLE_MS + 1);
                    me.store.compact().expect("compact");
                }
                Step::Remove(k) => remove(&me.store, k),
            }
        }
    }
}

/// Everything a client can read for the generated namespaces, keys, sets and members,
/// plus the version vector and the number of held-back ops.
#[derive(Debug, PartialEq, Eq)]
struct Dump {
    entries: Vec<KeyRead>,
    keys: Vec<Vec<String>>,
    members: Vec<Vec<Vec<u8>>>,
    contains: Vec<bool>,
    vv: VersionVector,
    pending: u64,
}

/// `get_entry` and `get` of one generated key.
#[derive(Debug, PartialEq, Eq)]
struct KeyRead {
    ns: &'static str,
    key: &'static str,
    entry: Option<RegEntry>,
    value: Option<Vec<u8>>,
}

fn dump(s: &S) -> Dump {
    let mut d = Dump {
        entries: Vec::new(),
        keys: Vec::new(),
        members: Vec::new(),
        contains: Vec::new(),
        vv: s.version_vector().expect("vv"),
        pending: s.pending_len().expect("pending_len"),
    };
    for ns in NSS {
        for key in KEYS {
            d.entries.push(KeyRead {
                ns,
                key,
                entry: s.get_entry(ns, key).expect("get_entry"),
                value: s.get(ns, key).expect("get"),
            });
        }
        d.keys
            .push(s.list_keys(ns, "", None, 1000).expect("list_keys"));
        for set in SETS {
            d.members
                .push(s.set_members(ns, set, None, 1000).expect("set_members"));
            for m in MEMBERS {
                d.contains
                    .push(s.set_contains(ns, set, m).expect("set_contains"));
            }
        }
    }
    d
}

/// The reference union view: a fresh reader applying `ops` in `(origin, seq)` order, then
/// `finish` (e.g. a removal).
fn reference(dir: &Path, ops: &[SignedOp], finish: impl FnOnce(&S)) -> Dump {
    let clock = ManualClock::new(T0);
    let s = store(dir, READER_BASE - 1, &clock);
    for op in in_order(ops) {
        s.apply(op, &AcceptAll).expect("apply");
    }
    finish(&s);
    dump(&s)
}

// ---------------------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------------------

type ConvergenceCase = (Vec<u64>, Vec<Action>, Vec<Vec<Step>>);

fn convergence_strategy() -> impl Strategy<Value = ConvergenceCase> {
    (script_strategy(2..=5), 2usize..=4).prop_flat_map(|((offsets, script), m)| {
        let n = script.len();
        let one = |subset| {
            schedule_strategy(
                n,
                ScheduleSpec {
                    subset,
                    pulls: 0..=4,
                    compacts: 0..=0,
                    removals: 0,
                },
            )
        };
        // Reader 0 is delivered every op; the others only a random part of them, so the
        // rest must reach them through (partial and final) catch-ups.
        let first = one(false);
        let rest = prop::collection::vec(one(true), m - 1);
        (Just(offsets), Just(script), first, rest).prop_map(|(o, s, first, mut rest)| {
            rest.insert(0, first);
            (o, s, rest)
        })
    })
}

type CompactionCase = (Vec<u64>, Vec<Action>, Vec<Step>, Vec<Step>);

fn compaction_strategy() -> impl Strategy<Value = CompactionCase> {
    script_strategy(2..=5).prop_flat_map(|(offsets, script)| {
        let n = script.len();
        // Reader 0 compacts; reader 1 gets part of the ops directly and the rest by
        // pulling from reader 0 (receiving its `Covered` markers).
        let compacting = schedule_strategy(
            n,
            ScheduleSpec {
                subset: false,
                pulls: 0..=0,
                compacts: 1..=4,
                removals: 0,
            },
        );
        let catching_up = schedule_strategy(
            n,
            ScheduleSpec {
                subset: true,
                pulls: 0..=4,
                compacts: 0..=0,
                removals: 0,
            },
        );
        (Just(offsets), Just(script), compacting, catching_up)
    })
}

/// Script, removed origin X = writer 1, cutoff `c`, optional raised cutoff `c2 > c`, and
/// two readers' schedules (each with both removals placed). Reader 0 is delivered every
/// op, reader 1 a random part (the rest arrives by catch-up from reader 0).
type RetractionCase = (Vec<u64>, Vec<Action>, u64, Option<u64>, Vec<Vec<Step>>);

fn retraction_strategy() -> impl Strategy<Value = RetractionCase> {
    script_strategy(2..=4).prop_flat_map(|(offsets, script)| {
        let n = script.len();
        let max_x = script.iter().filter(|a| a.writer == 0).count() as u64;
        let one = |subset| {
            schedule_strategy(
                n,
                ScheduleSpec {
                    subset,
                    pulls: 0..=3,
                    compacts: 0..=0,
                    removals: 2,
                },
            )
        };
        let cutoffs =
            (0..=max_x).prop_flat_map(move |c| (Just(c), prop::option::of(c + 1..=max_x + 1)));
        (Just(offsets), Just(script), cutoffs, one(false), one(true))
            .prop_map(|(o, s, (c, c2), a, b)| (o, s, c, c2, vec![a, b]))
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// Readers given the ops (all of them, or a random part) in different orders, with
    /// duplicates and partial catch-ups from each other, end equal to the union view
    /// after a full exchange.
    #[test]
    fn convergence((offsets, script, schedules) in convergence_strategy()) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ops = run_writers(dir.path(), &offsets, &script);
        let want = reference(dir.path(), &ops, |_| {});
        let rs = readers(dir.path(), schedules.len());
        let m = rs.len();
        run_schedules(&rs, &schedules, &ops, &|r, p| peer_of(r, p, m), &|_, _| {});
        let stores: Vec<&S> = rs.iter().map(|r| &r.store).collect();
        prop_assert!(full_exchange(&stores), "full exchange did not settle");
        for (i, r) in rs.iter().enumerate() {
            prop_assert_eq!(&dump(&r.store), &want, "reader {}", i);
        }
    }

    /// Compacting at random points (clock advanced past SETTLE each time) never changes
    /// what a reader that saw every op reads, nor what a peer catching up from it reads.
    #[test]
    fn compaction_invisible((offsets, script, compacting, catching_up) in compaction_strategy()) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ops = run_writers(dir.path(), &offsets, &script);
        let want = reference(dir.path(), &ops, |_| {});
        let rs = readers(dir.path(), 2);
        // Reader 1 only ever pulls from reader 0; reader 0 never pulls.
        run_schedules(&rs, &[compacting, catching_up], &ops, &|_, _| 0, &|_, _| {});
        prop_assert_eq!(&dump(&rs[0].store), &want, "compacting reader");
        // Catch reader 1 up from reader 0 only.
        let mut settled = false;
        for _ in 0..MAX_ROUNDS {
            if pull(&rs[1].store, &rs[0].store, usize::MAX) == 0 {
                settled = true;
                break;
            }
        }
        prop_assert!(settled, "catch-up did not settle");
        rs[1].store.retry_pending(&AcceptAll).expect("retry_pending");
        prop_assert_eq!(&dump(&rs[1].store), &want, "reader caught up from compacted peer");
    }

    /// A removal of origin X with cutoff `c` (and optionally a raised cutoff `c2`) placed
    /// anywhere in each reader's delivery sequence gives the same reads as applying every
    /// op first and the effective (max) cutoff last. No clock advance: within SETTLE.
    #[test]
    fn retraction_deterministic((offsets, script, c, c2, schedules) in retraction_strategy()) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ops = run_writers(dir.path(), &offsets, &script);
        let x: NodeId = node(1);
        let effective = c2.unwrap_or(c).max(c);
        let want = reference(dir.path(), &ops, |s| {
            s.on_removal(x, effective).expect("on_removal");
        });
        let rs = readers(dir.path(), 2);
        let remove = |s: &S, i: usize| {
            let cutoff = match i {
                0 => Some(c),
                _ => c2,
            };
            if let Some(cutoff) = cutoff {
                s.on_removal(x, cutoff).expect("on_removal");
            }
        };
        run_schedules(&rs, &schedules, &ops, &|r, p| peer_of(r, p, 2), &remove);
        let stores: Vec<&S> = rs.iter().map(|r| &r.store).collect();
        prop_assert!(full_exchange(&stores), "full exchange did not settle");
        for (i, r) in rs.iter().enumerate() {
            prop_assert_eq!(&dump(&r.store), &want, "reader {}", i);
        }
    }
}
