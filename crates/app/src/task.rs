//! Background work: futures run on a tokio runtime and hand their results back to the UI
//! thread, which polls them once per frame (no locking in the UI).

use std::future::Future;

use anyhow::{Result, anyhow};
use tokio::sync::oneshot;

/// A pending result from a background future.
pub struct Task<T> {
    rx: oneshot::Receiver<Result<T>>,
}

impl<T> Task<T> {
    /// Returns the result once ready; afterwards the task must be dropped.
    pub fn poll(&mut self) -> Option<Result<T>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => Some(Err(anyhow!("background task was cancelled"))),
        }
    }
}

/// Polls an optional task in place, clearing it when it finishes.
pub fn poll_slot<T>(slot: &mut Option<Task<T>>) -> Option<Result<T>> {
    let out = slot.as_mut()?.poll();
    if out.is_some() {
        *slot = None;
    }
    out
}

/// Spawns futures and wakes the UI when they complete.
#[derive(Clone)]
pub struct Spawner {
    rt: tokio::runtime::Handle,
    ctx: eframe::egui::Context,
}

impl Spawner {
    pub fn new(rt: tokio::runtime::Handle, ctx: eframe::egui::Context) -> Self {
        Self { rt, ctx }
    }

    pub fn spawn<T, F>(&self, fut: F) -> Task<T>
    where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let ctx = self.ctx.clone();
        self.rt.spawn(async move {
            let out = fut.await;
            let _ = tx.send(out);
            ctx.request_repaint();
        });
        Task { rx }
    }

    /// Fire-and-forget (e.g. cancellation requests).
    pub fn detach<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.rt.spawn(fut);
    }
}
