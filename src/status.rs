//! Watches each printer's state and job queue while at least one browser is
//! watching: streamed from IPP event notifications where the printer supports
//! them, polled where it doesn't.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ipp::prelude::*;
use ipp::value::BoundedString;
use serde::Serialize;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until};
use tracing::{debug, info};

use crate::discovery::{Printer, Registry};
use crate::notifications::EventClient;
use crate::printing::{PrintError, flatten, status_message};

/// The latest status of each watched printer, keyed by [`Printer::id`].
/// A printer has no entry until its first status arrives.
pub type Statuses = watch::Sender<HashMap<String, PrinterStatus>>;

/// What a printer is doing, and its queue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PrinterStatus {
    /// `idle`, `processing` or `stopped` (from `printer-state`), or
    /// `unreachable` if the printer didn't answer.
    pub state: &'static str,
    /// `printer-state-reasons`, e.g. `media-empty-error`, without `none`.
    pub reasons: Vec<String>,
    /// `printer-state-message`, if the printer gave one.
    pub message: Option<String>,
    /// How many jobs the printer has yet to finish, from any source.
    pub queued: usize,
    /// The unfinished jobs the printer lists, in the order it will process
    /// them.
    pub jobs: Vec<Job>,
    /// Jobs submitted through inkdrop that have finished, oldest first.
    pub finished: Vec<Job>,
}

/// A print job and its state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Job {
    /// The printer's `job-id`.
    pub id: i32,
    /// The `job-state` keyword, e.g. `pending` or `completed`, or `forgotten`
    /// if the printer no longer knows the job.
    pub state: &'static str,
}

/// How often the monitor may contact printers.
#[derive(Clone, Copy, Debug)]
pub struct Timings {
    /// Interval between polls of a printer with nothing queued.
    pub idle_poll: Duration,
    /// Interval between polls of a printer with jobs queued.
    pub busy_poll: Duration,
    /// Least time between successive Get-Notifications requests.
    pub min_gap: Duration,
    /// How often to check, locally, whether anyone is watching.
    pub viewer_check: Duration,
    /// Lease requested for event subscriptions, which are replaced before
    /// they expire.
    pub lease: Duration,
    /// Time limit for each request, including a held Get-Notifications.
    pub request_timeout: Duration,
    /// How long to poll a printer whose subscription attempt failed to
    /// connect before trying to subscribe again.
    pub retry_streaming: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            idle_poll: Duration::from_secs(5),
            busy_poll: Duration::from_secs(2),
            min_gap: Duration::from_secs(1),
            viewer_check: Duration::from_secs(1),
            lease: Duration::from_secs(300),
            request_timeout: Duration::from_secs(120),
            retry_streaming: Duration::from_secs(60),
        }
    }
}

const EVENTS: &[&str] = &["printer-state-changed", "printer-queue-order-changed", "job-created", "job-state-changed", "job-completed"];

/// How many finished inkdrop jobs each printer's status remembers.
const FINISHED_KEPT: usize = 20;

/// Keeps a [`PrinterStatus`] for every printer in a [`Registry`].
#[derive(Clone)]
pub struct Monitor {
    statuses: Statuses,
    watchers: Arc<Mutex<HashMap<String, Watcher>>>,
}

struct Watcher {
    uri: Uri,
    submitted: mpsc::UnboundedSender<i32>,
    task: JoinHandle<()>,
}

impl Monitor {
    /// Start watching every printer in `registry`, as printers come and go,
    /// contacting them no more often than `timings` allows.
    ///
    /// # Panics
    ///
    /// If called outside a Tokio runtime.
    pub fn spawn(registry: &Registry, timings: Timings) -> Monitor {
        let result = Monitor { statuses: watch::channel(HashMap::new()).0, watchers: Arc::default() };
        let monitor = result.clone();
        let mut printers = registry.subscribe();
        tokio::spawn(async move {
            loop {
                let current = printers.borrow_and_update().clone();
                monitor.sync(&current, timings);
                if printers.changed().await.is_err() {
                    break;
                }
            }
        });
        result
    }

    /// The printers' statuses. Printers are only contacted while something
    /// subscribes to this.
    pub fn statuses(&self) -> &Statuses {
        &self.statuses
    }

    /// Follow job `job_id` on printer `printer_id`, so that its final state
    /// is reported in [`PrinterStatus::finished`]. Unknown printers are
    /// ignored.
    pub fn job_submitted(&self, printer_id: &str, job_id: i32) {
        if let Some(watcher) = self.watchers.lock().unwrap().get(printer_id) {
            let _ = watcher.submitted.send(job_id);
        }
    }

    fn sync(&self, printers: &HashMap<String, Printer>, timings: Timings) {
        let mut watchers = self.watchers.lock().unwrap();
        watchers.retain(|id, watcher| {
            let keep = printers.get(id).is_some_and(|printer| printer.uri == watcher.uri);
            if !keep {
                watcher.task.abort();
                self.statuses.send_if_modified(|statuses| statuses.remove(id).is_some());
            }
            keep
        });
        for (id, printer) in printers {
            watchers.entry(id.clone()).or_insert_with(|| {
                let (submitted, submissions) = mpsc::unbounded_channel();
                let watch = Watch::new(id.clone(), printer.uri.clone(), self.statuses.clone(), submissions, timings);
                Watcher { uri: printer.uri.clone(), submitted, task: tokio::spawn(watch.run()) }
            });
        }
    }
}

/// Jobs submitted through inkdrop: those still in progress, and the most
/// recently finished.
#[derive(Default)]
struct JobTracker {
    active: Vec<i32>,
    finished: VecDeque<Job>,
}

impl JobTracker {
    fn track(&mut self, job_id: i32) {
        if !self.active.contains(&job_id) {
            self.active.push(job_id);
        }
    }

    fn finish(&mut self, job: Job) {
        self.active.retain(|id| *id != job.id);
        if self.finished.len() == FINISHED_KEPT {
            self.finished.pop_front();
        }
        self.finished.push_back(job);
    }

    fn finished(&self) -> Vec<Job> {
        self.finished.iter().cloned().collect()
    }
}

/// The task watching one printer.
struct Watch {
    id: String,
    uri: Uri,
    client: AsyncIppClient,
    events: EventClient,
    statuses: Statuses,
    submissions: mpsc::UnboundedReceiver<i32>,
    tracker: JobTracker,
    busy: bool,
    timings: Timings,
}

impl Watch {
    fn new(id: String, uri: Uri, statuses: Statuses, submissions: mpsc::UnboundedReceiver<i32>, timings: Timings) -> Self {
        let client = AsyncIppClient::builder(uri.clone()).request_timeout(timings.request_timeout).build();
        let events = EventClient::new(&uri, timings.request_timeout);
        Watch { id, uri, client, events, statuses, submissions, tracker: JobTracker::default(), busy: false, timings }
    }

    async fn run(mut self) {
        let mut can_stream = true;
        loop {
            self.wait_for_viewers().await;
            if !can_stream {
                self.poll(None).await;
                continue;
            }
            match self.events.subscribe(EVENTS, self.timings.lease).await {
                Ok(subscription) => {
                    debug!(printer = %self.uri, subscription, "streaming printer events");
                    self.stream(subscription).await;
                }
                Err(err) if err.is_refusal() => {
                    info!(printer = %self.uri, %err, "printer has no event notifications; polling it instead");
                    can_stream = false;
                }
                Err(err) => {
                    debug!(printer = %self.uri, %err, "couldn't subscribe to printer events; polling for now");
                    self.poll(Some(Instant::now() + self.timings.retry_streaming)).await;
                }
            }
        }
    }

    async fn wait_for_viewers(&mut self) {
        while self.statuses.receiver_count() == 0 {
            tokio::select! {
                () = sleep(self.timings.viewer_check) => {}
                Some(job_id) = self.submissions.recv() => self.tracker.track(job_id),
            }
        }
    }

    /// Refresh whenever Get-Notifications answers until nobody is watching,
    /// the subscription is lost, or its lease is nearly up.
    ///
    /// Every answer triggers a refresh, events or not, because printers are
    /// unreliable here: `ippserver`, for one, ends a held request without the
    /// events that ended it, and never reports some state changes at all.
    async fn stream(&mut self, subscription: i32) {
        let replace_at = Instant::now() + self.timings.lease.mul_f32(0.8);
        let mut sequence = 1;
        self.refresh().await;
        loop {
            let started = Instant::now();
            let outcome = tokio::select! {
                result = self.events.notifications(subscription, sequence, true) => Some(result),
                Some(job_id) = self.submissions.recv() => {
                    self.tracker.track(job_id);
                    None
                }
                () = sleep(self.timings.idle_poll), if self.busy => None,
                () = nobody_watching(&self.statuses, self.timings.viewer_check) => break,
                () = sleep_until(replace_at) => break,
            };
            match outcome {
                None => self.refresh().await,
                Some(Ok(events)) => {
                    if let Some(last) = events.iter().map(|event| event.sequence).max() {
                        sequence = last + 1;
                    }
                    // A printer that doesn't hold requests answers at once, every time.
                    let answered_at_once = events.is_empty() && started.elapsed() < self.timings.min_gap;
                    self.refresh().await;
                    let pause = if answered_at_once { self.timings.idle_poll } else { self.timings.min_gap };
                    sleep_until(started + pause).await;
                }
                Some(Err(err)) => {
                    debug!(printer = %self.uri, %err, "lost printer event subscription");
                    break;
                }
            }
        }
        let _ = self.events.cancel(subscription).await;
    }

    /// Refresh periodically until nobody is watching or `until` passes.
    async fn poll(&mut self, until: Option<Instant>) {
        loop {
            self.refresh().await;
            let interval = if self.busy { self.timings.busy_poll } else { self.timings.idle_poll };
            tokio::select! {
                () = sleep(interval) => {}
                Some(job_id) = self.submissions.recv() => self.tracker.track(job_id),
                () = nobody_watching(&self.statuses, self.timings.viewer_check) => return,
            }
            if until.is_some_and(|until| Instant::now() >= until) {
                return;
            }
        }
    }

    async fn refresh(&mut self) {
        let status = match fetch_status(&self.client, &self.uri, &mut self.tracker).await {
            Ok(status) => status,
            Err(err) => {
                debug!(printer = %self.uri, %err, "couldn't fetch printer status");
                PrinterStatus {
                    state: "unreachable",
                    reasons: Vec::new(),
                    message: None,
                    queued: 0,
                    jobs: Vec::new(),
                    finished: self.tracker.finished(),
                }
            }
        };
        self.busy = !status.jobs.is_empty() || !self.tracker.active.is_empty();
        self.statuses.send_if_modified(|statuses| {
            if statuses.get(&self.id) == Some(&status) {
                return false;
            }
            statuses.insert(self.id.clone(), status);
            true
        });
    }
}

async fn nobody_watching(statuses: &Statuses, check: Duration) {
    while statuses.receiver_count() > 0 {
        sleep(check).await;
    }
}

/// Ask the printer at `uri` for its state and queue, and settle any jobs in
/// `tracker` that have left the queue.
async fn fetch_status(client: &AsyncIppClient, uri: &Uri, tracker: &mut JobTracker) -> Result<PrinterStatus, PrintError> {
    let operation = IppOperationBuilder::get_printer_attributes(uri.clone())
        .attribute("printer-state")
        .attribute("printer-state-reasons")
        .attribute("printer-state-message")
        .attribute("queued-job-count")
        .build()?;
    let response = checked(client.send(operation).await?)?;
    let printer = response.attributes().first_of(DelimiterTag::PrinterAttributes);
    let attr = |name: &str| printer.and_then(|group| group.get(name)).map(IppAttribute::value);

    let state = match attr("printer-state").and_then(|value| value.as_enum().copied()) {
        Some(5) => "stopped",
        Some(4) => "processing",
        _ => "idle",
    };
    let reasons = attr("printer-state-reasons")
        .map(|value| flatten(value).iter().map(ToString::to_string).filter(|reason| reason != "none").collect())
        .unwrap_or_default();
    let message = attr("printer-state-message").map(ToString::to_string).filter(|message| !message.is_empty());
    let queued = attr("queued-job-count").and_then(|value| value.as_integer().copied()).map(|count| count.max(0) as usize);

    let mut jobs = if queued != Some(0) || !tracker.active.is_empty() { unfinished_jobs(client, uri).await? } else { Vec::new() };
    for job_id in tracker.active.clone() {
        if jobs.iter().any(|job| job.id == job_id) {
            continue;
        }
        let state = job_state(client, uri, job_id).await?;
        let job = Job { id: job_id, state };
        if matches!(state, "canceled" | "aborted" | "completed" | "forgotten") {
            tracker.finish(job);
        } else {
            jobs.push(job);
        }
    }

    Ok(PrinterStatus {
        state,
        reasons,
        message,
        queued: queued.unwrap_or(0).max(jobs.len()),
        jobs,
        finished: tracker.finished(),
    })
}

/// The printer's unfinished jobs, in the order it will process them.
async fn unfinished_jobs(client: &AsyncIppClient, uri: &Uri) -> Result<Vec<Job>, PrintError> {
    let mut request: IppRequestResponse = IppOperationBuilder::get_jobs(uri.clone()).user_name("inkdrop").build()?.into();
    let operation = DelimiterTag::OperationAttributes;
    request.attributes_mut().add(operation, IppAttribute::with_name("which-jobs", keyword("not-completed")?)?);
    let requested = IppValue::Array(vec![keyword("job-id")?, keyword("job-state")?]);
    request.attributes_mut().add(operation, IppAttribute::with_name("requested-attributes", requested)?);

    let response = checked(client.send(request).await?)?;
    let result = response
        .attributes()
        .groups_of(DelimiterTag::JobAttributes)
        .filter_map(|group| {
            let id = *group.get("job-id")?.value().as_integer()?;
            let state = job_state_name(*group.get("job-state")?.value().as_enum()?);
            Some(Job { id, state })
        })
        .collect();
    Ok(result)
}

/// The state of job `job_id`, or `forgotten` if the printer doesn't know it.
async fn job_state(client: &AsyncIppClient, uri: &Uri, job_id: i32) -> Result<&'static str, PrintError> {
    let operation = IppOperationBuilder::get_job_attributes(uri.clone(), job_id).build()?;
    let response = client.send(operation).await?;
    if response.header().status_code() == StatusCode::ClientErrorNotFound {
        return Ok("forgotten");
    }
    let response = checked(response)?;
    let result = response
        .attributes()
        .first_of(DelimiterTag::JobAttributes)
        .and_then(|group| group.get("job-state"))
        .and_then(|attr| attr.value().as_enum().copied())
        .map_or("forgotten", job_state_name);
    Ok(result)
}

fn job_state_name(state: i32) -> &'static str {
    match JobState::try_from(state) {
        Ok(JobState::Pending) => "pending",
        Ok(JobState::PendingHeld) => "pending-held",
        Ok(JobState::Processing) => "processing",
        Ok(JobState::ProcessingStopped) => "processing-stopped",
        Ok(JobState::Canceled) => "canceled",
        Ok(JobState::Aborted) => "aborted",
        Ok(JobState::Completed) => "completed",
        Err(_) => "pending",
    }
}

fn checked(response: IppRequestResponse) -> Result<IppRequestResponse, PrintError> {
    let status = response.header().status_code();
    if status.is_success() {
        Ok(response)
    } else {
        Err(PrintError::Rejected { status, message: status_message(&response) })
    }
}

fn keyword(value: &str) -> Result<IppValue, PrintError> {
    Ok(IppValue::Keyword(BoundedString::new(value)?))
}

#[cfg(test)]
mod tests {
    use ipp::model::{JobState, PrinterState};

    use super::*;
    use crate::notifications::{CREATE_PRINTER_SUBSCRIPTIONS, GET_NOTIFICATIONS};
    use crate::test_support::{self, Config, FakePrinter};

    const WAIT: Duration = Duration::from_secs(5);

    fn printer(id: &str, uri: &str) -> Printer {
        Printer { id: id.to_owned(), name: id.to_owned(), uri: uri.parse().unwrap(), model: None, formats: vec!["PDF"] }
    }

    fn registry_of(printers: &[Printer]) -> Registry {
        watch::channel(printers.iter().map(|p| (p.id.clone(), p.clone())).collect()).0
    }

    /// Wait until printer `id`'s status satisfies `done`, returning it.
    async fn status_of(
        viewer: &mut watch::Receiver<HashMap<String, PrinterStatus>>,
        id: &str,
        mut done: impl FnMut(&PrinterStatus) -> bool,
    ) -> PrinterStatus {
        let found = tokio::time::timeout(WAIT, viewer.wait_for(|statuses| statuses.get(id).is_some_and(&mut done)))
            .await
            .map(|statuses| statuses.unwrap()[id].clone());
        found.unwrap_or_else(|_| panic!("timed out; last status: {:?}", viewer.borrow().get(id)))
    }

    async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(WAIT, async {
            while !condition() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
    }

    #[tokio::test]
    async fn streams_the_printer_state_and_follows_submitted_jobs() {
        test_support::init_tracing();
        let fake = FakePrinter::start(Config { notifications: true, ..Config::default() }).await;
        let monitor = Monitor::spawn(&registry_of(&[printer("p", &fake.uri())]), test_support::quick_timings());
        let mut viewer = monitor.statuses().subscribe();

        let idle = status_of(&mut viewer, "p", |_| true).await;
        assert_eq!((idle.state, idle.queued), ("idle", 0));
        eventually("subscribed", || fake.subscriptions().len() == 1).await;

        let queue = [42, 43, 44];
        for _ in queue {
            crate::printing::submit(
                &AsyncIppClient::new(fake.uri().parse().unwrap()),
                fake.uri().parse().unwrap(),
                bytes::Bytes::new(),
                crate::printing::DocumentFormat::Pdf,
                "t",
            )
            .await
            .unwrap();
        }
        monitor.job_submitted("p", 43);
        monitor.job_submitted("p", 44);
        monitor.job_submitted("missing", 1);
        fake.set_job_state(42, JobState::Processing);
        let busy = status_of(&mut viewer, "p", |status| status.jobs.first().is_some_and(|job| job.state == "processing")).await;
        assert_eq!(busy.queued, 3);
        assert_eq!(busy.jobs.iter().map(|job| job.id).collect::<Vec<_>>(), queue);

        fake.configure(|c| {
            c.printer_state = PrinterState::Stopped;
            c.printer_state_reasons = vec!["media-jam-error", "toner-low-warning"];
            c.printer_state_message = Some("Clear the jam");
        });
        let stopped = status_of(&mut viewer, "p", |status| status.state == "stopped").await;
        assert_eq!(stopped.reasons, ["media-jam-error", "toner-low-warning"]);
        assert_eq!(stopped.message.as_deref(), Some("Clear the jam"));

        fake.set_job_state(43, JobState::Canceled);
        fake.set_job_state(44, JobState::Completed);
        fake.forget_job(44);
        let finished = status_of(&mut viewer, "p", |status| status.finished.len() == 2).await;
        assert_eq!(finished.finished, [Job { id: 43, state: "canceled" }, Job { id: 44, state: "forgotten" }]);
        assert_eq!(finished.jobs, [Job { id: 42, state: "processing" }]);

        assert_eq!(fake.count(CREATE_PRINTER_SUBSCRIPTIONS), 1, "one subscription does it all");
    }

    #[tokio::test]
    async fn copes_with_printers_that_wake_without_events_or_miss_them() {
        let fake = FakePrinter::start(Config {
            notifications: true,
            notify_wait: Duration::from_secs(5),
            notify_wakes_empty: true,
            ..Config::default()
        })
        .await;
        let monitor = Monitor::spawn(&registry_of(&[printer("p", &fake.uri())]), test_support::quick_timings());
        let mut viewer = monitor.statuses().subscribe();
        status_of(&mut viewer, "p", |_| true).await;
        eventually("subscribed", || fake.subscriptions().len() == 1).await;

        fake.configure(|c| c.printer_state = PrinterState::Stopped);
        status_of(&mut viewer, "p", |status| status.state == "stopped").await;

        let client = AsyncIppClient::new(fake.uri().parse().unwrap());
        let format = crate::printing::DocumentFormat::Pdf;
        crate::printing::submit(&client, fake.uri().parse().unwrap(), bytes::Bytes::new(), format, "t").await.unwrap();
        monitor.job_submitted("p", 42);
        status_of(&mut viewer, "p", |status| status.jobs.len() == 1).await;
        fake.set_job_state_quietly(42, JobState::Completed);
        let finished = status_of(&mut viewer, "p", |status| !status.finished.is_empty()).await;
        assert_eq!(finished.finished, [Job { id: 42, state: "completed" }]);
    }

    #[tokio::test]
    async fn polls_printers_without_notifications_gently() {
        let fake = FakePrinter::start(Config::default()).await;
        let timings = Timings { idle_poll: Duration::from_millis(200), ..test_support::quick_timings() };
        let monitor = Monitor::spawn(&registry_of(&[printer("p", &fake.uri())]), timings);
        let mut viewer = monitor.statuses().subscribe();

        status_of(&mut viewer, "p", |status| status.state == "idle").await;
        fake.configure(|c| c.printer_state = PrinterState::Processing);
        status_of(&mut viewer, "p", |status| status.state == "processing").await;

        let polls = fake.count(Operation::GetPrinterAttributes as u16);
        sleep(Duration::from_millis(500)).await;
        let more = fake.count(Operation::GetPrinterAttributes as u16) - polls;
        assert!((1..=3).contains(&more), "polled {more} times in 500ms at a 200ms interval");
        assert_eq!(fake.count(CREATE_PRINTER_SUBSCRIPTIONS), 1, "refused once, never retried");
    }

    #[tokio::test]
    async fn contacts_printers_only_while_someone_watches() {
        let fake = FakePrinter::start(Config { notifications: true, ..Config::default() }).await;
        let monitor = Monitor::spawn(&registry_of(&[printer("p", &fake.uri())]), test_support::quick_timings());
        sleep(Duration::from_millis(100)).await;
        assert!(fake.received().is_empty(), "nobody is watching yet");

        monitor.job_submitted("p", 42);
        let mut viewer = monitor.statuses().subscribe();
        status_of(&mut viewer, "p", |_| true).await;
        eventually("subscribed", || fake.subscriptions().len() == 1).await;

        drop(viewer);
        eventually("the subscription is cancelled", || fake.subscriptions().is_empty()).await;
        let requests = fake.received().len();
        sleep(Duration::from_millis(200)).await;
        assert_eq!(fake.received().len(), requests, "no requests once nobody watches");
    }

    #[tokio::test]
    async fn resubscribes_when_the_subscription_is_lost_or_its_lease_is_up() {
        let fake = FakePrinter::start(Config { notifications: true, notify_wait: Duration::from_millis(50), ..Config::default() }).await;
        let timings = Timings { lease: Duration::from_millis(500), ..test_support::quick_timings() };
        let monitor = Monitor::spawn(&registry_of(&[printer("p", &fake.uri())]), timings);
        let _viewer = monitor.statuses().subscribe();

        eventually("subscribed", || fake.subscriptions().len() == 1).await;
        fake.drop_subscriptions();
        eventually("subscribed again", || fake.count(CREATE_PRINTER_SUBSCRIPTIONS) == 2).await;
        eventually("the lease is replaced", || fake.count(CREATE_PRINTER_SUBSCRIPTIONS) == 3).await;
        assert_eq!(fake.subscriptions().len(), 1, "the old subscription is cancelled");
        assert!(fake.count(GET_NOTIFICATIONS) > 0);
    }

    #[tokio::test]
    async fn reports_unreachable_printers_and_retries_streaming() {
        test_support::init_tracing();
        let fake = FakePrinter::start(Config { notifications: true, online: false, ..Config::default() }).await;
        let monitor = Monitor::spawn(&registry_of(&[printer("p", &fake.uri())]), test_support::quick_timings());
        let mut viewer = monitor.statuses().subscribe();

        let unreachable = status_of(&mut viewer, "p", |_| true).await;
        assert_eq!(unreachable.state, "unreachable");

        fake.configure(|c| c.online = true);
        status_of(&mut viewer, "p", |status| status.state == "idle").await;
        eventually("subscribed once the printer answers", || fake.subscriptions().len() == 1).await;
    }

    #[tokio::test]
    async fn follows_printers_as_they_come_go_and_move() {
        let first = FakePrinter::start(Config::default()).await;
        let second = FakePrinter::start(Config { printer_state: PrinterState::Stopped, ..Config::default() }).await;
        let registry = registry_of(&[printer("p", &first.uri())]);
        let monitor = Monitor::spawn(&registry, test_support::quick_timings());
        let mut viewer = monitor.statuses().subscribe();
        status_of(&mut viewer, "p", |status| status.state == "idle").await;

        registry.send_modify(|printers| {
            printers.insert("p".to_owned(), printer("p", &second.uri()));
        });
        status_of(&mut viewer, "p", |status| status.state == "stopped").await;

        registry.send_modify(HashMap::clear);
        tokio::time::timeout(WAIT, viewer.wait_for(HashMap::is_empty)).await.unwrap().unwrap();
        assert!(monitor.watchers.lock().unwrap().is_empty());
    }

    #[test]
    fn only_the_latest_finished_jobs_are_kept() {
        let mut tracker = JobTracker::default();
        tracker.track(1);
        tracker.track(1);
        assert_eq!(tracker.active, [1]);
        for id in 0..25 {
            tracker.finish(Job { id, state: "completed" });
        }
        assert!(tracker.active.is_empty());
        assert_eq!(tracker.finished().len(), FINISHED_KEPT);
        assert_eq!(tracker.finished()[0].id, 5);
    }

    #[test]
    fn job_states_have_their_ipp_names() {
        let names: Vec<_> = (3..=9).map(job_state_name).collect();
        assert_eq!(names, ["pending", "pending-held", "processing", "processing-stopped", "canceled", "aborted", "completed"]);
        assert_eq!(job_state_name(99), "pending");
    }
}
