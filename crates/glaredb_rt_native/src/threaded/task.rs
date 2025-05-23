use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use glaredb_core::execution::partition_pipeline::ExecutablePartitionPipeline;
use glaredb_core::runtime::pipeline::ErrorSink;
use glaredb_core::runtime::profile_buffer::ProfileSink;
use glaredb_error::DbError;
use parking_lot::Mutex;
use rayon::ThreadPool;

use crate::time::NativeInstant;

/// State shared by the partition pipeline task and the waker.
#[derive(Debug)]
pub(crate) struct TaskState {
    /// The partition pipeline we're operating on alongside a boolean for if the
    /// query's been canceled.
    pub(crate) pipeline: Mutex<PipelineState>,
    /// Error sink for any errors that occur during execution.
    pub(crate) errors: Arc<dyn ErrorSink>,
    /// The threadpool to execute on.
    pub(crate) pool: Arc<ThreadPool>,
    /// Where to put the profile when this pipeline completes.
    pub(crate) profile_sink: ProfileSink,
}

#[derive(Debug)]
pub(crate) struct PipelineState {
    pub(crate) pipeline: ExecutablePartitionPipeline,
    pub(crate) query_canceled: bool,
}

/// Task for executing a partition pipeline.
pub struct PartitionPipelineTask {
    state: Arc<TaskState>,
}

impl PartitionPipelineTask {
    pub(crate) fn from_task_state(state: Arc<TaskState>) -> Self {
        PartitionPipelineTask { state }
    }

    pub(crate) fn execute(self) {
        let mut pipeline_state = self.state.pipeline.lock();

        if pipeline_state.query_canceled {
            self.state.errors.set_error(DbError::new("Query canceled"));
            return;
        }

        let waker: Waker = Arc::new(PartitionPipelineWaker {
            state: self.state.clone(),
        })
        .into();

        let mut cx = Context::from_waker(&waker);
        match pipeline_state
            .pipeline
            .poll_execute::<NativeInstant>(&mut cx)
        {
            Poll::Ready(Ok(prof)) => {
                // Pushing through the pipeline was successful. Put our profile.
                // We'll never execute again.
                self.state.profile_sink.put(prof);
            }
            Poll::Ready(Err(e)) => {
                self.state.errors.set_error(e);
            }
            Poll::Pending => {
                // Exit the loop. Waker was already stored in the pending
                // sink/source, we'll be woken back up when there's more
                // this operator chain can start executing.
            }
        }
    }
}

/// A waker implementation that will re-execute the pipeline once woken.
struct PartitionPipelineWaker {
    state: Arc<TaskState>,
}

impl Wake for PartitionPipelineWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref()
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let pool = self.state.pool.clone();
        let task = PartitionPipelineTask {
            state: self.state.clone(),
        };
        pool.spawn(|| task.execute());
    }
}
