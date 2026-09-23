use std::time::Duration;

use crossbeam_channel::RecvError;
use crossbeam_channel::RecvTimeoutError;
use crossbeam_channel::Sender;
use rustc_hash::FxHashSet;
use starpls_common::File;

use crate::event_loop::Task;

pub(crate) struct AnalysisDebouncer {
    pub(crate) sender: Sender<Vec<File>>,
}

impl AnalysisDebouncer {
    pub(crate) fn new(duration: Duration, sink: Sender<Task>) -> Self {
        let (source_tx, source_rx) = crossbeam_channel::unbounded::<Vec<File>>();

        std::thread::spawn(move || {
            let mut active = false;
            let mut pending_file_ids: FxHashSet<File> = FxHashSet::default();
            loop {
                if active {
                    match source_rx.recv_timeout(duration) {
                        Ok(file_ids) => pending_file_ids.extend(file_ids),
                        Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {
                            if sink
                                .send(Task::AnalysisRequested(pending_file_ids.drain().collect()))
                                .is_err()
                            {
                                break;
                            }
                            active = false;
                        }
                    }
                } else {
                    match source_rx.recv() {
                        Ok(file_ids) => {
                            active = true;
                            pending_file_ids.extend(file_ids);
                        }
                        Err(RecvError) => break,
                    }
                }
            }
        });

        Self { sender: source_tx }
    }
}
