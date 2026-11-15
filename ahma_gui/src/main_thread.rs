//! The process's real main thread, for the one thing that must run there
//! (SPEC R-DESK.3).
//!
//! `ahma` runs its async runtime on a worker thread with a large stack
//! (`ahma_bin`'s `main`). A native window cannot follow it there: macOS (and
//! in practice every platform's toolkit) runs a window's event loop on the
//! thread the process started on. So that thread waits here for work: the
//! runtime hands it a task (the window), or says it is done.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Mutex, OnceLock};

/// Work for the main thread.
pub type Task = Box<dyn FnOnce() + Send>;

/// What the main thread is told.
pub enum Message {
    /// Run this, then wait for the next message.
    Run(Task),
    /// The runtime has finished: stop waiting.
    Done,
}

static MAIN: OnceLock<Mutex<Sender<Message>>> = OnceLock::new();

/// Make `sender` the way to reach the main thread. Called once, by `main`.
pub fn install(sender: Sender<Message>) {
    let _ = MAIN.set(Mutex::new(sender));
}

/// Run `task` on the main thread. Fails when nothing serves the main thread
/// (a binary that never called [`install`], or a test).
pub fn run(task: Task) -> anyhow::Result<()> {
    let main = MAIN
        .get()
        .ok_or_else(|| anyhow::anyhow!("this program does not hand its main thread to a window"))?;
    main.lock()
        .unwrap_or_else(|e| e.into_inner())
        .send(Message::Run(task))
        .map_err(|_| anyhow::anyhow!("the main thread is no longer waiting for work"))
}

/// Serve the main thread: run each task as it arrives, until told the runtime
/// is done or every sender has gone.
pub fn serve(receiver: Receiver<Message>) {
    while let Ok(Message::Run(task)) = receiver.recv() {
        task();
    }
}

/// Tells the main thread the runtime is done when dropped, including during a
/// panic's unwinding, so [`serve`] never waits for a runtime that has gone.
pub struct DoneOnDrop(pub Sender<Message>);

impl Drop for DoneOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(Message::Done);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Tasks run in order, on the serving thread, and `Done` ends it.
    #[test]
    fn tasks_run_in_order_until_done() {
        let (tx, rx) = std::sync::mpsc::channel();
        let seen = Arc::new(Mutex::new(Vec::new()));
        for i in 0..3 {
            let seen = seen.clone();
            tx.send(Message::Run(Box::new(move || seen.lock().unwrap().push(i))))
                .unwrap();
        }
        tx.send(Message::Done).unwrap();
        let ran_after_done = Arc::new(AtomicUsize::new(0));
        let late = ran_after_done.clone();
        tx.send(Message::Run(Box::new(move || {
            late.fetch_add(1, Ordering::SeqCst);
        })))
        .unwrap();
        serve(rx);
        assert_eq!(*seen.lock().unwrap(), [0, 1, 2]);
        assert_eq!(ran_after_done.load(Ordering::SeqCst), 0, "Done ends it");
    }

    /// A runtime that panics still releases the main thread.
    #[test]
    fn a_panicking_runtime_still_says_done() {
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _done = DoneOnDrop(tx);
            panic!("the runtime failed");
        });
        serve(rx);
        assert!(worker.join().is_err());
    }
}
