//! A oneshot broadcast channel.
//!
//! The motivation for this channel type was to allow multiple
//! receivers to either wait for something to finish,
//! or to have an inexpensive method of checking if it has finished.
//!
//! See [`channel()`].

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::task::{Context, Poll, Waker};

use slotmap_careful::DenseSlotMap;

slotmap_careful::new_key_type! { struct WakerKey; }

/// A [oneshot broadcast][crate::util::oneshot_broadcast] sender.
#[derive(Debug)]
pub(crate) struct Sender<T> {
    /// State shared with all [`Receiver`]s.
    shared: Weak<Shared<T>>,
}

/// A [oneshot broadcast][crate::util::oneshot_broadcast] receiver.
#[derive(Clone, Debug)]
pub(crate) struct Receiver<T> {
    /// State shared with the sender and all other receivers.
    shared: Arc<Shared<T>>,
}

/// State shared between the sender and receivers.
/// Correctness:
///
/// Sending a message:
///  - set the message OnceLock (A)
///  - acquire the wakers Mutex
///  - take all wakers (B)
///  - release the wakers Mutex (C)
///  - wake all wakers
///
/// Polling:
///  - if message was set, return it (fast path)
///  - acquire the wakers Mutex (D)
///  - if message was set, return it (E)
///  - add waker (F)
///  - release the wakers Mutex
///
/// When the wakers Mutex is released at (C), a release-store operation is performed by the Mutex,
/// which means that the message set at (A) will be seen by all future acquire-load operations by
/// that same Mutex. More specifically, after (C) has occured and when the same mutex is acquired at
/// (D), the message set at (A) is guaranteed to be visible at (E). This means that after the wakers
/// are taken at (B), no future wakers will be added at (F) and no waker will be "lost".
#[derive(Debug)]
struct Shared<T> {
    /// The message sent from the [`Sender`] to the [`Receiver`]s.
    msg: OnceLock<Result<T, CancelledError>>,
    /// The wakers waiting for a value to be sent.
    /// Will be set to `None` after the wakers have been woken.
    // the `Option` isn't really needed here,
    // but we use it to help detect bugs where something
    // tries to add a `Waker` after we've already woken them all
    wakers: Mutex<Option<DenseSlotMap<WakerKey, Waker>>>,
}

/// A future that will be ready when the sender sends a message or is dropped.
///
/// This borrows the shared state from the [`Receiver`],
/// so is more efficient than [`ReceiverOwnedFuture`].
#[derive(Debug)]
struct ReceiverBorrowedFuture<'a, T> {
    /// State shared with the sender and all other receivers.
    shared: &'a Shared<T>,
    /// The key for any waker that we've added to [`Shared::wakers`].
    waker_key: Option<WakerKey>,
}

/// A future that will be ready when the sender sends a message or is dropped.
///
/// This holds an `Arc` of the shared state,
/// so can be used as a `'static` future.
// it would have been nice if we could store a `ReceiverBorrowedFuture`
// holding a reference to our `Arc<Shared>`,
// but that would be a self-referential struct,
// so we need to duplicate everything instead
#[derive(Debug)]
struct ReceiverOwnedFuture<T> {
    /// State shared with the sender and all other receivers.
    shared: Arc<Shared<T>>,
    /// The key for any waker that we've added to [`Shared::wakers`].
    waker_key: Option<WakerKey>,
}

/// The sender was dropped, so the channel is closed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct CancelledError;

/// Create a new oneshot broadcast channel.
pub(crate) fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        msg: OnceLock::new(),
        wakers: Mutex::new(Some(DenseSlotMap::with_key())),
    });

    let sender = Sender {
        shared: Arc::downgrade(&shared),
    };

    let receiver = Receiver { shared };

    (sender, receiver)
}

impl<T> Sender<T> {
    /// Send the message to the [`Receiver`]s.
    ///
    /// The message may be lost if all receivers have been dropped.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn send(self, msg: T) {
        // Even if the `Weak` upgrade is successful,
        // it's possible that the last receiver
        // will be dropped during this `send` method,
        // in which case we will be holding the last `Arc`.
        //
        // We could return the message in an `Err` here,
        // but I don't see a use case for it since we don't
        // have a race-free method to ensure that the message isn't lost.
        // So best to keep things simple and just drop the message.
        let Some(shared) = self.shared.upgrade() else {
            return;
        };

        // set the message and inform the wakers
        if Self::send_and_wake(&shared, Ok(msg)).is_err() {
            // this 'send()` method takes an owned self,
            // and we don't send a message outside of here and the drop handler,
            // so this shouldn't be possible
            unreachable!("the message was already set");
        }
    }

    /// Send the message, and wake and clear all wakers.
    ///
    /// If the message was unable to be set, returns the message in an `Err`.
    fn send_and_wake(
        shared: &Shared<T>,
        msg: Result<T, CancelledError>,
    ) -> Result<(), Result<T, CancelledError>> {
        // set the message
        shared.msg.set(msg)?;

        let mut wakers = {
            let mut wakers = shared.wakers.lock().expect("poisoned");
            // Take the wakers and drop the mutex guard, releasing the lock.
            // We could use `mem::take` here, but the `Option` should help catch bugs if something
            // tries adding a new waker later after we've already woken the wakers.
            //
            // The above `msg.set()` will only ever succeed once,
            // which means that we should only end up here once.
            wakers.take().expect("wakers were taken more than once")
        };

        // Once we drop the mutex guard, which does a release-store on its own atomic, any other
        // code which later acquires the wakers mutex is guaranteed to see the msg as "set".
        // See comments on `Shared`.

        // Wake while not holding the lock.
        // Since the lock is used in `ReceiverFuture::poll` and should not block for long periods of
        // time, we don't want to run third-party waker code here while holding the mutex.
        for (_key, waker) in wakers.drain() {
            waker.wake();
        }

        Ok(())
    }

    /// Returns `true` if all [`Receiver`]s (and all futures created from the receivers) have been
    /// dropped.
    ///
    /// This can be useful to skip doing extra work to generate the message if the message will be
    /// discarded anyways.
    // This is for external use.
    // It is not always valid to call this internally.
    // For example when we've done a `Weak::upgrade` internally, like in `send`,
    // this won't return the correct value.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.shared.strong_count() == 0
    }
}

impl<T> std::ops::Drop for Sender<T> {
    fn drop(&mut self) {
        let Some(shared) = self.shared.upgrade() else {
            // all receivers have dropped; nothing to do
            return;
        };

        // set an error message to indicate that the sender was dropped and inform the wakers;
        // it's fine if setting the message fails since it might have been set previously during a
        // `send()`
        let _ = Self::send_and_wake(&shared, Err(CancelledError));
    }
}

impl<T> Receiver<T> {
    /// Receive the message from the [`Sender`].
    ///
    /// This is cancellation-safe.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn recv(&self) -> Result<&T, CancelledError> {
        ReceiverBorrowedFuture {
            shared: &self.shared,
            waker_key: None,
        }
        .await
    }

    /// The receiver is ready.
    ///
    /// If `true`, the [`Sender`] has either sent its message or been dropped.
    pub(crate) fn is_ready(&self) -> bool {
        self.shared.msg.get().is_some()
    }
}

impl<T: Clone + 'static> Receiver<T> {
    /// Receive and clone the message from the [`Sender`].
    ///
    /// This returns a `'static` future and is slightly more expensive than [`recv`](Self::recv).
    ///
    /// This is cancellation-safe.
    pub(crate) fn recv_clone(&self) -> impl Future<Output = Result<T, CancelledError>> + 'static {
        ReceiverOwnedFuture {
            shared: Arc::clone(&self.shared),
            waker_key: None,
        }
    }
}

impl<'a, T> Future for ReceiverBorrowedFuture<'a, T> {
    type Output = Result<&'a T, CancelledError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let self_ = self.get_mut();
        receiver_fut_poll(self_.shared, &mut self_.waker_key, cx.waker())
    }
}

impl<T> std::ops::Drop for ReceiverBorrowedFuture<'_, T> {
    fn drop(&mut self) {
        receiver_fut_drop(self.shared, &mut self.waker_key);
    }
}

impl<T: Clone> Future for ReceiverOwnedFuture<T> {
    type Output = Result<T, CancelledError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let self_ = self.get_mut();
        match receiver_fut_poll(&self_.shared, &mut self_.waker_key, cx.waker()) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(x) => Poll::Ready(x.cloned()),
        }
    }
}

impl<T> std::ops::Drop for ReceiverOwnedFuture<T> {
    fn drop(&mut self) {
        receiver_fut_drop(&self.shared, &mut self.waker_key);
    }
}

/// The shared poll implementation for receiver futures.
fn receiver_fut_poll<'a, T>(
    shared: &'a Shared<T>,
    waker_key: &mut Option<WakerKey>,
    new_waker: &Waker,
) -> Poll<Result<&'a T, CancelledError>> {
    // if the message was already set, return it
    if let Some(msg) = shared.msg.get() {
        return Poll::Ready(msg.as_ref().or(Err(CancelledError)));
    }

    let mut wakers = shared.wakers.lock().expect("poisoned");

    // check again now that we've acquired the mutex
    if let Some(msg) = shared.msg.get() {
        return Poll::Ready(msg.as_ref().or(Err(CancelledError)));
    }

    // we have acquired the wakers mutex and checked that the message wasn't set,
    // so we know that wakers have not yet been woken
    // and it's okay to add our waker to the wakers map
    let wakers = wakers.as_mut().expect("wakers were already woken");

    match waker_key {
        // we have added a waker previously
        Some(waker_key) => {
            // replace the old entry
            let waker = wakers
                .get_mut(*waker_key)
                // the waker is only removed from the map by our drop handler,
                // so the waker should never be missing
                .expect("waker key is missing from map");
            waker.clone_from(new_waker);
        }
        // we have never added a waker
        None => {
            // add a new entry
            let new_key = wakers.insert(new_waker.clone());
            *waker_key = Some(new_key);
        }
    }

    Poll::Pending
}

/// The shared drop implementation for receiver futures.
fn receiver_fut_drop<T>(shared: &Shared<T>, waker_key: &mut Option<WakerKey>) {
    if let Some(waker_key) = waker_key.take() {
        let mut wakers = shared.wakers.lock().expect("poisoned");
        if let Some(wakers) = wakers.as_mut() {
            wakers
                .remove(waker_key)
                // this is the only place that removes the waker from the map,
                // so the waker should never be missing
                .expect("the waker key was not found");
        }
    }
}

impl std::fmt::Display for CancelledError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the sender was dropped")
    }
}

impl std::error::Error for CancelledError {}

impl<T> Shared<T> {
    /// Count the number of wakers.
    #[cfg(test)]
    fn count_wakers(&self) -> usize {
        self.wakers
            .lock()
            .expect("poisoned")
            .as_ref()
            .map(|x| x.len())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod test {
    #![allow(clippy::unwrap_used)]

    use super::*;

    use futures::future::FutureExt;
    use futures::task::SpawnExt;

    #[test]
    fn standard_usage() {
        tor_rtmock::MockRuntime::test_with_various(|_rt| async move {
            let (tx, rx) = channel();
            tx.send(0_u8);
            assert_eq!(rx.recv().await, Ok(&0));

            let (tx, rx) = channel();
            tx.send(0_u8);
            assert_eq!(rx.recv_clone().await, Ok(0));
        });
    }

    #[test]
    fn immediate_drop() {
        let _ = channel::<()>();

        let (tx, rx) = channel::<()>();
        drop(tx);
        drop(rx);

        let (tx, rx) = channel::<()>();
        drop(rx);
        drop(tx);
    }

    #[test]
    fn drop_sender() {
        tor_rtmock::MockRuntime::test_with_various(|_rt| async move {
            let (tx, rx_1) = channel::<u8>();

            let rx_2 = rx_1.clone();
            drop(tx);
            let rx_3 = rx_1.clone();
            assert_eq!(rx_1.recv().await, Err(CancelledError));
            assert_eq!(rx_2.recv().await, Err(CancelledError));
            assert_eq!(rx_3.recv().await, Err(CancelledError));
        });
    }

    #[test]
    fn clone_before_send() {
        tor_rtmock::MockRuntime::test_with_various(|_rt| async move {
            let (tx, rx_1) = channel();

            let rx_2 = rx_1.clone();
            tx.send(0_u8);
            assert_eq!(rx_1.recv().await, Ok(&0));
            assert_eq!(rx_2.recv().await, Ok(&0));
        });
    }

    #[test]
    fn clone_after_send() {
        tor_rtmock::MockRuntime::test_with_various(|_rt| async move {
            let (tx, rx_1) = channel();

            tx.send(0_u8);
            let rx_2 = rx_1.clone();
            assert_eq!(rx_1.recv().await, Ok(&0));
            assert_eq!(rx_2.recv().await, Ok(&0));
        });
    }

    #[test]
    fn clone_after_recv() {
        tor_rtmock::MockRuntime::test_with_various(|_rt| async move {
            let (tx, rx_1) = channel();

            tx.send(0_u8);
            assert_eq!(rx_1.recv().await, Ok(&0));
            let rx_2 = rx_1.clone();
            assert_eq!(rx_2.recv().await, Ok(&0));
        });
    }

    #[test]
    fn drop_one_receiver() {
        tor_rtmock::MockRuntime::test_with_various(|_rt| async move {
            let (tx, rx_1) = channel();

            let rx_2 = rx_1.clone();
            drop(rx_1);
            tx.send(0_u8);
            assert_eq!(rx_2.recv().await, Ok(&0));
        });
    }

    #[test]
    fn drop_all_receivers() {
        let (tx, rx_1) = channel();

        let rx_2 = rx_1.clone();
        drop(rx_1);
        drop(rx_2);
        tx.send(0_u8);
    }

    #[test]
    fn drop_fut() {
        let (_tx, rx) = channel::<u8>();
        let fut = rx.recv();
        assert_eq!(rx.shared.count_wakers(), 0);
        drop(fut);
        assert_eq!(rx.shared.count_wakers(), 0);

        // drop after sending
        let (tx, rx) = channel();
        tx.send(0_u8);
        let fut = rx.recv();
        assert_eq!(rx.shared.count_wakers(), 0);
        drop(fut);
        assert_eq!(rx.shared.count_wakers(), 0);

        // drop after polling once
        let (_tx, rx) = channel::<u8>();
        let mut fut = Box::pin(rx.recv());
        assert_eq!(rx.shared.count_wakers(), 0);
        assert_eq!(fut.as_mut().now_or_never(), None);
        assert_eq!(rx.shared.count_wakers(), 1);
        drop(fut);
        assert_eq!(rx.shared.count_wakers(), 0);

        // drop after polling once and send
        let (tx, rx) = channel();
        let mut fut = Box::pin(rx.recv());
        assert_eq!(rx.shared.count_wakers(), 0);
        assert_eq!(fut.as_mut().now_or_never(), None);
        assert_eq!(rx.shared.count_wakers(), 1);
        tx.send(0_u8);
        assert_eq!(rx.shared.count_wakers(), 0);
        drop(fut);
    }

    #[test]
    fn drop_owned_fut() {
        let (_tx, rx) = channel::<u8>();
        let fut = rx.recv_clone();
        assert_eq!(rx.shared.count_wakers(), 0);
        drop(fut);
        assert_eq!(rx.shared.count_wakers(), 0);

        // drop after sending
        let (tx, rx) = channel();
        tx.send(0_u8);
        let fut = rx.recv_clone();
        assert_eq!(rx.shared.count_wakers(), 0);
        drop(fut);
        assert_eq!(rx.shared.count_wakers(), 0);

        // drop after polling once
        let (_tx, rx) = channel::<u8>();
        let mut fut = Box::pin(rx.recv_clone());
        assert_eq!(rx.shared.count_wakers(), 0);
        assert_eq!(fut.as_mut().now_or_never(), None);
        assert_eq!(rx.shared.count_wakers(), 1);
        drop(fut);
        assert_eq!(rx.shared.count_wakers(), 0);

        // drop after polling once and send
        let (tx, rx) = channel();
        let mut fut = Box::pin(rx.recv_clone());
        assert_eq!(rx.shared.count_wakers(), 0);
        assert_eq!(fut.as_mut().now_or_never(), None);
        assert_eq!(rx.shared.count_wakers(), 1);
        tx.send(0_u8);
        assert_eq!(rx.shared.count_wakers(), 0);
        drop(fut);
    }

    #[test]
    fn is_ready_after_send() {
        let (tx, rx_1) = channel();
        assert!(!rx_1.is_ready());
        let rx_2 = rx_1.clone();
        assert!(!rx_2.is_ready());

        tx.send(0_u8);

        assert!(rx_1.is_ready());
        assert!(rx_2.is_ready());

        let rx_3 = rx_1.clone();
        assert!(rx_3.is_ready());
    }

    #[test]
    fn is_ready_after_drop() {
        let (tx, rx_1) = channel::<u8>();
        assert!(!rx_1.is_ready());
        let rx_2 = rx_1.clone();
        assert!(!rx_2.is_ready());

        drop(tx);

        assert!(rx_1.is_ready());
        assert!(rx_2.is_ready());

        let rx_3 = rx_1.clone();
        assert!(rx_3.is_ready());
    }

    #[test]
    fn is_cancelled() {
        let (tx, rx) = channel::<u8>();
        assert!(!tx.is_cancelled());
        drop(rx);
        assert!(tx.is_cancelled());

        let (tx, rx_1) = channel::<u8>();
        assert!(!tx.is_cancelled());
        let rx_2 = rx_1.clone();
        drop(rx_1);
        assert!(!tx.is_cancelled());
        drop(rx_2);
        assert!(tx.is_cancelled());
    }

    #[test]
    fn recv_in_task() {
        tor_rtmock::MockRuntime::test_with_various(|rt| async move {
            let (tx, rx) = channel();

            let join = rt
                .spawn_with_handle(async move {
                    assert_eq!(rx.recv().await, Ok(&0));
                    assert_eq!(rx.recv_clone().await, Ok(0));
                })
                .unwrap();

            tx.send(0_u8);

            join.await;
        });
    }

    #[test]
    fn recv_multiple_in_task() {
        tor_rtmock::MockRuntime::test_with_various(|rt| async move {
            let (tx, rx) = channel();
            let rx_1 = rx.clone();
            let rx_2 = rx.clone();

            let join_1 = rt
                .spawn_with_handle(async move {
                    assert_eq!(rx_1.recv().await, Ok(&0));
                })
                .unwrap();
            let join_2 = rt
                .spawn_with_handle(async move {
                    assert_eq!(rx_2.recv_clone().await, Ok(0));
                })
                .unwrap();

            tx.send(0_u8);

            join_1.await;
            join_2.await;
            assert_eq!(rx.recv().await, Ok(&0));
        });
    }

    #[test]
    fn recv_multiple_times() {
        tor_rtmock::MockRuntime::test_with_various(|_rt| async move {
            let (tx, rx) = channel();

            tx.send(0_u8);
            assert_eq!(rx.recv().await, Ok(&0));
            assert_eq!(rx.recv().await, Ok(&0));
            assert_eq!(rx.recv_clone().await, Ok(0));
            assert_eq!(rx.recv_clone().await, Ok(0));
        });
    }

    #[test]
    fn stress() {
        // Since we don't really have control over the runtime and where/when tasks are scheduled,
        // we try as best as possible to send the message while simultaneously creating new
        // receivers and waiting on them. It's possible this might be entirely ineffective, but in
        // the worst case, it's still a test with multiple receivers on different tasks, so is
        // useful to have.
        tor_rtmock::MockRuntime::test_with_various(|rt| async move {
            let (tx, rx) = channel();

            rt.spawn(async move {
                // this tries to delay the send a little bit
                // to give time for some of the receiver tasks to start
                for _ in 0..20 {
                    tor_rtcompat::task::yield_now().await;
                }
                tx.send(0_u8);
            })
            .unwrap();

            let mut joins = vec![];
            for _ in 0..100 {
                let rx_clone = rx.clone();
                let join = rt
                    .spawn_with_handle(async move { rx_clone.recv().await.cloned() })
                    .unwrap();
                joins.push(join);
                // allows the send task to make progress if single-threaded
                tor_rtcompat::task::yield_now().await;
            }

            for join in joins {
                assert!(matches!(join.await, Ok(0)));
            }
        });
    }
}
