use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStage {
    Extract,
    Layout,
    Translate,
    Typeset,
    Compose,
}

#[derive(Debug, Clone)]
pub enum ProgressEvent {
    Stage(JobStage),
    Layout {
        pages: usize,
        lines: usize,
        paragraphs: usize,
        protected: usize,
    },
    Translation {
        completed: usize,
        total: usize,
    },
}

#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    #[cfg(any(feature = "gui", test))]
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            bail!("translation cancelled")
        }
        Ok(())
    }
}

pub type ProgressSink = dyn Fn(ProgressEvent) + Sync;
