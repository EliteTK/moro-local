use std::{cell::RefCell, marker::PhantomData, pin::Pin, rc::Rc, task::Poll};

use futures::{channel::oneshot, future::LocalBoxFuture, stream::FuturesUnordered, Future, Stream};

use crate::Spawned;

/// Represents a moro "async scope". See the [`async_scope`][crate::async_scope] macro for details.
pub struct Scope<'scope, 'env: 'scope, R: 'env> {
    /// Stores the set of futures that have been spawned.
    ///
    /// This is behind a mutex so that multiple concurrent actors can access it.
    /// A `RwLock` seems better, but `FuturesUnordered is not `Sync` in the case.
    /// But in fact it doesn't matter anyway, because all spawned futures execute
    /// CONCURRENTLY and hence there will be no contention.
    futures: RefCell<Pin<Box<FuturesUnordered<LocalBoxFuture<'scope, ()>>>>>,
    terminated: RefCell<Option<R>>,
    phantom: PhantomData<&'scope &'env ()>,
}

impl<'scope, 'env, R> Scope<'scope, 'env, R> {
    /// Create a scope.
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            futures: RefCell::new(Box::pin(FuturesUnordered::new())),
            terminated: Default::default(),
            phantom: Default::default(),
        })
    }

    /// Polls the jobs that were spawned thus far. Returns:
    ///
    /// * `Pending` if there are jobs that cannot complete
    /// * `Ready(Ok(()))` if all jobs are completed
    /// * `Ready(Err(c))` if the scope has been canceled
    ///
    /// Should not be invoked again once `Ready(Err(c))` is returned.
    ///
    /// It is ok to invoke it again after `Ready(Ok(()))` has been returned;
    /// if any new jobs have been spawned, they will execute.
    pub(crate) fn poll_jobs(&self, cx: &mut std::task::Context<'_>) -> Poll<Option<R>> {
        'outer: loop {
            // once we are terminated, we do no more work.
            if let Some(r) = self.terminated.take().take() {
                return Poll::Ready(Some(r));
            }

            while let Some(()) = ready!(self.futures.borrow_mut().as_mut().poll_next(cx)) {
                // once we are terminated, we do no more work.
                if self.terminated.borrow().is_some() {
                    continue 'outer;
                }
            }

            return Poll::Ready(None);
        }
    }

    /// Clear out all pending jobs. This is used when dropping the
    /// scope body to ensure that any possible references to `Scope`
    /// are removed before we drop it.
    ///
    /// # Unsafe contract
    ///
    /// Once this returns, there are no more pending tasks.
    pub(crate) fn clear(&self) {
        self.futures.borrow_mut().clear();
    }

    /// Terminate the scope immediately -- all existing jobs will stop at their next await point
    /// and never wake up again. Anything on their stacks will be dropped. This is most useful
    /// for propagating errors, but it can be used to propagate any kind of final value (e.g.,
    /// perhaps you are searching for something and want to stop once you find it.)
    ///
    /// This returns a future that you should await, but it will never complete
    /// (because you will never be reawoken). Since termination takes effect at the next
    /// await point, awaiting the returned future ensures that your current future stops
    /// immediately.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # futures::executor::block_on(async {
    /// let result = moro_local::async_scope!(|scope| {
    ///     scope.spawn(async { /* ... */ });
    ///
    ///     // Calling `scope.terminate` here will terminate the async
    ///     // scope and use the string `"cancellation-value"` as
    ///     // the final value.
    ///     let result: () = scope.terminate("cancellation-value").await;
    ///     unreachable!() // this code never executes
    /// }).await;
    ///
    /// assert_eq!(result, "cancellation-value");
    /// # });
    /// ```
    pub fn terminate<T>(&'scope self, value: R) -> impl Future<Output = T> + 'scope
    where
        T: 'scope,
    {
        if self.terminated.borrow().is_none() {
            self.terminated.replace(Some(value));
        }

        // The code below will never run
        self.spawn(async { panic!() })
    }

    /// Spawn a job that will run concurrently with everything else in the scope.
    /// The job may access stack fields defined outside the scope.
    /// The scope will not terminate until this job completes or the scope is cancelled.
    pub fn spawn<T>(
        &'scope self,
        future: impl Future<Output = T> + 'scope,
    ) -> Spawned<impl Future<Output = T>>
    where
        T: 'scope,
    {
        // Use a channel to communicate result from the *actual* future
        // (which lives in the futures-unordered) and the caller.
        // This is kind of crappy because, ideally, the caller expressing interest
        // in the result of the future would let it run, but that would require
        // more clever coding and I'm just trying to stand something up quickly
        // here. What will happen when caller expresses an interest in result
        // now is that caller will block which should (eventually) allow the
        // futures-unordered to be polled and make progress. Good enough.

        let (tx, rx) = oneshot::channel();

        self.futures.borrow_mut().push(Box::pin(async move {
            let v = future.await;
            let _ = tx.send(v);
        }));

        Spawned::new(async move {
            match rx.await {
                Ok(v) => v,
                Err(e) => panic!("unexpected error: {e:?}"),
            }
        })
    }
}
