use event_listener::Event;
use std::{
    cell::Cell,
    mem::MaybeUninit,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Once,
    },
};

pub fn oneshot<T>() -> (Sender<T>, Receiver<T>) {
    let chan = Arc::new(Chan::new(1));
    (Sender { chan: chan.clone() }, Receiver { chan })
}

struct Chan<T> {
    tx: Once,
    rx: Once,
    notify: Event,
    sender_rc: AtomicUsize,
    data: Cell<MaybeUninit<T>>,
}

impl<T> Chan<T> {
    const fn new(sender_rc: usize) -> Self {
        Self {
            data: Cell::new(MaybeUninit::uninit()),
            tx: Once::new(),
            rx: Once::new(),
            notify: Event::new(),
            sender_rc: AtomicUsize::new(sender_rc),
        }
    }

    fn set(&self, data: T) -> Result<(), T> {
        let mut data = Some(data);
        self.tx.call_once(|| {
            self.data.replace(MaybeUninit::new(data.take().unwrap()));
        });
        match data {
            None => {
                self.notify.notify(1);
                Ok(())
            }
            Some(data) => Err(data),
        }
    }

    fn take(&self) -> Option<T> {
        if self.rx.is_completed() || !self.tx.is_completed() {
            return None;
        }

        let mut data = None;
        self.rx.call_once(|| {
            data = Some(self.data.replace(MaybeUninit::uninit()));
        });

        // SAFETY: `data` is only `Some` once, due to `self.rx`
        data.map(|data| unsafe { data.assume_init() })
    }

    fn is_set(&self) -> bool {
        self.tx.is_completed()
    }

    fn is_dropped(&self) -> bool {
        self.sender_rc.load(Ordering::Acquire) == 0
    }
}

unsafe impl<T: Send + Sync> Sync for Chan<T> {}
unsafe impl<T: Send> Send for Chan<T> {}

pub struct Sender<T> {
    chan: Arc<Chan<T>>,
}

pub struct Receiver<T> {
    chan: Arc<Chan<T>>,
}

impl<T> Sender<T> {
    pub fn send(&self, data: T) -> Result<(), T> {
        self.chan.set(data)
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.chan.sender_rc.fetch_add(1, Ordering::Relaxed);
        Self {
            chan: self.chan.clone(),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.chan.sender_rc.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.chan.notify.notify(1);
        }
    }
}

impl<T> Receiver<T> {
    pub async fn recv(&self) -> Option<T> {
        // fast path, don't setup the listener
        if self.chan.is_set() || self.chan.is_dropped() {
            return self.chan.take();
        }
        let listener = self.chan.notify.listen();
        // re-check that we didn't miss the notification before listener was setup
        if self.chan.is_set() || self.chan.is_dropped() {
            return self.chan.take();
        }

        listener.await;

        self.chan.take()
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
}
