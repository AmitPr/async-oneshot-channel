use std::{
    cell::Cell,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, Once},
    task::{Context, Poll, Waker},
};

#[derive(Clone)]
pub struct NotifyOnce {
    inner: Arc<Mutex<Inner>>,
    listening: Arc<Once>,
}

struct Inner {
    waker: Cell<Option<Waker>>,
    once: Once,
}

pub struct Listen {
    notify: NotifyOnce,
}

impl NotifyOnce {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                waker: Cell::new(None),
                once: Once::new(),
            })),
            listening: Arc::new(Once::new()),
        }
    }

    pub fn listen(&self) -> Option<Listen> {
        let mut called = false;
        self.listening.call_once(|| {
            called = true;
        });
        if called {
            Some(Listen {
                notify: self.clone(),
            })
        } else {
            None
        }
    }

    pub fn notify(&self) {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.once.call_once(|| {});
        if let Some(waker) = inner.waker.take() {
            waker.wake();
        }
    }
}

impl Future for Listen {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self.notify.inner.lock().unwrap_or_else(|e| e.into_inner());
        // fast path
        if inner.once.is_completed() {
            return Poll::Ready(());
        }

        inner.waker.replace(Some(cx.waker().clone()));

        if inner.once.is_completed() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn test_notify_once() {
        let notify = NotifyOnce::new();

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let notify2 = notify.clone();
        let jh = tokio::spawn(async move {
            let listener = notify2.listen().unwrap();
            listener.await;
            counter_clone.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(counter.load(Ordering::SeqCst), 0);
        notify.notify();
        jh.await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}
