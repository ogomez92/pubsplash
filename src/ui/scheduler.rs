//! The live media schedulers: one worker per Media Scheduler source in the
//! active scene.
//!
//! The same shape and the same reasons as [`super::media`] — a worker is keyed
//! by `SourceConfig.name`, the identity key `ExternalFeeds` routes on, and is
//! started and retired from `home::on_sources_changed`, because that is the
//! moment the rings appear and disappear. Only the active scene's schedulers
//! run: a scheduler in a scene nobody is on has no ring to feed, and firing
//! into one would be audible nowhere.
//!
//! That last point is the design decision worth naming. A scheduler in an
//! inactive scene **does not fire**, and does not catch up when its scene comes
//! back — the worker primes itself against the clock on every start (see
//! [`crate::media::scheduler`]), so switching scenes across the hour cannot
//! ring the hour late. A schedule that must survive scene changes belongs in
//! every scene that can be live, which is the same rule a microphone follows.

use crate::audio::ExternalFeeds;
use crate::config::{ScheduleItem, SourceConfig, SourceKindConfig};
use crate::media::scheduler::{Command, Scheduler, Status};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Schedulers {
    workers: RefCell<HashMap<String, Scheduler>>,
    /// Bumped by any worker whose status changes; polled by the pump.
    generation: Arc<AtomicU64>,
}

impl Schedulers {
    /// How many times any scheduler's status has changed. The pump compares
    /// this with what it last saw rather than being sent an event, because the
    /// writers are worker threads and the readers are labels.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Brings the running schedulers in line with `sources`: starts one for
    /// every Media Scheduler in the active scene, retires the ones whose source
    /// has gone, and reloads a surviving one whose items changed.
    ///
    /// Reloading a scheduler whose items did not move would re-prime it against
    /// the clock for nothing, and this is called on every scene edit — including
    /// edits to entirely different sources.
    pub fn apply(&self, sources: &[SourceConfig], feeds: &ExternalFeeds) {
        let wanted: Vec<(String, Vec<ScheduleItem>)> = sources
            .iter()
            .filter_map(|source| match &source.kind {
                SourceKindConfig::Scheduler(scheduler) => {
                    Some((source.name.clone(), scheduler.items.clone()))
                }
                _ => None,
            })
            .collect();

        let retired: Vec<Scheduler> = {
            let mut workers = self.workers.borrow_mut();
            let gone: Vec<String> = workers
                .keys()
                .filter(|name| !wanted.iter().any(|(wanted, _)| wanted == *name))
                .cloned()
                .collect();
            let retired = gone.iter().filter_map(|name| workers.remove(name)).collect();

            for (name, items) in wanted {
                match workers.get_mut(&name) {
                    Some(worker) => {
                        if worker.items != items {
                            worker.items = items.clone();
                            worker.send(Command::Reload { items });
                        }
                    }
                    None => {
                        let worker = Scheduler::start(
                            name.clone(),
                            feeds.clone(),
                            items,
                            self.generation.clone(),
                        );
                        workers.insert(name, worker);
                    }
                }
            }
            retired
        };
        // Stopped outside the borrow: `stop` joins a thread, and nothing that
        // waits should be holding a `RefCell` the rest of the UI reaches for.
        for worker in retired {
            worker.stop();
        }
    }

    /// Every running scheduler's state, keyed by source name, for the labels.
    pub fn statuses(&self) -> HashMap<String, Status> {
        self.workers
            .borrow()
            .iter()
            .map(|(name, worker)| (name.clone(), worker.status()))
            .collect()
    }

    /// Retires every scheduler. Used at shutdown.
    pub fn stop_all(&self) {
        let retired: Vec<Scheduler> = self.workers.borrow_mut().drain().map(|(_, w)| w).collect();
        for worker in retired {
            worker.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ScheduleTrigger, SchedulerSourceConfig};

    fn scheduler_source(name: &str, items: Vec<ScheduleItem>) -> SourceConfig {
        SourceConfig {
            name: name.to_string(),
            kind: SourceKindConfig::Scheduler(SchedulerSourceConfig { items }),
            ..Default::default()
        }
    }

    fn item(file: &str) -> ScheduleItem {
        ScheduleItem {
            file: file.to_string(),
            enabled: true,
            trigger: ScheduleTrigger::DailyAt { hour: 9, minute: 0 },
        }
    }

    #[test]
    fn a_scheduler_source_gets_a_worker_and_loses_it_with_the_scene() {
        let feeds = ExternalFeeds::default();
        let schedulers = Schedulers::default();

        schedulers.apply(&[scheduler_source("Media Scheduler", Vec::new())], &feeds);
        assert_eq!(schedulers.workers.borrow().len(), 1);

        schedulers.apply(&[], &feeds);
        assert!(schedulers.workers.borrow().is_empty());
    }

    /// The property `apply` exists for: an edit to some other source must not
    /// re-prime a running schedule.
    #[test]
    fn an_unrelated_edit_leaves_the_scheduler_alone() {
        let feeds = ExternalFeeds::default();
        let schedulers = Schedulers::default();
        schedulers.apply(&[scheduler_source("Media Scheduler", vec![item("a.mp3")])], &feeds);
        let first = schedulers.workers.borrow()["Media Scheduler"].thread_id();

        schedulers.apply(
            &[
                scheduler_source("Media Scheduler", vec![item("a.mp3")]),
                SourceConfig {
                    name: "Microphone".to_string(),
                    ..Default::default()
                },
            ],
            &feeds,
        );

        assert_eq!(
            schedulers.workers.borrow()["Media Scheduler"].thread_id(),
            first,
            "the same worker is still running"
        );
        schedulers.stop_all();
    }

    #[test]
    fn an_item_change_reaches_the_running_scheduler_without_restarting_it() {
        let feeds = ExternalFeeds::default();
        let schedulers = Schedulers::default();
        schedulers.apply(&[scheduler_source("Media Scheduler", Vec::new())], &feeds);
        let first = schedulers.workers.borrow()["Media Scheduler"].thread_id();

        schedulers.apply(&[scheduler_source("Media Scheduler", vec![item("a.mp3")])], &feeds);

        let borrowed = schedulers.workers.borrow();
        let worker = &borrowed["Media Scheduler"];
        assert_eq!(worker.thread_id(), first, "reloaded, not restarted");
        assert_eq!(worker.items.len(), 1);
        drop(borrowed);
        schedulers.stop_all();
    }
}
