//! Task management.
//!
//! This file aims to abstract a lot of potentially annoying Rust async logic, which may be
//! subject to change.

use futures_util::{FutureExt, TryFutureExt};
use std::{future::Future, pin::Pin, task::Poll};
use tokio::{sync::mpsc, task::JoinHandle};

/// Handles all of the processes within lowfi.
/// This entails initializing/closing tasks, and handling any potential errors that arise.
pub struct Tasks {
    /// A simple [`Vec`] of [`JoinHandle`]s.
    pub handles: Vec<JoinHandle<crate::Result<()>>>,

    /// A sender, which is kept for convenience to be used when
    /// initializing various other tasks.
    tx: mpsc::Sender<crate::Message>,
}

impl Tasks {
    /// Creates a new task manager.
    pub const fn new(tx: mpsc::Sender<crate::Message>) -> Self {
        Self {
            tx,
            handles: Vec::new(),
        }
    }

    /// Processes a task, and adds it to the internal buffer.
    pub fn spawn<E: Into<crate::Error> + Send + Sync + 'static>(
        &mut self,
        future: impl Future<Output = Result<(), E>> + Send + 'static,
    ) {
        self.handles.push(tokio::spawn(future.map_err(Into::into)));
    }

    /// Gets a copy of the internal [`mpsc::Sender`].
    pub fn tx(&self) -> mpsc::Sender<crate::Message> {
        self.tx.clone()
    }
}

impl Future for Tasks {
    type Output = crate::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        for handle in &mut self.get_mut().handles {
            match handle.poll_unpin(cx) {
                Poll::Ready(Ok(x)) => return Poll::Ready(x),
                Poll::Ready(Err(x)) => return Poll::Ready(Err(crate::Error::JoinError(x))),
                Poll::Pending => (),
            }
        }

        Poll::Pending
    }
}
