//! A simple (<150 LoC, dependency-free) "oneshot" channel for asynchronously sending a single value between tasks, in a thread-safe manner and async-runtime-agnostic manner.
//!
//! This crate provides a oneshot channel that allows sending a single value from one
//! producer to one consumer. The handle to the sender can be cloned, but only one send
//! operation succeeds. Supports tasks running on different threads.
//!
//! See [`oneshot`] for more details.
//!
//! # Examples
//!
//! Basic usage:
//! ```rust
//! # use futures::executor::block_on;
//! use async_oneshot_channel::oneshot;
//! let (tx, rx) = oneshot();
//! let result = tx.send(42);
//! assert!(result.is_ok());
//!
//! let received = block_on(rx.recv());
//! assert_eq!(received, Some(42));
//! ```
//!
//! Multiple senders (only one succeeds):
//! ```rust
//! # use futures::executor::block_on;
//! # use async_oneshot_channel::oneshot;
//! let (tx1, rx) = oneshot();
//! let tx2 = tx1.clone();
//!
//! // First send succeeds
//! assert!(tx1.send(1).is_ok());
//! // Second send fails and returns the value
//! assert_eq!(tx2.send(2), Err(2));
//!
//! let received = block_on(rx.recv());
//! assert_eq!(received, Some(1));
//! ```
//!
//! Handling sender drop:
//! ```rust
//! # use futures::executor::block_on;
//! # use async_oneshot_channel::oneshot;
//! let (tx, rx) = oneshot::<()>();
//! drop(tx);
//!
//! // Receiver gets None when all senders are dropped without sending
//! let received = block_on(rx.recv());
//! assert_eq!(received, None);
//! ```

use std::{
    cell::Cell,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Once, OnceLock,
    },
    task::{Context, Poll, Waker},
};

/// Creates a new oneshot channel pair of sender and receiver.
///
/// The channel allows for multiple senders (through cloning) but only one send
/// operation will succeed. The first sender to successfully call `send` will
/// transfer the value, and all subsequent sends will fail, returning the input value.
///
/// # Examples
///
/// ```rust
/// # use futures::executor::block_on;
/// # use async_oneshot_channel::oneshot;
/// let (tx, rx) = oneshot();
///
/// // Send a value
/// tx.send(42).unwrap();
///
/// // Receive the value
/// assert_eq!(block_on(rx.recv()), Some(42));
///
/// // A second send will fail
/// assert_eq!(tx.send(43), Err(43));
/// // A second receive will return None
/// assert_eq!(block_on(rx.recv()), None);
/// ```
pub fn oneshot<T>() -> (Sender<T>, Receiver<T>) {
    let chan = Arc::new(Chan::new());
    (Sender { chan: chan.clone() }, Receiver { chan })
}

struct Chan<T> {
    tx: Once,
    sender_rc: AtomicUsize,
    data: Cell<Option<T>>,
    waker: OnceLock<Waker>,
}

impl<T> Chan<T> {
    const fn new() -> Self {
        Self {
            tx: Once::new(),
            sender_rc: AtomicUsize::new(1),
            data: Cell::new(None),
            waker: OnceLock::new(),
        }
    }

    /// Attempts to store a value in the channel.
    ///
    /// # Returns
    /// - `Ok(())` if the value was successfully stored
    /// - `Err(T)` if a value has already been stored, returning the provided value
    fn set(&self, data: T) -> Result<(), T> {
        let mut data = Some(data);
        self.tx.call_once(|| {
            self.data.set(data.take());
        });
        match data {
            None => {
                if let Some(waker) = self.waker.get() {
                    waker.wake_by_ref();
                }
                Ok(())
            }
            Some(data) => Err(data),
        }
    }

    /// Attempts to take the stored value from the channel.
    ///
    /// # Returns
    /// - `Some(T)` if a value was successfully retrieved
    /// - `None` if no value is available or the value was already taken
    ///
    /// # Safety
    /// This function assumes that the value stored in `data` is valid if `tx` has
    /// completed, but `rx` has not.
    fn take(&self) -> Option<T> {
        if !self.tx.is_completed() {
            return None;
        }

        self.data.take()
    }

    /// Returns true if a value has been stored in the channel.
    fn is_set(&self) -> bool {
        self.tx.is_completed()
    }

    /// Returns true if all senders have been dropped.
    fn is_dropped(&self) -> bool {
        self.sender_rc.load(Ordering::Acquire) == 0
    }
}

unsafe impl<T: Send + Sync> Sync for Chan<T> {}

/// The sending half of the oneshot channel.
///
/// Multiple `Sender`s may exist (through cloning), but only one send operation
/// will succeed. Senders can be freely cloned and sent between threads.
///
/// See [`oneshot`] for more details.
pub struct Sender<T> {
    chan: Arc<Chan<T>>,
}

/// The receiving half of the oneshot channel.
///
/// Only one receiver exists for each channel, and it can only successfully
/// receive one value.
///
/// See [`oneshot`] for more details.
pub struct Receiver<T> {
    chan: Arc<Chan<T>>,
}

impl<T> Sender<T> {
    /// Attempts to send a value through the channel.
    ///
    /// # Returns
    /// - `Ok(())` if the value was successfully sent
    /// - `Err(T)` if the channel already contains a value/has been used,
    ///   returning ownership of the input value
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use async_oneshot_channel::oneshot;
    /// let (tx, rx) = oneshot();
    ///
    /// // First send succeeds
    /// assert!(tx.send(1).is_ok());
    ///
    /// // Second send fails
    /// assert_eq!(tx.send(2), Err(2));
    /// ```
    pub fn send(&self, data: T) -> Result<(), T> {
        self.chan.set(data)
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.chan.sender_rc.fetch_add(1, Ordering::Release);
        Self {
            chan: self.chan.clone(),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.chan.sender_rc.fetch_sub(1, Ordering::AcqRel) == 1 {
            if let Some(waker) = self.chan.waker.get() {
                waker.wake_by_ref();
            }
        }
    }
}

/// Future returned by [`oneshot`](oneshot). When awaited, resolves
/// to the value sent through the channel, or `None` if either:
/// - all senders were dropped without sending a value
/// - the value was already received
///
/// See [`Receiver::recv`](Receiver::recv) and [`oneshot`] for more details.
pub struct Recv<T> {
    chan: Arc<Chan<T>>,
}

impl<T> Future for Recv<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // fast path
        if self.chan.is_set() || self.chan.is_dropped() {
            return Poll::Ready(self.chan.take());
        }

        // Only need to set waker on first poll
        self.chan.waker.get_or_init(|| cx.waker().clone());

        if self.chan.is_set() || self.chan.is_dropped() {
            Poll::Ready(self.chan.take())
        } else {
            Poll::Pending
        }
    }
}

impl<T> Receiver<T> {
    /// Asynchronously receives a value from the channel.
    ///
    /// # Returns
    /// - `Some(T)` if a value was successfully received
    /// - `None` if all senders were dropped without sending a value or if
    ///   the value was already received
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use futures::executor::block_on;
    /// # use async_oneshot_channel::oneshot;
    /// let (tx, rx) = oneshot();
    ///
    /// // Send a value
    /// tx.send(42).unwrap();
    ///
    /// // Receive the value
    /// assert_eq!(block_on(rx.recv()), Some(42));
    ///
    /// // Second receive returns None
    /// assert_eq!(block_on(rx.recv()), None);
    /// ```
    pub fn recv(&self) -> Recv<T> {
        Recv {
            chan: self.chan.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::{
        task::JoinSet,
        time::{sleep, Duration},
    };

    #[tokio::test]
    async fn test_basic_send_recv() {
        let (tx, rx) = oneshot();
        tx.send(42).unwrap();
        assert_eq!(rx.recv().await, Some(42));
    }

    #[tokio::test]
    async fn test_multiple_sends_fail() {
        let (tx, rx) = oneshot();
        assert!(tx.send(1).is_ok());
        assert!(tx.send(2).is_err());
        assert_eq!(rx.recv().await, Some(1));
    }

    #[tokio::test]
    async fn test_multiple_receives_fail() {
        let (tx, rx) = oneshot();
        tx.send(1).unwrap();
        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn test_sender_drop_before_send() {
        let (tx, rx) = oneshot::<i32>();
        drop(tx);
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn test_receiver_drop_before_receive() {
        let (tx, _rx) = oneshot();
        assert!(tx.send(1).is_ok());
    }

    #[tokio::test]
    async fn test_concurrent_send_receive() {
        for _ in 0..1000 {
            let (tx, rx) = oneshot();

            let tx_handle = tokio::spawn(async move {
                sleep(Duration::from_micros(1)).await;
                tx.send(42)
            });

            let rx_handle = tokio::spawn(async move { rx.recv().await });

            let (send_result, receive_result) = tokio::join!(tx_handle, rx_handle);
            assert!(send_result.unwrap().is_ok());
            assert_eq!(receive_result.unwrap(), Some(42));
        }
    }

    #[tokio::test]
    async fn test_clone_sender() {
        let (tx1, rx) = oneshot();
        let tx2 = tx1.clone();

        // Only one sender should succeed
        let handle1 = tokio::spawn(async move {
            sleep(Duration::from_micros(1)).await;
            tx1.send(1)
        });

        let handle2 = tokio::spawn(async move {
            sleep(Duration::from_micros(1)).await;
            tx2.send(2)
        });

        let (result1, result2) = tokio::join!(handle1, handle2);
        let results = [result1.unwrap(), result2.unwrap()];
        assert!(results.iter().filter(|r| r.is_ok()).count() == 1);
        assert!(results.iter().filter(|r| r.is_err()).count() == 1);

        let received = rx.recv().await;
        assert!(received.is_some());
        assert!([1, 2].contains(&received.unwrap()));
    }

    #[tokio::test]
    async fn test_sender_ref_counting() {
        let (tx1, rx) = oneshot::<i32>();
        let tx2 = tx1.clone();
        let tx3 = tx2.clone();

        assert!(!rx.chan.is_dropped());
        drop(tx1);
        assert!(!rx.chan.is_dropped());
        drop(tx2);
        assert!(!rx.chan.is_dropped());
        drop(tx3);
        assert!(rx.chan.is_dropped());
    }

    #[tokio::test]
    async fn test_concurrent_clone_and_send() {
        for _ in 0..1000 {
            let (tx, rx) = oneshot();
            let tx = Arc::new(tx);

            let mut jset = JoinSet::new();

            // Spawn multiple threads that clone and try to send
            for i in 0..10 {
                let tx = tx.clone();
                jset.spawn(async move {
                    let tx = tx.clone();
                    sleep(Duration::from_micros(1)).await;
                    tx.send(i)
                });
            }

            let results = jset.join_all().await;
            let ok_count = results.iter().filter(|r| r.is_ok()).count();
            assert_eq!(ok_count, 1);

            let received = rx.recv().await;
            assert!(received.is_some());
        }
    }

    #[test]
    fn test_sync_send() {
        fn assert_sync<T: Sync>() {}
        fn assert_send<T: Send>() {}

        assert_sync::<Chan<i32>>();
        assert_send::<Chan<i32>>();
        assert_sync::<Sender<i32>>();
        assert_send::<Sender<i32>>();
        assert_sync::<Receiver<i32>>();
        assert_send::<Receiver<i32>>();
    }

    #[tokio::test]
    async fn test_concurrent_take_operations() {
        for _ in 0..1000 {
            let (tx, rx) = oneshot();
            let rx = Arc::new(rx);

            tx.send(42).unwrap();

            let mut jset = JoinSet::new();
            for _ in 0..10 {
                let rx = rx.clone();
                jset.spawn(tokio::spawn(async move { rx.recv().await }));
            }

            let results = jset.join_all().await;
            let some_count = results
                .iter()
                .filter(|r| r.as_ref().unwrap().is_some())
                .count();
            assert_eq!(some_count, 1);
        }
    }

    #[tokio::test]
    async fn test_receive_after_sender_dropped() {
        let (tx, rx) = oneshot();
        tx.send(42).unwrap();
        drop(tx);
        assert_eq!(rx.recv().await, Some(42));
    }

    #[tokio::test]
    async fn test_receive_timeout() {
        let (tx, rx) = oneshot();

        let timeout_result = tokio::time::timeout(Duration::from_millis(10), rx.recv()).await;

        assert!(timeout_result.is_err()); // Should timeout

        tx.send(42).unwrap();

        let timeout_result = tokio::time::timeout(Duration::from_millis(10), rx.recv()).await;

        assert!(timeout_result.is_ok());
        assert_eq!(timeout_result.unwrap(), Some(42));
    }

    #[tokio::test]
    async fn test_detailed_sender_drops() {
        // Case 1: Single sender drops before send
        let (tx, rx) = oneshot::<i32>();
        drop(tx);
        assert_eq!(rx.recv().await, None);

        // Case 2: One sender drops, other sends successfully
        let (tx, rx) = oneshot::<i32>();
        let tx2 = tx.clone();
        drop(tx);
        assert_eq!(tx2.send(42), Ok(()));
        assert_eq!(rx.recv().await, Some(42));

        // Case 3: All senders drop before send
        let (tx, rx) = oneshot::<i32>();
        let tx2 = tx.clone();
        drop(tx);
        drop(tx2);
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn test_detached_spawn_send() {
        let (tx, rx) = oneshot::<i32>();

        tokio::spawn(async move {
            let send = tx.send(42);
            assert_eq!(send, Ok(()));
            let send = tx.send(43);
            assert_eq!(send, Err(43));
        });

        let data = rx.recv().await;
        assert_eq!(data, Some(42));
        let data = rx.recv().await;
        assert_eq!(data, None);
    }

    #[tokio::test]
    async fn test_detached_spawn_with_clone() {
        let (tx, rx) = oneshot::<i32>();

        tokio::spawn(async move {
            let tx2 = tx.clone();
            let send = tx.send(42);
            assert_eq!(send, Ok(()));
            let send = tx2.send(43);
            assert_eq!(send, Err(43));
        });

        let data = rx.recv().await;
        assert_eq!(data, Some(42));
        let data = rx.recv().await;
        assert_eq!(data, None);
    }

    #[test]
    fn trait_compiles() {
        fn test_send<T: Send>() {}
        fn test_sync<T: Sync>() {}

        test_send::<Sender<i32>>();
        test_send::<Receiver<i32>>();
        test_sync::<Sender<i32>>();
        test_sync::<Receiver<i32>>();
        test_send::<Chan<i32>>();
        test_sync::<Chan<i32>>();
    }
}
