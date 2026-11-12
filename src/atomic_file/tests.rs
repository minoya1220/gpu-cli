use super::*;
use std::sync::{Arc, Barrier};
#[test]
fn concurrent_updates_share_a_stable_lock_without_extra_files() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("counter");
    let gate = Arc::new(Barrier::new(8));
    let workers = (0..8)
        .map(|_| {
            let path = path.clone();
            let gate = gate.clone();
            std::thread::spawn(move || {
                gate.wait();
                for _ in 0..8 {
                    update(&path, |bytes| {
                        let number = std::str::from_utf8(bytes)
                            .unwrap()
                            .parse::<u64>()
                            .unwrap_or(0);
                        Ok((number + 1).to_string().into_bytes())
                    })
                    .unwrap();
                }
            })
        })
        .collect::<Vec<_>>();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "64");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}
