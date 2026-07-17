//! One-shot completion slot: the correlation point between a pending
//! request and the receive loop.
//!
//! Hand-rolled rather than a channel crate because the consumer side needs
//! two waiting modes the std/tokio primitives do not offer together without
//! pulling an async runtime into this crate: a *bounded* blocking wait
//! (tests, synchronous callers — an unbounded block would violate the
//! no-indefinite-blocking rule) and `Future` polling (the async
//! `DriveSource` adapter to come). The producer side is the receive loop,
//! which completes the slot exactly once; completion after the first is
//! reported so the runtime can count it as a discarded duplicate.
//!
//! Waker discipline: the waker is stored under the same mutex as the value
//! and always woken *after* the lock is released, so a waker that
//! re-enters the runtime (drops a request, takes the state lock) cannot
//! deadlock against the slot.

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

pub(crate) struct Slot<T> {
    state: Mutex<SlotState<T>>,
    filled: Condvar,
}

struct SlotState<T> {
    value: Option<T>,
    complete: bool,
    waker: Option<Waker>,
}

impl<T> Slot<T> {
    pub(crate) fn new() -> Slot<T> {
        Slot {
            state: Mutex::new(SlotState {
                value: None,
                complete: false,
                waker: None,
            }),
            filled: Condvar::new(),
        }
    }

    // Poison recovery: a panic in some other thread while holding this lock
    // cannot break the slot's invariants (they are a value and two flags),
    // so waiting sides keep working rather than propagating the panic.
    fn lock(&self) -> MutexGuard<'_, SlotState<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Complete the slot. Returns `false` — with the value dropped — when
    /// the slot was already completed; the first completion wins.
    pub(crate) fn complete(&self, value: T) -> bool {
        let waker = {
            let mut state = self.lock();
            if state.complete {
                return false;
            }
            state.value = Some(value);
            state.complete = true;
            state.waker.take()
        };
        self.filled.notify_all();
        if let Some(waker) = waker {
            waker.wake();
        }
        true
    }

    /// Block until completed or `timeout` elapses. `None` on timeout (or if
    /// the value was already taken — unreachable through `PendingRequest`,
    /// which consumes itself on the taking paths).
    pub(crate) fn wait_timeout(&self, timeout: Duration) -> Option<T> {
        let deadline = Instant::now().checked_add(timeout);
        let mut state = self.lock();
        loop {
            if state.complete {
                return state.value.take();
            }
            let now = Instant::now();
            let remaining = match deadline {
                // A timeout too large to represent is an unbounded wait the
                // caller cannot have meant; treat it as "very long".
                None => Duration::MAX,
                Some(deadline) => {
                    if now >= deadline {
                        return None;
                    }
                    deadline - now
                }
            };
            let (guard, _timed_out) = self
                .filled
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner);
            state = guard;
        }
    }

    /// Poll for completion, parking the task's waker when still pending.
    /// Each poll replaces the stored waker — only the most recent poller is
    /// woken, matching single-consumer use.
    pub(crate) fn poll_take(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut state = self.lock();
        if state.complete {
            Poll::Ready(state.value.take())
        } else {
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::task::Wake;
    use std::thread;

    struct FlagWaker(std::sync::atomic::AtomicBool);

    impl Wake for FlagWaker {
        fn wake(self: Arc<Self>) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn completes_once_and_reports_duplicates() {
        let slot = Slot::new();
        assert!(slot.complete(1));
        assert!(!slot.complete(2));
        assert_eq!(slot.wait_timeout(Duration::ZERO), Some(1));
    }

    #[test]
    fn wait_times_out_then_succeeds_after_completion() {
        let slot = Arc::new(Slot::new());
        assert_eq!(slot.wait_timeout(Duration::from_millis(10)), None);

        let producer = Arc::clone(&slot);
        let handle = thread::spawn(move || producer.complete(7));
        assert_eq!(slot.wait_timeout(Duration::from_secs(5)), Some(7));
        assert!(handle.join().unwrap());
    }

    #[test]
    fn poll_registers_waker_and_wakes_on_complete() {
        let slot: Slot<u32> = Slot::new();
        let flag = Arc::new(FlagWaker(std::sync::atomic::AtomicBool::new(false)));
        let waker = Waker::from(Arc::clone(&flag));
        let mut cx = Context::from_waker(&waker);

        assert!(slot.poll_take(&mut cx).is_pending());
        assert!(slot.complete(9));
        assert!(flag.0.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(slot.poll_take(&mut cx), Poll::Ready(Some(9)));
        // Taken once; a second take observes completion with no value.
        assert_eq!(slot.poll_take(&mut cx), Poll::Ready(None));
    }
}
