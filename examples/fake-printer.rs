//! A fake IPP printer to drive by hand, for trying out inkdrop's printer
//! state and job tracking without a real printer or paper.
//!
//! Point inkdrop at it with `INKDROP_PRINTERS`, then type commands to stop
//! the printer, hold or cancel jobs, and so on (`help` lists them). In auto
//! mode, on by default, it prints queued jobs one at a time, as a real
//! printer does.

#[path = "../tests/support/mod.rs"]
mod support;

use std::net::{Ipv4Addr, SocketAddr};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use ipp::model::{JobState, PrinterState};
use support::{Config, FakeJob, FakePrinter};
use tokio::io::{AsyncBufReadExt, BufReader};

const HELP: &str = "\
Commands:
  status                 show the printer and its queue
  stop [REASON...]       stop the printer, e.g. `stop media-empty-error`
  start                  start it again, clearing its reasons
  reasons [REASON...]    set printer-state-reasons, e.g. `reasons toner-low-warning`;
                         with none, clear them
  message [TEXT]         set printer-state-message; with no text, clear it
  hold ID                hold a job
  release ID             release a held job
  cancel ID              cancel a job
  abort ID               abort a job, as if printing failed
  complete ID            finish a job
  forget ID              forget a job, as printers with little history do
  auto on|off            print queued jobs automatically, or not
  speed SECONDS          how long each job takes to print in auto mode
  offline | online       stop or start answering requests
  help                   show this
  quit                   stop the fake printer";

/// A fake IPP printer to drive by hand.
#[derive(Parser)]
struct Args {
    /// Port to listen on, at 127.0.0.1.
    #[arg(long, default_value_t = 1632)]
    port: u16,

    /// Name the printer gives itself.
    #[arg(long, default_value = "Fake Printer")]
    name: String,

    /// Seconds each job takes to print in auto mode.
    #[arg(long, default_value_t = 10)]
    speed: u64,

    /// Behave like a printer without IPP event notifications, so inkdrop
    /// polls it.
    #[arg(long)]
    no_notifications: bool,

    /// Accept only PWG-Raster, so inkdrop converts PDFs before sending them.
    #[arg(long)]
    raster: bool,
}

/// What auto mode is doing.
struct Auto {
    enabled: bool,
    per_job: Duration,
    printing: Option<(i32, Instant)>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let base = if args.raster { Config::raster_only(300, &["srgb_8", "sgray_8"]) } else { Config::default() };
    let config = Config {
        printer_info: Some(leak(&args.name)),
        model: Some("inkdrop fake printer"),
        job_id: Some(1),
        job_state: Some(ipp::value::IppValue::Enum(JobState::Pending as i32)),
        notifications: !args.no_notifications,
        notify_wait: Duration::from_secs(30),
        ..base
    };
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, args.port));
    let printer = match FakePrinter::start_at(addr, config).await {
        Ok(printer) => Arc::new(printer),
        Err(err) => {
            eprintln!("error: can't listen on {addr}: {err}");
            return ExitCode::FAILURE;
        }
    };
    let auto = Arc::new(Mutex::new(Auto { enabled: true, per_job: Duration::from_secs(args.speed), printing: None }));

    println!("{} is listening at {}", args.name, printer.uri());
    println!("Run inkdrop with:  INKDROP_PRINTERS={} cargo run", printer.uri());
    println!("Type `help` for commands.\n");

    tokio::spawn(run_queue(printer.clone(), auto.clone()));

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            [] => {}
            ["quit" | "exit"] => break,
            words => println!("{}", run_command(&printer, &auto, words)),
        }
    }
    ExitCode::SUCCESS
}

/// Carry out one command, returning what to tell the user.
fn run_command(printer: &FakePrinter, auto: &Mutex<Auto>, words: &[&str]) -> String {
    match words {
        ["help"] => HELP.to_owned(),
        ["status"] => describe(printer, &auto.lock().unwrap()),
        ["stop", reasons @ ..] => {
            printer.configure(|c| {
                c.printer_state = PrinterState::Stopped;
                c.printer_state_reasons = reasons.iter().map(|reason| leak(reason)).collect();
            });
            "Printer stopped.".to_owned()
        }
        ["start"] => {
            printer.configure(|c| {
                c.printer_state = PrinterState::Idle;
                c.printer_state_reasons.clear();
            });
            "Printer started.".to_owned()
        }
        ["reasons", reasons @ ..] => {
            printer.configure(|c| c.printer_state_reasons = reasons.iter().map(|reason| leak(reason)).collect());
            "Reasons set.".to_owned()
        }
        ["message"] => {
            printer.configure(|c| c.printer_state_message = None);
            "Message cleared.".to_owned()
        }
        ["message", ..] => {
            let text = words[1..].join(" ");
            printer.configure(|c| c.printer_state_message = Some(leak(&text)));
            "Message set.".to_owned()
        }
        [command @ ("hold" | "release" | "cancel" | "abort" | "complete" | "forget"), id] => {
            let Ok(id) = id.parse::<i32>() else {
                return format!("Not a job id: {id}");
            };
            if !printer.jobs().iter().any(|job| job.id == id) {
                return format!("No job {id}.");
            }
            match *command {
                "forget" => printer.forget_job(id),
                "hold" => printer.set_job_state(id, JobState::PendingHeld),
                "release" => printer.set_job_state(id, JobState::Pending),
                "cancel" => printer.set_job_state(id, JobState::Canceled),
                "abort" => printer.set_job_state(id, JobState::Aborted),
                _ => printer.set_job_state(id, JobState::Completed),
            }
            format!("Job {id}: {command} done.")
        }
        ["auto", setting @ ("on" | "off")] => {
            auto.lock().unwrap().enabled = *setting == "on";
            format!("Auto mode {setting}.")
        }
        ["speed", seconds] => match seconds.parse::<u64>() {
            Ok(seconds) => {
                auto.lock().unwrap().per_job = Duration::from_secs(seconds);
                format!("Each job now takes {seconds} s.")
            }
            Err(_) => format!("Not a number of seconds: {seconds}"),
        },
        [setting @ ("offline" | "online")] => {
            printer.configure(|c| c.online = *setting == "online");
            format!("Printer {setting}.")
        }
        _ => format!("Unknown command: {}. Type `help` for commands.", words.join(" ")),
    }
}

fn describe(printer: &FakePrinter, auto: &Auto) -> String {
    let config = printer.config();
    let state = match config.printer_state {
        PrinterState::Idle => "idle",
        PrinterState::Processing => "processing",
        PrinterState::Stopped => "stopped",
    };
    let reasons = if config.printer_state_reasons.is_empty() { "none".to_owned() } else { config.printer_state_reasons.join(", ") };
    let mut result = format!(
        "{} at {}\n  state: {state}, reasons: {reasons}, message: {}\n  {}, auto mode {} ({} s per job), notifications {}",
        config.printer_info.unwrap_or("Printer"),
        printer.uri(),
        config.printer_state_message.unwrap_or("none"),
        if config.online { "online" } else { "offline" },
        if auto.enabled { "on" } else { "off" },
        auto.per_job.as_secs(),
        if config.notifications { "on" } else { "off" },
    );
    let jobs = printer.jobs();
    if jobs.is_empty() {
        result.push_str("\n  no jobs");
    }
    for job in jobs {
        result.push_str(&format!("\n  job {:>3}: {}", job.id, job_state_name(job.state)));
    }
    result
}

/// Announce new jobs, and in auto mode print them one at a time.
async fn run_queue(printer: Arc<FakePrinter>, auto: Arc<Mutex<Auto>>) {
    let mut announced = 0;
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let newest = printer.jobs().iter().map(|job| job.id).max().unwrap_or(0).max(announced);
        for id in announced + 1..=newest {
            println!("Job {id} received.");
        }
        announced = newest;
        let mut auto = auto.lock().unwrap();
        if auto.enabled {
            advance(&printer, &mut auto);
        }
    }
}

/// Take auto mode one step: finish the job being printed once its time is
/// up, then start the next, keeping the printer's state in step.
fn advance(printer: &FakePrinter, auto: &mut Auto) {
    let config = printer.config();
    if !config.online {
        return;
    }
    let jobs = printer.jobs();
    let state_of = |id: i32| jobs.iter().find(|job| job.id == id).map(|job| job.state);

    if config.printer_state == PrinterState::Stopped {
        if let Some((id, _)) = auto.printing.take()
            && state_of(id) == Some(JobState::Processing)
        {
            printer.set_job_state(id, JobState::ProcessingStopped);
            println!("Job {id} stopped with the printer.");
        }
        return;
    }

    if let Some((id, started)) = auto.printing {
        match state_of(id) {
            Some(JobState::Processing) if started.elapsed() >= auto.per_job => {
                printer.set_job_state(id, JobState::Completed);
                println!("Job {id} printed.");
                auto.printing = None;
            }
            Some(JobState::Processing) => return,
            _ => auto.printing = None,
        }
    }

    let next = jobs
        .iter()
        .find(|job| job.state == JobState::ProcessingStopped)
        .or_else(|| jobs.iter().find(|job| job.state == JobState::Pending));
    match next {
        Some(&FakeJob { id, .. }) => {
            printer.set_job_state(id, JobState::Processing);
            auto.printing = Some((id, Instant::now()));
            println!("Printing job {id}.");
            set_printer_state(printer, &config, PrinterState::Processing);
        }
        None => set_printer_state(printer, &config, PrinterState::Idle),
    }
}

fn set_printer_state(printer: &FakePrinter, config: &Config, state: PrinterState) {
    if config.printer_state != state {
        printer.configure(|c| c.printer_state = state);
    }
}

fn job_state_name(state: JobState) -> &'static str {
    match state {
        JobState::Pending => "pending",
        JobState::PendingHeld => "held",
        JobState::Processing => "processing",
        JobState::ProcessingStopped => "stopped",
        JobState::Canceled => "cancelled",
        JobState::Aborted => "aborted",
        JobState::Completed => "completed",
    }
}

/// Config holds `&'static str`s; the few strings typed in a session can
/// safely live for the rest of it.
fn leak(text: &str) -> &'static str {
    Box::leak(text.to_owned().into_boxed_str())
}
