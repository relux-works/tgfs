//! Bounded update queue with shutdown-aware backpressure.
//!
//! One queue per client carries its updates from the receive loop to the
//! consumer. Hand-rolled over `Mutex`/`Condvar` because the runtime needs
//! one property std's `sync_channel` cannot give it: a *third party* — the
//! shutdown path, which owns neither endpoint — must be able to `close` the
//! queue and thereby wake a producer blocked on a full buffer. Without
//! that, shutdown would deadlock: the receive loop blocks in `push`, and
//! the only thing that could unblock it (dropping the consumer) sits
//! behind a `join` of the very thread that is blocked.
//!
//! Semantics:
//! - `push_blocking` applies backpressure while the buffer is full, and
//!   returns immediately with the reason when the queue is closed
//!   (shutdown / client closed) or the receiver was dropped.
//! - `close` stops intake but never discards: items already buffered stay
//!   receivable, so a consumer drains what was delivered before the close
//!   and then sees the end of the stream.
//! - Dropping the receiver marks it disconnected, waking blocked producers;
//!   subsequent pushes fail fast and the runtime counts them.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

pub(crate) struct Queue<T> {
    state: Mutex<QueueState<T>>,
    not_full: Condvar,
    not_empty: Condvar,
}

struct QueueState<T> {
    items: VecDeque<T>,
    capacity: usize,
    closed: bool,
    receiver_alive: bool,
}

/// Why a push did not enqueue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PushError {
    /// Buffer full (only `try_push` reports this; `push_blocking` waits).
    Full,
    /// The queue was closed (shutdown or client closed).
    Closed,
    /// The consumer dropped its stream; nobody would ever receive the item.
    Disconnected,
}

/// Why a receive returned no item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecvError {
    /// Nothing arrived within the wait (queue still open).
    Timeout,
    /// The queue is closed and fully drained; no item will ever arrive.
    Closed,
}

impl<T> Queue<T> {
    /// A queue holding at most `capacity` items (raised to 1 — a zero
    /// buffer could never accept a push and would wedge the receive loop).
    pub(crate) fn new(capacity: usize) -> Queue<T> {
        Queue {
            state: Mutex::new(QueueState {
                items: VecDeque::new(),
                capacity: capacity.max(1),
                closed: false,
                receiver_alive: true,
            }),
            not_full: Condvar::new(),
            not_empty: Condvar::new(),
        }
    }

    // Poison recovery as in `Slot::lock`: the invariants are a deque and
    // two flags, all valid at every await point, so a panic elsewhere must
    // not wedge shutdown.
    fn lock(&self) -> MutexGuard<'_, QueueState<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Enqueue, blocking while the buffer is full. Wakes and reports when
    /// the queue is closed or the receiver disappears mid-wait.
    pub(crate) fn push_blocking(&self, item: T) -> Result<(), PushError> {
        let mut state = self.lock();
        loop {
            if state.closed {
                return Err(PushError::Closed);
            }
            if !state.receiver_alive {
                return Err(PushError::Disconnected);
            }
            if state.items.len() < state.capacity {
                state.items.push_back(item);
                drop(state);
                self.not_empty.notify_all();
                return Ok(());
            }
            state = self
                .not_full
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Enqueue without blocking — the shutdown drain uses this so a full
    /// buffer can never stall the exit path.
    pub(crate) fn try_push(&self, item: T) -> Result<(), PushError> {
        let mut state = self.lock();
        if state.closed {
            return Err(PushError::Closed);
        }
        if !state.receiver_alive {
            return Err(PushError::Disconnected);
        }
        if state.items.len() >= state.capacity {
            return Err(PushError::Full);
        }
        state.items.push_back(item);
        drop(state);
        self.not_empty.notify_all();
        Ok(())
    }

    /// Dequeue, waiting up to `timeout`. Buffered items win over the closed
    /// flag, so a consumer always drains what was delivered before a close.
    pub(crate) fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvError> {
        let deadline = Instant::now().checked_add(timeout);
        let mut state = self.lock();
        loop {
            if let Some(item) = state.items.pop_front() {
                drop(state);
                self.not_full.notify_all();
                return Ok(item);
            }
            if state.closed {
                return Err(RecvError::Closed);
            }
            let now = Instant::now();
            let remaining = match deadline {
                None => Duration::MAX,
                Some(deadline) => {
                    if now >= deadline {
                        return Err(RecvError::Timeout);
                    }
                    deadline - now
                }
            };
            let (guard, _timed_out) = self
                .not_empty
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner);
            state = guard;
        }
    }

    /// Dequeue without waiting.
    pub(crate) fn try_recv(&self) -> Result<T, RecvError> {
        let mut state = self.lock();
        if let Some(item) = state.items.pop_front() {
            drop(state);
            self.not_full.notify_all();
            return Ok(item);
        }
        if state.closed {
            Err(RecvError::Closed)
        } else {
            Err(RecvError::Timeout)
        }
    }

    /// Stop intake and wake everyone. Buffered items remain receivable;
    /// idempotent.
    pub(crate) fn close(&self) {
        {
            let mut state = self.lock();
            state.closed = true;
        }
        self.not_full.notify_all();
        self.not_empty.notify_all();
    }

    /// Mark the consumer gone (its stream was dropped), waking any blocked
    /// producer so backpressure against a dead consumer cannot hold the
    /// receive loop.
    pub(crate) fn disconnect_receiver(&self) {
        {
            let mut state = self.lock();
            state.receiver_alive = false;
        }
        self.not_full.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    const GUARD: Duration = Duration::from_secs(5);

    #[test]
    fn delivers_in_order_within_capacity() {
        let queue = Queue::new(4);
        for n in 0..4 {
            queue.push_blocking(n).unwrap();
        }
        for n in 0..4 {
            assert_eq!(queue.recv_timeout(GUARD).unwrap(), n);
        }
        assert_eq!(queue.try_recv(), Err(RecvError::Timeout));
    }

    #[test]
    fn full_buffer_applies_backpressure_until_drained() {
        let queue = Arc::new(Queue::new(1));
        queue.push_blocking(1).unwrap();

        let producer = Arc::clone(&queue);
        let handle = thread::spawn(move || producer.push_blocking(2));

        assert_eq!(queue.recv_timeout(GUARD).unwrap(), 1);
        handle.join().unwrap().unwrap();
        assert_eq!(queue.recv_timeout(GUARD).unwrap(), 2);
    }

    #[test]
    fn close_wakes_blocked_producer_and_preserves_buffered_items() {
        let queue = Arc::new(Queue::new(1));
        queue.push_blocking(1).unwrap();

        let producer = Arc::clone(&queue);
        let handle = thread::spawn(move || producer.push_blocking(2));

        queue.close();
        assert_eq!(handle.join().unwrap(), Err(PushError::Closed));
        // The buffered item survives the close; then the stream ends.
        assert_eq!(queue.recv_timeout(GUARD).unwrap(), 1);
        assert_eq!(queue.recv_timeout(Duration::ZERO), Err(RecvError::Closed));
    }

    #[test]
    fn receiver_disconnect_wakes_blocked_producer() {
        let queue = Arc::new(Queue::new(1));
        queue.push_blocking(1).unwrap();

        let producer = Arc::clone(&queue);
        let handle = thread::spawn(move || producer.push_blocking(2));

        queue.disconnect_receiver();
        assert_eq!(handle.join().unwrap(), Err(PushError::Disconnected));
        assert_eq!(queue.try_push(3), Err(PushError::Disconnected));
    }

    #[test]
    fn try_push_reports_full_without_blocking() {
        let queue = Queue::new(1);
        queue.try_push(1).unwrap();
        assert_eq!(queue.try_push(2), Err(PushError::Full));
    }
}
