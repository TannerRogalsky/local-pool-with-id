use futures::executor::enter;
use futures::future::{FutureObj, LocalFutureObj, RemoteHandle};
use futures::prelude::*;
use futures::stream::FuturesUnordered;
use futures::task::{waker_ref, ArcWake, LocalSpawn, Spawn, SpawnError};
use std::cell::RefCell;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::thread;
use std::thread::Thread;

#[must_use = "futures do nothing unless you `.await` or poll them"]
#[derive(Debug)]
struct IndexWrapper<T> {
    data: T, // A future or a future's output
    index: usize,
}

impl<T> IndexWrapper<T> {
    pin_utils::unsafe_pinned!(data: T);
}

impl<T> Future for IndexWrapper<T>
where
    T: Future,
{
    type Output = IndexWrapper<T::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.as_mut()
            .data()
            .as_mut()
            .poll(cx)
            .map(|output| IndexWrapper {
                data: output,
                index: self.index,
            })
    }
}

/// A single-threaded task pool for polling futures to completion.
///
/// This executor allows you to multiplex any number of tasks onto a single
/// thread. It's appropriate to poll strictly I/O-bound futures that do very
/// little work in between I/O actions.
///
/// To get a handle to the pool that implements
/// [`Spawn`](futures_task::Spawn), use the
/// [`spawner()`](LocalPool::spawner) method. Because the executor is
/// single-threaded, it supports a special form of task spawning for non-`Send`
/// futures, via [`spawn_local_obj`](futures_task::LocalSpawn::spawn_local_obj).
#[derive(Debug)]
pub struct LocalPool {
    pool: FuturesUnordered<IndexWrapper<LocalFutureObj<'static, ()>>>,
    incoming: Rc<Incoming>,
}

/// A handle to a [`LocalPool`](LocalPool) that implements
/// [`Spawn`](futures_task::Spawn).
#[derive(Clone, Debug)]
pub struct LocalSpawner {
    incoming: Weak<Incoming>,
}

#[derive(Debug, Default)]
struct IncomingTracking {
    queue: Vec<(usize, LocalFutureObj<'static, ()>)>,
    index: usize,
}

type Incoming = RefCell<IncomingTracking>;

pub(crate) struct ThreadNotify {
    /// The (single) executor thread.
    thread: Thread,
    /// A flag to ensure a wakeup (i.e. `unpark()`) is not "forgotten"
    /// before the next `park()`, which may otherwise happen if the code
    /// being executed as part of the future(s) being polled makes use of
    /// park / unpark calls of its own, i.e. we cannot assume that no other
    /// code uses park / unpark on the executing `thread`.
    unparked: AtomicBool,
}

thread_local! {
    static CURRENT_THREAD_NOTIFY: Arc<ThreadNotify> = Arc::new(ThreadNotify {
        thread: thread::current(),
        unparked: AtomicBool::new(false),
    });
}

impl ArcWake for ThreadNotify {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        // Make sure the wakeup is remembered until the next `park()`.
        let unparked = arc_self.unparked.swap(true, Ordering::Relaxed);
        if !unparked {
            // If the thread has not been unparked yet, it must be done
            // now. If it was actually parked, it will run again,
            // otherwise the token made available by `unpark`
            // may be consumed before reaching `park()`, but `unparked`
            // ensures it is not forgotten.
            arc_self.thread.unpark();
        }
    }
}

// Set up and run a basic single-threaded spawner loop, invoking `f` on each
// turn.
fn run_executor<T, F: FnMut(&mut Context<'_>) -> Poll<T>>(mut f: F) -> T {
    let _enter = enter().expect(
        "cannot execute `LocalPool` executor from within \
         another executor",
    );

    CURRENT_THREAD_NOTIFY.with(|thread_notify| {
        let waker = waker_ref(thread_notify);
        let mut cx = Context::from_waker(&waker);
        loop {
            if let Poll::Ready(t) = f(&mut cx) {
                return t;
            }
            // Consume the wakeup that occurred while executing `f`, if any.
            let unparked = thread_notify.unparked.swap(false, Ordering::Acquire);
            if !unparked {
                // No wakeup occurred. It may occur now, right before parking,
                // but in that case the token made available by `unpark()`
                // is guaranteed to still be available and `park()` is a no-op.
                thread::park();
                // When the thread is unparked, `unparked` will have been set
                // and needs to be unset before the next call to `f` to avoid
                // a redundant loop iteration.
                thread_notify.unparked.store(false, Ordering::Release);
            }
        }
    })
}

fn poll_executor<T, F: FnMut(&mut Context<'_>) -> T>(mut f: F) -> T {
    let _enter = enter().expect(
        "cannot execute `LocalPool` executor from within \
         another executor",
    );

    CURRENT_THREAD_NOTIFY.with(|thread_notify| {
        let waker = waker_ref(thread_notify);
        let mut cx = Context::from_waker(&waker);
        f(&mut cx)
    })
}

impl LocalPool {
    /// Create a new, empty pool of tasks.
    pub fn new() -> LocalPool {
        LocalPool {
            pool: FuturesUnordered::new(),
            incoming: Default::default(),
        }
    }

    /// Get a clonable handle to the pool as a [`Spawn`].
    pub fn spawner(&self) -> LocalSpawner {
        LocalSpawner {
            incoming: Rc::downgrade(&self.incoming),
        }
    }

    /// Run all tasks in the pool to completion.
    ///
    /// ```
    /// use futures::executor::LocalPool;
    ///
    /// let mut pool = LocalPool::new();
    ///
    /// // ... spawn some initial tasks using `spawn.spawn()` or `spawn.spawn_local()`
    ///
    /// // run *all* tasks in the pool to completion, including any newly-spawned ones.
    /// pool.run();
    /// ```
    ///
    /// The function will block the calling thread until *all* tasks in the pool
    /// are complete, including any spawned while running existing tasks.
    pub fn run(&mut self) {
        run_executor(|cx| self.poll_pool(cx))
    }

    /// Runs all the tasks in the pool until the given future completes.
    ///
    /// ```
    /// use futures::executor::LocalPool;
    ///
    /// let mut pool = LocalPool::new();
    /// # let my_app  = async {};
    ///
    /// // run tasks in the pool until `my_app` completes
    /// pool.run_until(my_app);
    /// ```
    ///
    /// The function will block the calling thread *only* until the future `f`
    /// completes; there may still be incomplete tasks in the pool, which will
    /// be inert after the call completes, but can continue with further use of
    /// one of the pool's run or poll methods. While the function is running,
    /// however, all tasks in the pool will try to make progress.
    pub fn run_until<F: Future>(&mut self, future: F) -> F::Output {
        pin_utils::pin_mut!(future);

        run_executor(|cx| {
            {
                // if our main task is done, so are we
                let result = future.as_mut().poll(cx);
                if let Poll::Ready(output) = result {
                    return Poll::Ready(output);
                }
            }

            let _ = self.poll_pool(cx);
            Poll::Pending
        })
    }

    /// Runs all tasks and returns after completing one future or until no more progress
    /// can be made. Returns `true` if one future was completed, `false` otherwise.
    ///
    /// ```
    /// use futures::executor::LocalPool;
    /// use futures::task::LocalSpawnExt;
    /// use futures::future::{ready, pending};
    ///
    /// let mut pool = LocalPool::new();
    /// let spawner = pool.spawner();
    ///
    /// spawner.spawn_local(ready(())).unwrap();
    /// spawner.spawn_local(ready(())).unwrap();
    /// spawner.spawn_local(pending()).unwrap();
    ///
    /// // Run the two ready tasks and return true for them.
    /// pool.try_run_one(); // returns true after completing one of the ready futures
    /// pool.try_run_one(); // returns true after completing the other ready future
    ///
    /// // the remaining task can not be completed
    /// assert!(!pool.try_run_one()); // returns false
    /// ```
    ///
    /// This function will not block the calling thread and will return the moment
    /// that there are no tasks left for which progress can be made or after exactly one
    /// task was completed; Remaining incomplete tasks in the pool can continue with
    /// further use of one of the pool's run or poll methods.
    /// Though only one task will be completed, progress may be made on multiple tasks.
    pub fn try_run_one(&mut self) -> Option<usize> {
        poll_executor(|ctx| {
            loop {
                let ret = self.poll_pool_once(ctx);

                // return if we have executed a future
                if let Poll::Ready(Some(key)) = ret {
                    return Some(key);
                }

                // if there are no new incoming futures
                // then there is no feature that can make progress
                // and we can return without having completed a single future
                if self.incoming.borrow().queue.is_empty() {
                    return None;
                }
            }
        })
    }

    /// Runs all tasks in the pool and returns if no more progress can be made
    /// on any task.
    ///
    /// ```
    /// use futures::executor::LocalPool;
    /// use futures::task::LocalSpawnExt;
    /// use futures::future::{ready, pending};
    ///
    /// let mut pool = LocalPool::new();
    /// let spawner = pool.spawner();
    ///
    /// spawner.spawn_local(ready(())).unwrap();
    /// spawner.spawn_local(ready(())).unwrap();
    /// spawner.spawn_local(pending()).unwrap();
    ///
    /// // Runs the two ready task and returns.
    /// // The empty task remains in the pool.
    /// pool.run_until_stalled();
    /// ```
    ///
    /// This function will not block the calling thread and will return the moment
    /// that there are no tasks left for which progress can be made;
    /// remaining incomplete tasks in the pool can continue with further use of one
    /// of the pool's run or poll methods. While the function is running, all tasks
    /// in the pool will try to make progress.
    pub fn run_until_stalled(&mut self) {
        poll_executor(|ctx| {
            let _ = self.poll_pool(ctx);
        });
    }

    // Make maximal progress on the entire pool of spawned task, returning `Ready`
    // if the pool is empty and `Pending` if no further progress can be made.
    fn poll_pool(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        // state for the FuturesUnordered, which will never be used
        loop {
            let ret = self.poll_pool_once(cx);

            // we queued up some new tasks; add them and poll again
            if !self.incoming.borrow().queue.is_empty() {
                continue;
            }

            // no queued tasks; we may be done
            match ret {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(()),
                _ => {}
            }
        }
    }

    // Try make minimal progress on the pool of spawned tasks
    fn poll_pool_once(&mut self, cx: &mut Context<'_>) -> Poll<Option<usize>> {
        // empty the incoming queue of newly-spawned tasks
        {
            let mut incoming = self.incoming.borrow_mut();
            for (key, task) in incoming.queue.drain(..) {
                self.pool.push(IndexWrapper {
                    data: task,
                    index: key,
                })
            }
        }

        // try to execute the next ready future
        self.pool
            .poll_next_unpin(cx)
            .map(|poll| poll.map(|wrapper| wrapper.index))
    }
}

impl Default for LocalPool {
    fn default() -> Self {
        Self::new()
    }
}

impl Spawn for LocalSpawner {
    fn spawn_obj(&self, future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
        self.spawn_obj_with_id(future).map(|_| ())
    }

    fn status(&self) -> Result<(), SpawnError> {
        if self.incoming.upgrade().is_some() {
            Ok(())
        } else {
            Err(SpawnError::shutdown())
        }
    }
}

impl LocalSpawn for LocalSpawner {
    fn spawn_local_obj(&self, future: LocalFutureObj<'static, ()>) -> Result<(), SpawnError> {
        self.spawn_local_obj_with_id(future).map(|_| ())
    }

    fn status_local(&self) -> Result<(), SpawnError> {
        if self.incoming.upgrade().is_some() {
            Ok(())
        } else {
            Err(SpawnError::shutdown())
        }
    }
}

impl SpawnWithId for LocalSpawner {
    fn spawn_obj_with_id(&self, future: FutureObj<'static, ()>) -> Result<usize, SpawnError> {
        if let Some(incoming) = self.incoming.upgrade() {
            let mut incoming = incoming.borrow_mut();
            let id = incoming.index;
            incoming.index += 1;
            incoming.queue.push((id, future.into()));
            Ok(id)
        } else {
            Err(SpawnError::shutdown())
        }
    }
}

impl LocalSpawnWithId for LocalSpawner {
    fn spawn_local_obj_with_id(
        &self,
        future: LocalFutureObj<'static, ()>,
    ) -> Result<usize, SpawnError> {
        if let Some(incoming) = self.incoming.upgrade() {
            let mut incoming = incoming.borrow_mut();
            let id = incoming.index;
            incoming.index += 1;
            incoming.queue.push((id, future));
            Ok(id)
        } else {
            Err(SpawnError::shutdown())
        }
    }
}

pub trait SpawnWithId {
    fn spawn_obj_with_id(&self, future: FutureObj<'static, ()>) -> Result<usize, SpawnError>;
}

pub trait LocalSpawnWithId {
    fn spawn_local_obj_with_id(
        &self,
        future: LocalFutureObj<'static, ()>,
    ) -> Result<usize, SpawnError>;
}

impl<Sp: ?Sized> SpawnWithIdExt for Sp where Sp: SpawnWithId {}
impl<Sp: ?Sized> LocalSpawnWithIdExt for Sp where Sp: LocalSpawnWithId {}

/// Extension trait for `Spawn`.
pub trait SpawnWithIdExt: SpawnWithId {
    /// Spawns a task that polls the given future with output `()` to
    /// completion.
    ///
    /// This method returns a [`Result`] that contains a [`SpawnError`] if
    /// spawning fails.
    ///
    /// You can use [`spawn_with_handle`](SpawnExt::spawn_with_handle) if
    /// you want to spawn a future with output other than `()` or if you want
    /// to be able to await its completion.
    ///
    /// Note this method will eventually be replaced with the upcoming
    /// `Spawn::spawn` method which will take a `dyn Future` as input.
    /// Technical limitations prevent `Spawn::spawn` from being implemented
    /// today. Feel free to use this method in the meantime.
    ///
    /// ```
    /// use futures::executor::ThreadPool;
    /// use futures::task::SpawnExt;
    ///
    /// let executor = ThreadPool::new().unwrap();
    ///
    /// let future = async { /* ... */ };
    /// executor.spawn(future).unwrap();
    /// ```
    fn spawn<Fut>(&self, future: Fut) -> Result<usize, SpawnError>
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.spawn_obj_with_id(FutureObj::new(Box::new(future)))
    }

    /// Spawns a task that polls the given future to completion and returns a
    /// future that resolves to the spawned future's output.
    ///
    /// This method returns a [`Result`] that contains a [`RemoteHandle`](crate::future::RemoteHandle), or, if
    /// spawning fails, a [`SpawnError`]. [`RemoteHandle`](crate::future::RemoteHandle) is a future that
    /// resolves to the output of the spawned future.
    ///
    /// ```
    /// use futures::executor::{block_on, ThreadPool};
    /// use futures::future;
    /// use futures::task::SpawnExt;
    ///
    /// let executor = ThreadPool::new().unwrap();
    ///
    /// let future = future::ready(1);
    /// let (id, join_handle_fut) = executor.spawn_with_handle(future).unwrap();
    /// assert_eq!(block_on(join_handle_fut), 1);
    /// ```
    fn spawn_with_handle<Fut>(
        &self,
        future: Fut,
    ) -> Result<(usize, RemoteHandle<Fut::Output>), SpawnError>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send,
    {
        let (future, handle) = future.remote_handle();
        let id = self.spawn(future)?;
        Ok((id, handle))
    }
}

/// Extension trait for `LocalSpawn`.
pub trait LocalSpawnWithIdExt: LocalSpawnWithId {
    /// Spawns a task that polls the given future with output `()` to
    /// completion.
    ///
    /// This method returns a [`Result`] that contains a [`SpawnError`] if
    /// spawning fails.
    ///
    /// You can use [`spawn_with_handle`](SpawnExt::spawn_with_handle) if
    /// you want to spawn a future with output other than `()` or if you want
    /// to be able to await its completion.
    ///
    /// Note this method will eventually be replaced with the upcoming
    /// `Spawn::spawn` method which will take a `dyn Future` as input.
    /// Technical limitations prevent `Spawn::spawn` from being implemented
    /// today. Feel free to use this method in the meantime.
    ///
    /// ```
    /// use futures::executor::LocalPool;
    /// use futures::task::LocalSpawnExt;
    ///
    /// let executor = LocalPool::new();
    /// let spawner = executor.spawner();
    ///
    /// let future = async { /* ... */ };
    /// spawner.spawn_local(future).unwrap();
    /// ```
    fn spawn_local<Fut>(&self, future: Fut) -> Result<usize, SpawnError>
    where
        Fut: Future<Output = ()> + 'static,
    {
        self.spawn_local_obj_with_id(LocalFutureObj::new(Box::new(future)))
    }

    /// Spawns a task that polls the given future to completion and returns a
    /// future that resolves to the spawned future's output.
    ///
    /// This method returns a [`Result`] that contains a [`RemoteHandle`](crate::future::RemoteHandle), or, if
    /// spawning fails, a [`SpawnError`]. [`RemoteHandle`](crate::future::RemoteHandle) is a future that
    /// resolves to the output of the spawned future.
    ///
    /// ```
    /// use futures::executor::LocalPool;
    /// use futures::task::LocalSpawnExt;
    ///
    /// let mut executor = LocalPool::new();
    /// let spawner = executor.spawner();
    ///
    /// let future = async { 1 };
    /// let (id, join_handle_fut) = spawner.spawn_local_with_handle(future).unwrap();
    /// assert_eq!(executor.run_until(join_handle_fut), 1);
    /// ```
    fn spawn_local_with_handle<Fut>(
        &self,
        future: Fut,
    ) -> Result<(usize, RemoteHandle<Fut::Output>), SpawnError>
    where
        Fut: Future + 'static,
    {
        let (future, handle) = future.remote_handle();
        let id = self.spawn_local(future)?;
        Ok((id, handle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracking() {
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
    }
}
