use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::atomic::Ordering::SeqCst;
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use super::SendError;
use super::TrySendError;
use super::SendTimeoutError;
use super::RecvError;
use super::TryRecvError;
use super::RecvTimeoutError;

struct Blocked<T> {
    thread: Thread,
    data: UnsafeCell<Option<T>>,
    ready: AtomicBool,
}

impl<T> Blocked<T> {
    unsafe fn put(&self, t: T) {
        *self.data.get().as_mut().unwrap() = Some(t);
        self.signal();
    }

    unsafe fn take(&self) -> T {
        let value = self.data.get().as_mut().unwrap().take().unwrap();
        self.signal();
        value
    }

    fn signal(&self) {
        let thread = self.thread.clone();
        self.ready.store(true, SeqCst);
        thread.unpark();
    }
}

struct Inner<T> {
    senders: VecDeque<*const Blocked<T>>,
    receivers: VecDeque<*const Blocked<T>>,
}

pub struct Queue<T> {
    lock: Mutex<Inner<T>>,
    closed: AtomicBool,
    senders_len: AtomicUsize,
    receivers_len: AtomicUsize,
    _marker: PhantomData<T>,
}

unsafe impl<T: Send> Send for Queue<T> {}
unsafe impl<T: Send> Sync for Queue<T> {}

impl<T> Queue<T> {
    pub fn new() -> Self {
        Queue {
            lock: Mutex::new(Inner {
                senders: VecDeque::new(),
                receivers: VecDeque::new(),
            }),
            closed: AtomicBool::new(false),
            senders_len: AtomicUsize::new(0),
            receivers_len: AtomicUsize::new(0),
            _marker: PhantomData,
        }
    }

    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        if self.closed.load(SeqCst) {
            return Err(TrySendError::Disconnected(value));
        }

        if self.receivers_len.load(SeqCst) == 0 {
            return Err(TrySendError::Full(value));
        }

        let mut lock = self.lock.lock().unwrap();

        if self.closed.load(SeqCst) {
            return Err(TrySendError::Disconnected(value));
        }

        if let Some(f) = lock.receivers.pop_front() {
            unsafe { (*f).put(value); }
            self.receivers_len.store(lock.receivers.len(), SeqCst);
            Ok(())
        } else {
            Err(TrySendError::Full(value))
        }
    }

    fn send_until(
        &self,
        value: T,
        deadline: Option<Instant>,
    ) -> Result<(), SendTimeoutError<T>> {
        if self.closed.load(SeqCst) {
            return Err(SendTimeoutError::Disconnected(value));
        }

        let blocked;
        {
            let mut lock = self.lock.lock().unwrap();

            if self.closed.load(SeqCst) {
                return Err(SendTimeoutError::Disconnected(value));
            }

            if let Some(f) = lock.receivers.pop_front() {
                unsafe { (*f).put(value); }
                self.receivers_len.store(lock.receivers.len(), SeqCst);
                return Ok(())
            }

            blocked = Blocked {
                thread: thread::current(),
                data: UnsafeCell::new(Some(value)),
                ready: AtomicBool::new(false),
            };
            lock.senders.push_back(&blocked);
            self.senders_len.store(lock.senders.len(), SeqCst);
        }

        loop {
            if blocked.ready.load(SeqCst) {
                return Ok(());
            }

            if self.closed.load(SeqCst) {
                break;
            }

            if let Some(end) = deadline {
                let now = Instant::now();

                if now >= end {
                    break;
                } else {
                    thread::park_timeout(end - now);
                }
            } else {
                thread::park();
            }
        }

        let mut lock = self.lock.lock().unwrap();

        if blocked.ready.load(SeqCst) {
            Ok(())
        } else {
            lock.senders.retain(|&s| s != &blocked);
            self.senders_len.store(lock.senders.len(), SeqCst);

            let v = unsafe { blocked.take() };

            if self.closed.load(SeqCst) {
                Err(SendTimeoutError::Disconnected(v))
            } else {
                Err(SendTimeoutError::Timeout(v))
            }
        }
    }

    pub fn send_timeout(&self, value: T, dur: Duration) -> Result<(), SendTimeoutError<T>> {
        self.send_until(value, Some(Instant::now() + dur))
    }

    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        match self.send_until(value, None) {
            Ok(()) => Ok(()),
            Err(SendTimeoutError::Disconnected(v)) => Err(SendError(v)),
            Err(SendTimeoutError::Timeout(_)) => unreachable!(),
        }
    }

    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        if self.closed.load(SeqCst) {
            return Err(TryRecvError::Disconnected);
        }

        if self.senders_len.load(SeqCst) == 0 {
            return Err(TryRecvError::Empty);
        }

        let mut lock = self.lock.lock().unwrap();

        if self.closed.load(SeqCst) {
            return Err(TryRecvError::Disconnected);
        }

        if let Some(f) = lock.senders.pop_front() {
            self.senders_len.store(lock.senders.len(), SeqCst);
            unsafe { Ok((*f).take()) }
        } else {
            Err(TryRecvError::Empty)
        }
    }

    fn recv_until(&self, deadline: Option<Instant>) -> Result<T, RecvTimeoutError> {
        if self.closed.load(SeqCst) {
            return Err(RecvTimeoutError::Disconnected);
        }

        let blocked;
        {
            let mut lock = self.lock.lock().unwrap();

            if self.closed.load(SeqCst) {
                return Err(RecvTimeoutError::Disconnected);
            }

            if let Some(f) = lock.senders.pop_front() {
                self.senders_len.store(lock.senders.len(), SeqCst);
                unsafe { return Ok((*f).take()); }
            }

            blocked = Blocked {
                thread: thread::current(),
                data: UnsafeCell::new(None),
                ready: AtomicBool::new(false),
            };
            lock.receivers.push_back(&blocked);
            self.receivers_len.store(lock.receivers.len(), SeqCst);
        }

        loop {
            if blocked.ready.load(SeqCst) {
                unsafe { return Ok(blocked.take()); }
            }

            if self.closed.load(SeqCst) {
                break;
            }

            if let Some(end) = deadline {
                let now = Instant::now();

                if now >= end {
                    break;
                } else {
                    thread::park_timeout(end - now);
                }
            } else {
                thread::park();
            }
        }

        let mut lock = self.lock.lock().unwrap();

        if blocked.ready.load(SeqCst) {
            unsafe { Ok(blocked.take()) }
        } else {
            lock.receivers.retain(|&s| s != &blocked);
            self.receivers_len.store(lock.receivers.len(), SeqCst);

            if self.closed.load(SeqCst) {
                Err(RecvTimeoutError::Disconnected)
            } else {
                Err(RecvTimeoutError::Timeout)
            }
        }
    }

    pub fn recv_timeout(&self, dur: Duration) -> Result<T, RecvTimeoutError> {
        self.recv_until(Some(Instant::now() + dur))
    }

    pub fn recv(&self) -> Result<T, RecvError> {
        if let Ok(v) = self.recv_until(None) {
            Ok(v)
        } else {
            Err(RecvError)
        }
    }

    pub fn close(&self) -> bool {
        if self.closed.load(SeqCst) {
            return false;
        }

        let mut lock = self.lock.lock().unwrap();

        if self.closed.swap(true, SeqCst) {
            return false;
        }

        for t in lock.senders.drain(..) {
            unsafe { (*t).thread.unpark(); }
        }
        for t in lock.receivers.drain(..) {
            unsafe { (*t).thread.unpark(); }
        }

        self.receivers_len.store(lock.receivers.len(), SeqCst);
        self.senders_len.store(lock.senders.len(), SeqCst);

        true
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(SeqCst)
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        if cfg!(debug_assertions) {
            let mut lock = self.lock.get_mut().unwrap();
            debug_assert_eq!(lock.senders.len(), 0);
            debug_assert_eq!(lock.receivers.len(), 0);
            debug_assert_eq!(self.senders_len.load(SeqCst), 0);
            debug_assert_eq!(self.receivers_len.load(SeqCst), 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering::SeqCst;
    use std::thread;

    use crossbeam;

    use super::*;

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn smoke() {
        let q = Queue::new();
        assert_eq!(q.try_send(7), Err(TrySendError::Full(7)));
        assert_eq!(q.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn recv() {
        let q = Queue::new();

        crossbeam::scope(|s| {
            s.spawn(|| {
                assert_eq!(q.recv(), Ok(7));
                thread::sleep(ms(100));
                assert_eq!(q.recv(), Ok(8));
                thread::sleep(ms(100));
                assert_eq!(q.recv(), Ok(9));
                assert_eq!(q.recv(), Err(RecvError));
            });
            s.spawn(|| {
                thread::sleep(ms(150));
                assert_eq!(q.send(7), Ok(()));
                assert_eq!(q.send(8), Ok(()));
                assert_eq!(q.send(9), Ok(()));
                q.close();
            });
        });
    }

    #[test]
    fn recv_timeout() {
        let q = Queue::new();

        crossbeam::scope(|s| {
            s.spawn(|| {
                assert_eq!(q.recv_timeout(ms(100)), Err(RecvTimeoutError::Timeout));
                assert_eq!(q.recv_timeout(ms(100)), Ok(7));
                assert_eq!(q.recv_timeout(ms(100)), Err(RecvTimeoutError::Disconnected));
            });
            s.spawn(|| {
                thread::sleep(ms(150));
                assert_eq!(q.send(7), Ok(()));
                q.close();
            });
        });
    }

    #[test]
    fn try_recv() {
        let q = Queue::new();

        crossbeam::scope(|s| {
            s.spawn(|| {
                assert_eq!(q.try_recv(), Err(TryRecvError::Empty));
                thread::sleep(ms(150));
                assert_eq!(q.try_recv(), Ok(7));
                thread::sleep(ms(50));
                assert_eq!(q.try_recv(), Err(TryRecvError::Disconnected));
            });
            s.spawn(|| {
                thread::sleep(ms(100));
                assert_eq!(q.send(7), Ok(()));
                q.close();
            });
        });
    }

    #[test]
    fn send() {
        let q = Queue::new();

        crossbeam::scope(|s| {
            s.spawn(|| {
                assert_eq!(q.send(7), Ok(()));
                thread::sleep(ms(100));
                assert_eq!(q.send(8), Ok(()));
                thread::sleep(ms(100));
                assert_eq!(q.send(9), Ok(()));
                assert_eq!(q.send(10), Err(SendError(10)));
            });
            s.spawn(|| {
                thread::sleep(ms(150));
                assert_eq!(q.recv(), Ok(7));
                assert_eq!(q.recv(), Ok(8));
                assert_eq!(q.recv(), Ok(9));
                q.close();
            });
        });
    }

    #[test]
    fn send_timeout() {
        let q = Queue::new();

        crossbeam::scope(|s| {
            s.spawn(|| {
                assert_eq!(q.send_timeout(7, ms(100)), Err(SendTimeoutError::Timeout(7)));
                assert_eq!(q.send_timeout(8, ms(100)), Ok(()));
                assert_eq!(q.send_timeout(9, ms(100)), Err(SendTimeoutError::Disconnected(9)));
            });
            s.spawn(|| {
                thread::sleep(ms(150));
                assert_eq!(q.recv(), Ok(8));
                q.close();
            });
        });
    }

    #[test]
    fn try_send() {
        let q = Queue::new();

        crossbeam::scope(|s| {
            s.spawn(|| {
                assert_eq!(q.try_send(7), Err(TrySendError::Full(7)));
                thread::sleep(ms(150));
                assert_eq!(q.try_send(8), Ok(()));
                thread::sleep(ms(50));
                assert_eq!(q.try_send(9), Err(TrySendError::Disconnected(9)));
            });
            s.spawn(|| {
                thread::sleep(ms(100));
                assert_eq!(q.recv(), Ok(8));
                q.close();
            });
        });
    }

    #[test]
    fn is_closed() {
        let q = Queue::<()>::new();

        crossbeam::scope(|s| {
            s.spawn(|| {
                assert!(!q.is_closed());
                thread::sleep(ms(150));
                assert!(q.is_closed());
            });
            s.spawn(|| {
                thread::sleep(ms(100));
                assert!(!q.is_closed());
                q.close();
                assert!(q.is_closed());
            });
        });
    }

    #[test]
    fn close_signals_sender() {
        let q = Queue::new();

        crossbeam::scope(|s| {
            s.spawn(|| {
                assert_eq!(q.send(()), Err(SendError(())));
                assert!(q.is_closed());
            });
            s.spawn(|| {
                thread::sleep(ms(100));
                q.close();
            });
        });
    }

    #[test]
    fn close_signals_receiver() {
        let q = Queue::<()>::new();

        crossbeam::scope(|s| {
            s.spawn(|| {
                assert_eq!(q.recv(), Err(RecvError));
                assert!(q.is_closed());
            });
            s.spawn(|| {
                thread::sleep(ms(100));
                q.close();
            });
        });
    }

    #[test]
    fn spsc() {
        const COUNT: usize = 100_000;

        let q = Queue::new();

        crossbeam::scope(|s| {
            s.spawn(|| {
                for i in 0..COUNT {
                    assert_eq!(q.recv(), Ok(i));
                }
                assert_eq!(q.recv(), Err(RecvError));
            });
            s.spawn(|| {
                for i in 0..COUNT {
                    q.send(i).unwrap();
                }
                q.close();
            });
        });
    }

    #[test]
    fn mpmc() {
        const COUNT: usize = 25_000;
        const THREADS: usize = 4;

        let q = Queue::<usize>::new();
        let v = (0..COUNT).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>();

        crossbeam::scope(|s| {
            for _ in 0..THREADS {
                s.spawn(|| {
                    for i in 0..COUNT {
                        let n = q.recv().unwrap();
                        v[n].fetch_add(1, SeqCst);
                    }
                });
            }
            for _ in 0..THREADS {
                s.spawn(|| {
                    for i in 0..COUNT {
                        q.send(i).unwrap();
                    }
                });
            }
        });

        for c in v {
            assert_eq!(c.load(SeqCst), THREADS);
        }
    }
}