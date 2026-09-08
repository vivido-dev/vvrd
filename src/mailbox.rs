//! Bounded, nonblocking submission with coalescing only inside ordering barriers.
use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub trait Coalesce {
    fn replaces(&self, previous: &Self) -> bool;
    /// Discard obsolete display results without moving their replacements before control barriers.
    fn discard_obsolete(&self) -> bool {
        false
    }
}

struct State<T> {
    queue: VecDeque<T>,
    closed: bool,
}
pub struct Sender<T> {
    state: Arc<Mutex<State<T>>>,
    wake: flume::Sender<()>,
}
pub struct Receiver<T> {
    state: Arc<Mutex<State<T>>>,
    wake: flume::Receiver<()>,
}

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let state = Arc::new(Mutex::new(State {
        queue: VecDeque::new(),
        closed: false,
    }));
    let (wake, receive) = flume::bounded(1);
    (
        Sender {
            state: state.clone(),
            wake,
        },
        Receiver {
            state,
            wake: receive,
        },
    )
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            wake: self.wake.clone(),
        }
    }
}

impl<T: Coalesce> Sender<T> {
    pub fn send(&self, value: T) -> io::Result<()> {
        self.send_recover(value).map_err(|(error, _)| error)
    }
    pub fn send_recover(&self, value: T) -> Result<(), (io::Error, T)> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return Err((
                io::Error::new(io::ErrorKind::BrokenPipe, "worker stopped"),
                value,
            ));
        }
        if value.discard_obsolete() {
            state.queue.retain(|old| !value.replaces(old));
        }
        if state.queue.back().is_some_and(|old| value.replaces(old)) {
            state.queue.pop_back();
        } else if state.queue.len() == 32 || state.queue.iter().any(|old| value.replaces(old)) {
            return Err((
                io::Error::new(io::ErrorKind::WouldBlock, "worker busy; retry the command"),
                value,
            ));
        }
        state.queue.push_back(value);
        let _ = self.wake.try_send(());
        Ok(())
    }
}

impl<T> Sender<T> {
    pub fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        state.queue.clear();
        let _ = self.wake.try_send(());
    }
}

impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Result<T, flume::TryRecvError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .queue
            .pop_front()
            .ok_or(if state.closed || self.wake.is_disconnected() {
                flume::TryRecvError::Disconnected
            } else {
                flume::TryRecvError::Empty
            })
    }
    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, flume::RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.try_recv() {
                Ok(value) => return Ok(value),
                Err(flume::TryRecvError::Disconnected) => {
                    return Err(flume::RecvTimeoutError::Disconnected);
                }
                Err(flume::TryRecvError::Empty) => {}
            }
            self.wake
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
        }
    }
    pub fn recv(&self) -> Result<T, flume::RecvError> {
        loop {
            match self.recv_timeout(Duration::from_secs(60)) {
                Ok(value) => return Ok(value),
                Err(flume::RecvTimeoutError::Disconnected) => {
                    return Err(flume::RecvError::Disconnected);
                }
                Err(flume::RecvTimeoutError::Timeout) => {}
            }
        }
    }
    pub fn is_empty(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queue
            .is_empty()
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        state.queue.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Item(u32, bool);
    impl Coalesce for Item {
        fn replaces(&self, old: &Self) -> bool {
            self.1 && old.1
        }
    }
    #[test]
    fn bounded_queue_keeps_barriers_and_closes_without_capacity() {
        let (tx, rx) = channel();
        tx.send(Item(1, true)).unwrap();
        tx.send(Item(2, true)).unwrap();
        tx.send(Item(3, false)).unwrap();
        assert_eq!(
            tx.send(Item(4, true)).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(rx.recv().unwrap().0, 2);
        tx.send(Item(4, true)).unwrap();
        assert_eq!(rx.recv().unwrap().0, 3);
        assert_eq!(rx.recv().unwrap().0, 4);
        for n in 0..32 {
            tx.send(Item(n, false)).unwrap();
        }
        assert_eq!(
            tx.send(Item(33, false)).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        tx.close();
        assert!(matches!(rx.recv(), Err(flume::RecvError::Disconnected)));
    }
}
