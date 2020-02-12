# local-pool-with-id
A minor variation on a [LocalPool](https://docs.rs/futures/0.3/futures/executor/struct.LocalPool.html) executor which exposes unique IDs for tracking future completion.

This should almost be a drop in replacement for the existing LocalPool. All existing traits are still implemented. There are two API differences:
1. New `(Local)SpawnWithId` traits have been implemented. These accept the same arguments as their non-ID counterparts but return a unique ID that can be used to identify whether a spawned future has been completed.
2. `try_run_one` now returns an `Option<usize>` instead of a boolean. This usize will correspond to the ID received from the previous APIs and can be used with external tracking mechanism to know if a future is complete.

## Example
```
let mut spawned_ids = std::collections::HashSet::new();
let mut pool = LocalPool::new();
let spawner = pool.spawner();

let (id1, handle1) = spawner
    .spawn_with_handle(futures::future::ready(1i32))
    .unwrap();
let (id2, handle2) = spawner
    .spawn_with_handle(futures::future::ready(2u32))
    .unwrap();

spawned_ids.insert(id1);
spawned_ids.insert(id2);

while !spawned_ids.is_empty() {
    if let Some(completed) = pool.try_run_one() {
        assert!(spawned_ids.remove(&completed))
    }
}

assert_eq!(handle1.now_or_never().unwrap(), 1);
assert_eq!(handle2.now_or_never().unwrap(), 2);
``` 