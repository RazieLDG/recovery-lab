use recovery_lab::runner::{run_blocking_with_events, Event, Report, Scenario};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Instant;

fn log_event(event: &Event) {
    eprintln!(
        "{:>7} ms  {}: {}",
        event.elapsed_ms, event.phase, event.detail
    );
}

fn reserve_report(path: &Option<PathBuf>) -> Result<Option<fs::File>, String> {
    path.as_ref()
        .map(|p| {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(p)
                .map_err(|e| format!("cannot create report {}: {e}", p.display()))
        })
        .transpose()
}
fn write_report(file: &mut fs::File, bytes: &[u8]) -> Result<(), String> {
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("cannot write report: {e}"))
}
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        println!("recovery-lab run SCENARIO.json [--json PATH] [--junit PATH]\nOnly literal loopback HTTP targets are accepted. Output files must not exist.");
        return;
    }
    if args.len() < 2 || args[0] != "run" {
        eprintln!("usage: recovery-lab run SCENARIO.json [--json PATH] [--junit PATH]");
        std::process::exit(2);
    }
    let mut json_path = None;
    let mut junit_path = None;
    let mut i = 2;
    while i < args.len() {
        if i + 1 >= args.len() {
            eprintln!("missing option value");
            std::process::exit(2);
        }
        match args[i].as_str() {
            "--json" if json_path.is_none() => json_path = Some(PathBuf::from(&args[i + 1])),
            "--junit" if junit_path.is_none() => junit_path = Some(PathBuf::from(&args[i + 1])),
            _ => {
                eprintln!("unknown or duplicate option");
                std::process::exit(2);
            }
        }
        i += 2;
    }
    let (mut json_file, mut junit_file) = match reserve_report(&json_path)
        .and_then(|j| reserve_report(&junit_path).map(|x| (j, x)))
    {
        Ok(files) => files,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let start = Instant::now();
    let c = Arc::new(AtomicBool::new(false));
    let cc = c.clone();
    let result = ctrlc::set_handler(move || cc.store(true, Ordering::SeqCst))
        .map_err(|_| "cannot install cancellation handler".to_string())
        .and_then(|_| Scenario::load(&args[1]).map_err(|error| error.to_string()));
    let mut report = match result {
        Ok(scenario) => match run_blocking_with_events(&scenario, &c, log_event) {
            Ok(report) => report,
            Err(error) => *error.report,
        },
        Err(error) => {
            let report = Report::error("unloaded", error, start.elapsed());
            for event in &report.events {
                log_event(event);
            }
            report
        }
    };
    let mut code = report.exit_code;
    report.elapsed_ms = start.elapsed().as_millis();
    if let Some(file) = &mut json_file {
        if let Err(e) = write_report(file, &report.to_json_pretty().expect("serializable report")) {
            eprintln!("{e}");
            if code != 3 {
                code = 2;
            }
            report.exit_code = code;
            report.outcome = "error".into();
        }
    }
    if let Some(file) = &mut junit_file {
        if let Err(e) = write_report(file, report.to_junit().as_bytes()) {
            eprintln!("{e}");
            if code != 3 {
                code = 2;
            }
            report.exit_code = code;
            report.outcome = "error".into();
        }
    }
    eprintln!("result: {} (exit {code})", report.outcome);
    std::process::exit(code);
}
