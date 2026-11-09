//! Crash safety: a hard-killed writer leaves a consistent, reopenable store.

mod common;

use common::{node, store};
use mesh_hlc::ManualClock;
use mesh_proto::VersionVector;
use mesh_store::SyncItem;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const T0: u64 = 1_700_000_000_000;
const ROUNDS: usize = 5;
const MIN_COMMITS: u64 = 200;

/// Child: writes forever, printing each committed seq. Never returns.
fn run_child(dir: &Path) -> ! {
    let clock = ManualClock::new(T0);
    let s = store(dir, 1, &clock);
    let mut i = s.version_vector().unwrap().get(&s.node_id());
    loop {
        let op = s
            .set("s", &format!("k{i}"), &[0u8; 4096])
            .expect("child set");
        assert_eq!(op.body.seq, i + 1);
        println!("{}", op.body.seq);
        i += 1;
    }
}

#[test]
fn crash_mid_write_leaves_consistent_store() {
    if let Ok(dir) = std::env::var("MESH_CRASH_CHILD") {
        run_child(Path::new(&dir));
    }

    let dir = tempfile::tempdir().unwrap();
    let exe = std::env::current_exe().unwrap();
    let mut prev_vv = 0u64;

    for round in 1..=ROUNDS {
        let mut child = Command::new(&exe)
            .args([
                "--exact",
                "crash_mid_write_leaves_consistent_store",
                "--nocapture",
            ])
            .env("MESH_CRASH_CHILD", dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn child");
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel::<u64>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                // The harness may print its own lines; only numbers are commits.
                if let Ok(n) = line.trim().parse::<u64>()
                    && tx.send(n).is_err()
                {
                    break;
                }
            }
        });

        let deadline = Instant::now() + Duration::from_secs(60);
        let mut last = 0u64;
        while last < prev_vv + MIN_COMMITS {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(n) => last = n,
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("round {round}: child printed only up to {last} within 60 s");
                }
            }
        }
        child.kill().expect("kill child");
        child.wait().unwrap();
        drop(rx);

        // Reopen and check consistency.
        let clock = ManualClock::new(T0);
        let s = store(dir.path(), 1, &clock);
        let me = node(1);
        let vv = s.version_vector().unwrap();
        let seq = vv.get(&me);
        // Everything printed was committed; at most the in-flight commit(s) beyond it.
        assert!(seq >= last, "round {round}: vv {seq} < last printed {last}");
        for i in 0..seq {
            assert_eq!(
                s.get("s", &format!("k{i}")).unwrap().as_deref(),
                Some(&[0u8; 4096][..]),
                "round {round}: key k{i} missing"
            );
        }
        assert_eq!(s.get("s", &format!("k{seq}")).unwrap(), None);
        // Every ops row decodes, verifies, and has seq <= vv.
        let items = s.diff_since(&VersionVector::default(), usize::MAX).unwrap();
        let mut ops = 0u64;
        for item in items {
            if let SyncItem::Op(op) = item {
                assert!(op.body.seq <= vv.get(&op.body.origin));
                assert!(op.verify().is_ok(), "round {round}: bad op row");
                ops += 1;
            }
        }
        assert_eq!(ops, seq, "round {round}: op row count");
        println!("round {round}: last printed {last}, vv {seq}, ops {ops}");
        drop(s);
        prev_vv = seq;
    }
}
