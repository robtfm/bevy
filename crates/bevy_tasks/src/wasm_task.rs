use alloc::boxed::Box;
use bevy_platform::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use core::{
    any::{type_name, Any},
    future::{Future, IntoFuture},
    panic::{AssertUnwindSafe, UnwindSafe},
    pin::Pin,
    task::{Context, Poll},
};

use futures_channel::oneshot;
use futures_util::future::{AbortHandle, Abortable};

/// Wraps an asynchronous task, a spawned future.
///
/// Tasks are also futures themselves and yield the output of the spawned future.
///
/// When a task is dropped, it gets canceled and won't be polled again. To cancel a task a bit
/// more gracefully and wait until it stops running, use the [`Task::cancel()`] method.
#[derive(Debug)]
#[must_use = "Tasks are canceled when dropped, use `.detach()` to run them in the background."]
pub struct Task<T> {
    receiver: oneshot::Receiver<Result<T, Panic>>,
    /// Aborts the future driving the task; `None` once detached.
    abort: Option<AbortHandle>,
    /// Set once the output has been sent.
    finished: Arc<AtomicBool>,
}

impl<T: 'static> Task<T> {
    pub(crate) fn wrap_future(future: impl Future<Output = T> + 'static) -> Self {
        let (task, driver) = Self::split(future);
        wasm_bindgen_futures::spawn_local(driver);
        task
    }

    /// Splits `future` into the task handle and the future that drives it; the caller
    /// decides where the latter runs.
    pub(crate) fn split(
        future: impl Future<Output = T> + 'static,
    ) -> (Self, impl Future<Output = ()> + 'static) {
        let (sender, receiver) = oneshot::channel();
        let (abort, registration) = AbortHandle::new_pair();
        let finished = Arc::new(AtomicBool::new(false));
        let done = finished.clone();
        let driver = async move {
            // Catch any panics that occur when polling the future so they can
            // be propagated back to the task handle.
            let future = CatchUnwind(AssertUnwindSafe(future));
            // Aborted means the task handle was dropped: nobody is waiting for the output.
            match Abortable::new(future, registration).await {
                Ok(value) => {
                    let _ = sender.send(value);
                    done.store(true, Ordering::Release);
                }
                Err(_aborted) => {
                    tracing::debug!("aborted a task yielding `{}`", type_name::<T>());
                }
            }
        };
        (
            Self {
                receiver: receiver.into_future(),
                abort: Some(abort),
                finished,
            },
            driver,
        )
    }

    /// Detaches the task to let it keep running in the background.
    pub fn detach(mut self) {
        self.abort = None;
    }

    /// Returns `true` if the current task is finished.
    ///
    /// Unlike poll, it doesn't resolve the final value, it just checks if the task has finished.
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// Cancels the task and waits for it to stop running.
    ///
    /// Returns the task's output if it was completed just before it got canceled, or [`None`] if
    /// it didn't complete.
    ///
    /// While it's possible to simply drop the [`Task`] to cancel it, this is a cleaner way of
    /// canceling because it also waits for the task to stop running.
    pub async fn cancel(mut self) -> Option<T> {
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
        match (&mut self.receiver).await {
            Ok(Ok(value)) => Some(value),
            Err(_) => None,
            Ok(Err(panic)) => {
                // drop this to prevent the panic payload from resuming the panic on drop.
                // this also leaks the box but I'm not sure how to avoid that
                core::mem::forget(panic);
                None
            }
        }
    }
}

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        if let Some(abort) = &self.abort {
            abort.abort();
            if matches!(self.receiver.try_recv(), Ok(None)) {
                tracing::debug!(
                    "dropped a pending task yielding `{}`, aborting it",
                    type_name::<T>()
                );
            }
        }
    }
}

impl<T> Future for Task<T> {
    type Output = T;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.receiver).poll(cx) {
            Poll::Ready(Ok(Ok(value))) => Poll::Ready(value),
            // NOTE: Propagating the panic here sorta has parity with the async_executor behavior.
            // For those tasks, polling them after a panic returns a `None` which gets `unwrap`ed, so
            // using `resume_unwind` here is essentially keeping the same behavior while adding more information.
            #[cfg(feature = "std")]
            Poll::Ready(Ok(Err(panic))) => std::panic::resume_unwind(panic),
            #[cfg(not(feature = "std"))]
            Poll::Ready(Ok(Err(_panic))) => {
                unreachable!("catching a panic is only possible with std")
            }
            Poll::Ready(Err(_)) => panic!("Polled a task after it was cancelled"),
            Poll::Pending => Poll::Pending,
        }
    }
}

type Panic = Box<dyn Any + Send + 'static>;

#[pin_project::pin_project]
struct CatchUnwind<F: UnwindSafe>(#[pin] F);

impl<F: Future + UnwindSafe> Future for CatchUnwind<F> {
    type Output = Result<F::Output, Panic>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let f = AssertUnwindSafe(|| self.project().0.poll(cx));

        #[cfg(feature = "std")]
        let result = std::panic::catch_unwind(f)?;

        #[cfg(not(feature = "std"))]
        let result = f();

        result.map(Ok)
    }
}
