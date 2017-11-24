//! Manage a group of threads that all have the same return type and can be join()ed as a unit.
//!
//! The implementation uses a mpsc channel internally so that children (spawned threads) can notify the parent
//! (owner of a `ThreadGroup`) that they are finished without the parent having to use a blocking
//! `std::thread::JoinHandle.join()` call.
//!
//! # Examples
//! ```
//! use std::thread::sleep;
//! use std::time::Duration;
//! use threadgroup::{JoinError, ThreadGroup};
//! // Initialize a group of threads returning `u32`.
//! let mut tg: ThreadGroup<u32> = ThreadGroup::new();
//! // Start a bunch of threads that'll return or panic after a while
//! tg.spawn::<_,u32>(|| {sleep(Duration::new(0,30000));2});
//! tg.spawn::<_,u32>(|| {sleep(Duration::new(0,15000));panic!()});
//! tg.spawn::<_,u32>(|| {sleep(Duration::new(10000,0));3});
//! tg.spawn::<_,u32>(|| {sleep(Duration::new(0,10000));1});
//! // Join them in the order they finished
//! asser_eq!(1,                   tg.join().unwrap());
//! asser_eq!(JoinError::Panicked, tg.join().unwrap_err());
//! asser_eq!(2,                   tg.join().unwrap());
//! asser_eq!(JoinError::Timeout,  tg.join_timeout().unwrap_err());
//! ```

use std::panic;
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, RecvError, RecvTimeoutError, Sender};
use std::thread;
use std::thread::{JoinHandle, ThreadId};
use std::time::Duration;

/// Possible error returns from `join()` and `join_timeout()`.
#[derive(Debug,PartialEq)]
pub enum JoinError {
    /// Thread list is empty, nothing to join.
    AllDone,
    /// Joined thread has panicked, no result available.
    Panicked,//FIXME: include panic::PanicInfo
    /// No thread has finished yet.
    Timeout,
    /// Internal channel got disconnected (should not happen, only included for completeness).
    Disconnected,
}

/// Holds the vector of threads and the notification channel.
/// All public functions operate on this struct.
pub struct ThreadGroup<T> {
    tx: Sender<ThreadId>,
    rx: Receiver<ThreadId>,
    handles: Vec<JoinHandle<T>>,
}

// TODO: Allow passing something during spawn() that'll be returned during join()
// TODO: check threads.len() on drop()
// TODO: join_all()
// TODO: iter() or into_iter()
impl<T> ThreadGroup<T> {
    /// Initialize a group of threads returning `T`.
    /// # Examples
    /// ```
    /// // spawning and joining require the struct to be mutable, and you'll need to provide type hints.
    /// let mut tg: ThreadGroup<u32> = ThreadGroup::new();
    /// ```
    pub fn new() -> ThreadGroup<T> {
        let (tx, rx): (Sender<ThreadId>, Receiver<ThreadId>) = mpsc::channel();
        ThreadGroup::<T>{tx: tx, rx: rx, handles: vec![]}
    }

    /// Spawn a new thread (like `std::thread::join()`) in this thread group.
    /// # Examples
    /// ```
    /// tg.spawn::<_,u32>(|| {sleep(Duration::new(0,1000000));1});
    /// ```
    // FIXME: Is there a way to remove the need to specify the type when calling spawn() ?
    //        The ThreadGroup has type T and we know that R is the same type, but the compiler doesn't see that.
    pub fn spawn<F, R>(&mut self, f: F)
        where
        F: FnOnce() -> T,
        F: Send + 'static,
        R: Send + 'static,
        T: Send + 'static,
    {
        let thread_tx = self.tx.clone();
        let jh: JoinHandle<T> = thread::spawn(move || {
            // FIXME: Playing with fire here, probably not in a safe way: no idea if f is actually
            //        UnwindSafe. On the other hand, we're only using this so we can send() and then
            //        resume panicing, so I guess problems won't propagate.
            //        https://doc.rust-lang.org/stable/std/panic/trait.UnwindSafe.html
            let ret = panic::catch_unwind(panic::AssertUnwindSafe(f));
            thread_tx.send(thread::current().id()).unwrap();
            match ret {
                Ok(r) => r,
                Err(e) => panic::resume_unwind(e),
            }
        });
        self.handles.push(jh);
    }

    /// Return count of threads that have been `spawn()`ed but not yet `join()`ed.
    pub fn len(&self) -> usize {
        self.handles.len()
    }

    /// Check if there is any thread not yet `join()`ed.
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// Join one thread of the ThreadGroup.
    ///
    /// The thread will be joined in termination order, not creation order.
    /// # Examples
    /// ```
    /// while !tg.empty() {
    ///     match tg.join() {
    ///         Ok(ret) => println!("Thread returned {}", ret),
    ///         Err(e) => panic!("Oh noes !"),
    ///     }
    /// }
    /// ```
    pub fn join(&mut self) -> Result<T, JoinError> {
        match self.handles.is_empty() {
            true => Err(JoinError::AllDone),
            false => match self.rx.recv() {
                Ok(id) => self.do_join(id),
                Err(RecvError{}) => Err(JoinError::Disconnected)
            }
        }
    }

    /// Try to join one thread of the ThreadGroup, and give up after a timeout.
    /// # Examples
    /// ```
    /// loop {
    ///    if let Err(JoinError::Timeout) = tg.join_timeout(Duration::new(5,0)) {
    ///        println!("Still working...");
    ///    }
    /// }
    /// ```
    pub fn join_timeout(&mut self, timeout: Duration) -> Result<T, JoinError> {
        match self.handles.is_empty() {
            true => Err(JoinError::AllDone),
            false => match self.rx.recv_timeout(timeout) {
                Ok(id) => self.do_join(id),
                Err(RecvTimeoutError::Timeout) => Err(JoinError::Timeout),
                Err(RecvTimeoutError::Disconnected) => Err(JoinError::Disconnected)
            }
        }
    }

    /// Find a thread by its id
    // TODO: replace with https://doc.rust-lang.org/nightly/std/vec/struct.Vec.html#method.remove_item
    fn find(&self, id: ThreadId) -> Option<usize> {
        for (i,jh) in self.handles.iter().enumerate() {
            if jh.thread().id() == id {
                return Some(i)
            }
        }
        None
    }

    /// Actual thread::join() once we know that the thread has finished
    fn do_join(&mut self, id: ThreadId) -> Result<T, JoinError> {
        // We need to separately find and remove the JoinHandle from the vector in order to not upset the borrow checker
        let i = self.find(id).unwrap();
        match self.handles.remove(i).join() {
            Ok(ret) => Ok(ret),
            Err(_) => Err(JoinError::Panicked),
        }
    }
}

//FIXME: This is taken from the module example, but I couldn't get it to work from there
#[cfg(test)]
mod tests {
    use std::thread::sleep;
    use std::time::Duration;
    use threadgroup::{JoinError, ThreadGroup};

    #[test]
    fn empty_group() {
        let mut tg: ThreadGroup<u32> = ThreadGroup::new();
        assert!(tg.is_empty());
        assert_eq!(tg.len(), 0);
        assert_eq!(JoinError::AllDone, tg.join().unwrap_err());
    }
    #[test]
    fn basic_join() {
        let mut tg: ThreadGroup<u32> = ThreadGroup::new();
        tg.spawn::<_,u32>(|| {sleep(Duration::new(0,1000000));1});
        tg.spawn::<_,u32>(|| {sleep(Duration::new(0,3000000));3});
        tg.spawn::<_,u32>(|| {sleep(Duration::new(0,2000000));2});
        assert_eq!(1, tg.join().unwrap());
        assert_eq!(2, tg.join().unwrap());
        assert_eq!(3, tg.join().unwrap());
        assert_eq!(JoinError::AllDone, tg.join().unwrap_err());
    }
    #[test]
    fn panic_join() {
        let mut tg: ThreadGroup<u32> = ThreadGroup::new();
        tg.spawn::<_,u32>(|| {sleep(Duration::new(0,1500000));panic!()});
        tg.spawn::<_,u32>(|| {sleep(Duration::new(0,1000000));1});
        assert_eq!(1,                   tg.join().unwrap());
        assert_eq!(JoinError::Panicked, tg.join().unwrap_err());
        assert_eq!(JoinError::AllDone,  tg.join().unwrap_err());
    }
    #[test]
    fn timeout_join() {
        let mut tg: ThreadGroup<u32> = ThreadGroup::new();
        tg.spawn::<_,u32>(|| {sleep(Duration::new(1000000,0));2});
        tg.spawn::<_,u32>(|| {sleep(Duration::new(0,1000000));1});
        let t = Duration::new(1,0);
        assert_eq!(1,                  tg.join_timeout(t).unwrap());
        assert_eq!(JoinError::Timeout, tg.join_timeout(t).unwrap_err());
        assert!(!tg.is_empty());
        assert_eq!(tg.len(), 1);
    }
}
