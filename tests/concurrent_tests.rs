use anyhow::Result;
use dotman::storage::FileEntry;
use dotman::storage::concurrent_index::ConcurrentIndex;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

#[test]
#[allow(clippy::unnecessary_wraps)]
fn test_concurrent_writes_no_data_loss() -> Result<()> {
    let index = Arc::new(ConcurrentIndex::new());
    let barrier = Arc::new(Barrier::new(10));
    let mut handles = vec![];

    for thread_id in 0u64..10 {
        let index_clone = Arc::clone(&index);
        let barrier_clone = Arc::clone(&barrier);

        let handle = thread::spawn(move || {
            // Synchronize start to maximize contention
            barrier_clone.wait();

            for i in 0u64..1000 {
                let entry = FileEntry {
                    path: PathBuf::from(format!("thread_{thread_id}_file_{i}.txt")),
                    hash: format!("hash_{thread_id}_{i}"),
                    size: thread_id * 1000 + i,
                    mode: 0o644,
                    modified: chrono::Utc::now().timestamp(),
                    cached_hash: None,
                };
                index_clone.stage_entry(entry);

                // Occasionally read to create contention
                if i % 100 == 0 {
                    let _ = index_clone.staged_entries();
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    // Verify all entries are present
    let entries = index.staged_entries();
    assert_eq!(entries.len(), 10000);

    // Verify no corruption
    for (path, entry) in entries {
        assert!(path.to_str().unwrap().contains("thread_"));
        assert!(entry.hash.starts_with("hash_"));
    }

    Ok(())
}

#[test]
#[allow(clippy::unnecessary_wraps)]
fn test_concurrent_read_write_consistency() -> Result<()> {
    let index = Arc::new(ConcurrentIndex::new());
    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = vec![];

    // Writer thread
    let index_writer = Arc::clone(&index);
    let stop_writer = Arc::clone(&stop);
    let writer = thread::spawn(move || {
        let mut counter: u64 = 0;
        while !stop_writer.load(Ordering::Relaxed) {
            let entry = FileEntry {
                path: PathBuf::from(format!("file_{counter}.txt")),
                hash: format!("hash_{counter}"),
                size: counter,
                mode: 0o644,
                modified: chrono::Utc::now().timestamp(),
                cached_hash: None,
            };
            index_writer.stage_entry(entry);
            counter += 1;
            thread::sleep(Duration::from_micros(10));
        }
        counter
    });
    handles.push(writer);

    // Multiple reader threads
    for _reader_id in 0..5 {
        let index_reader = Arc::clone(&index);
        let stop_reader = Arc::clone(&stop);
        let reader = thread::spawn(move || {
            let mut read_count = 0;
            while !stop_reader.load(Ordering::Relaxed) {
                let entries = index_reader.staged_entries();
                // Verify consistency - all entries should be valid
                for (path, entry) in entries {
                    assert!(path.to_str().unwrap().starts_with("file_"));
                    assert!(entry.hash.starts_with("hash_"));
                }
                read_count += 1;
                thread::sleep(Duration::from_micros(50));
            }
            read_count
        });
        handles.push(reader);
    }

    // Let it run for a bit
    thread::sleep(Duration::from_millis(100));
    stop.store(true, Ordering::Relaxed);

    for handle in handles {
        handle.join().unwrap();
    }

    Ok(())
}

#[test]
#[allow(clippy::unnecessary_wraps)]
fn test_concurrent_stage_commit_cycle() -> Result<()> {
    let index = Arc::new(ConcurrentIndex::new());
    let counter = Arc::new(AtomicU64::new(0));
    let mut handles = vec![];

    // Multiple threads doing stage/commit cycles
    for thread_id in 0..5 {
        let index_clone = Arc::clone(&index);
        let counter_clone = Arc::clone(&counter);

        let handle = thread::spawn(move || {
            for cycle in 0..20 {
                // Stage some entries
                for i in 0..10 {
                    let id = counter_clone.fetch_add(1, Ordering::SeqCst);
                    let entry = FileEntry {
                        path: PathBuf::from(format!("t{thread_id}_c{cycle}_f{i}.txt")),
                        hash: format!("hash_{id}"),
                        size: id,
                        mode: 0o644,
                        modified: chrono::Utc::now().timestamp(),
                        cached_hash: None,
                    };
                    index_clone.stage_entry(entry);
                }

                // Commit if this thread wins the race
                if cycle % 3 == thread_id % 3 {
                    index_clone.commit_staged();
                }

                thread::yield_now();
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    // Final commit to ensure all staged are committed
    index.commit_staged();

    // Verify final state
    let total = index.staged_entries().len();
    assert_eq!(total, 0); // All should be committed, so staged should be empty
    assert!(!index.has_staged_changes());

    Ok(())
}

#[test]
#[allow(clippy::unnecessary_wraps)]
fn test_concurrent_modifications() -> Result<()> {
    let index = Arc::new(ConcurrentIndex::new());
    let mut handles = vec![];

    // Pre-populate with some entries
    for i in 0..100 {
        let entry = FileEntry {
            path: PathBuf::from(format!("file_{i}.txt")),
            hash: format!("initial_hash_{i}"),
            size: i,
            mode: 0o644,
            modified: chrono::Utc::now().timestamp(),
            cached_hash: None,
        };
        index.stage_entry(entry);
    }
    index.commit_staged();

    let barrier = Arc::new(Barrier::new(5));

    // Multiple threads modifying the same files
    for thread_id in 0u64..5 {
        let index_clone = Arc::clone(&index);
        let barrier_clone = Arc::clone(&barrier);

        let handle = thread::spawn(move || {
            barrier_clone.wait();

            for i in 0u64..100 {
                let entry = FileEntry {
                    path: PathBuf::from(format!("file_{i}.txt")),
                    hash: format!("thread_{thread_id}_hash_{i}"),
                    size: thread_id * 100 + i,
                    mode: 0o644,
                    modified: chrono::Utc::now().timestamp(),
                    cached_hash: None,
                };
                index_clone.stage_entry(entry);

                // Occasionally mark entries for deletion
                if i % 20 == 0 {
                    let file_num = i + 50;
                    index_clone.mark_deleted(&PathBuf::from(format!("file_{file_num}.txt")));
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    // Verify consistency - last writer wins
    let staged = index.staged_entries();
    for (_path, entry) in staged {
        // All hashes should be from one of the threads
        assert!(entry.hash.contains("thread_") || entry.hash.contains("initial_"));
    }

    Ok(())
}

#[test]
#[allow(clippy::unnecessary_wraps)]
fn test_concurrent_clear_operations() -> Result<()> {
    let index = Arc::new(ConcurrentIndex::new());
    let mut handles = vec![];
    let barrier = Arc::new(Barrier::new(6));

    // Writer threads
    for thread_id in 0..3 {
        let index_clone = Arc::clone(&index);
        let barrier_clone = Arc::clone(&barrier);

        let handle = thread::spawn(move || {
            barrier_clone.wait();

            for i in 0..100 {
                let entry = FileEntry {
                    path: PathBuf::from(format!("writer_{thread_id}_file_{i}.txt")),
                    hash: format!("hash_{thread_id}_{i}"),
                    size: i,
                    mode: 0o644,
                    modified: chrono::Utc::now().timestamp(),
                    cached_hash: None,
                };
                index_clone.stage_entry(entry);
            }
        });
        handles.push(handle);
    }

    // Clearer threads
    for _clearer_id in 0..3 {
        let index_clone = Arc::clone(&index);
        let barrier_clone = Arc::clone(&barrier);

        let handle = thread::spawn(move || {
            barrier_clone.wait();
            thread::sleep(Duration::from_micros(50));

            for _ in 0..5 {
                index_clone.clear_staged();
                thread::sleep(Duration::from_micros(100));
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    // Final state should be consistent
    // Either empty or containing valid entries
    let staged = index.staged_entries();
    for (path, entry) in staged {
        assert!(path.to_str().unwrap().contains("writer_"));
        assert!(entry.hash.starts_with("hash_"));
    }

    Ok(())
}

#[test]
#[allow(clippy::unnecessary_wraps)]
fn test_concurrent_bulk_operations() -> Result<()> {
    let index = Arc::new(ConcurrentIndex::new());
    let barrier = Arc::new(Barrier::new(4));
    let mut handles = vec![];

    for thread_id in 0..4 {
        let index_clone = Arc::clone(&index);
        let barrier_clone = Arc::clone(&barrier);

        let handle = thread::spawn(move || {
            barrier_clone.wait();

            // Each thread does different bulk operations
            match thread_id {
                0 => {
                    // Bulk add
                    for i in 0..500 {
                        let entry = FileEntry {
                            path: PathBuf::from(format!("bulk_add_{i}.txt")),
                            hash: format!("hash_{i}"),
                            size: i,
                            mode: 0o644,
                            modified: chrono::Utc::now().timestamp(),
                            cached_hash: None,
                        };
                        index_clone.stage_entry(entry);
                    }
                }
                1 => {
                    // Periodic commits
                    for _ in 0..10 {
                        thread::sleep(Duration::from_millis(10));
                        index_clone.commit_staged();
                    }
                }
                2 => {
                    // Bulk reads
                    for _ in 0..100 {
                        let _ = index_clone.staged_entries().len();
                        let _ = index_clone.staged_entries();
                        thread::yield_now();
                    }
                }
                3 => {
                    // Mixed operations
                    for i in 0..200 {
                        if i % 2 == 0 {
                            let entry = FileEntry {
                                path: PathBuf::from(format!("mixed_{i}.txt")),
                                hash: format!("mixed_hash_{i}"),
                                size: i,
                                mode: 0o644,
                                modified: chrono::Utc::now().timestamp(),
                                cached_hash: None,
                            };
                            index_clone.stage_entry(entry);
                        } else {
                            index_clone.mark_deleted(&PathBuf::from(format!("bulk_add_{i}.txt")));
                        }
                    }
                }
                _ => {}
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    // Final state should be consistent
    index.commit_staged();
    let total = index.staged_entries().len();
    assert_eq!(total, 0); // All should be committed

    // Verify no internal corruption in staged entries
    let all_entries = index.staged_entries();
    for (_path, entry) in all_entries {
        assert!(!entry.hash.is_empty(), "staged entry has an empty hash");
        assert!(entry.mode > 0);
    }

    Ok(())
}
