//! `deadchaps-reader`: the DeadChaps Companion's game reader, as its own
//! program, so the one process that opens Deadlock is built from code anyone
//! can read.
//!
//!   deadchaps-reader run --exe deadlock.exe --ended <file> [--enabled] [--allowed]
//!   deadchaps-reader version
//!
//! `run` reads the game while it runs and talks to the app over stdin and
//! stdout, one JSON object a line ([`deadchaps_reader::protocol`]). It exits
//! when stdin closes, so it never outlives the app. Its log goes to stderr.
//! It opens the game with read rights alone and never uses the network.

use deadchaps_reader::protocol::{FromReader, PROTOCOL, ToReader};
use deadchaps_reader::{GameMemory, Hooks};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// How long a question to the app about Valve's CDN waits for its answer.
const VALVE_WAIT: Duration = Duration::from_secs(10);

/// Messages for the one thread that writes stdout.
enum Out {
    /// The view changed: write it, whole, unless a newer change follows.
    Changed,
    Line(FromReader),
}

struct Args {
    exe: String,
    ended: PathBuf,
    enabled: bool,
    allowed: bool,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut exe = None;
    let mut ended = None;
    let (mut enabled, mut allowed) = (false, false);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--exe" => exe = it.next().cloned(),
            "--ended" => ended = it.next().map(PathBuf::from),
            "--enabled" => enabled = true,
            "--allowed" => allowed = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        exe: exe.ok_or("--exe is needed")?,
        ended: ended.ok_or("--ended is needed")?,
        enabled,
        allowed,
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("version") => {
            println!("{}", serde_json::json!({ "version": VERSION, "protocol": PROTOCOL }));
        }
        Some("run") => match parse_args(&args[1..]) {
            Ok(args) => run(args),
            Err(e) => {
                eprintln!("deadchaps-reader: {e}");
                std::process::exit(2);
            }
        },
        _ => {
            eprintln!("usage: deadchaps-reader run --exe <name> --ended <file> [--enabled] [--allowed] | version");
            std::process::exit(2);
        }
    }
}

fn run(args: Args) {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let (out_tx, out_rx) = mpsc::channel::<Out>();
    let account: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let asked: Arc<Mutex<HashMap<u64, Sender<bool>>>> = Arc::new(Mutex::new(HashMap::new()));
    let next_id = Arc::new(AtomicU64::new(1));

    let hooks = Hooks {
        steam_account: {
            let account = Arc::clone(&account);
            Box::new(move || account.lock().unwrap_or_else(PoisonError::into_inner).clone())
        },
        on_change: {
            let tx = out_tx.clone();
            Box::new(move || {
                let _ = tx.send(Out::Changed);
            })
        },
        on_salts: {
            let tx = out_tx.clone();
            Box::new(move |url: &str| {
                let _ = tx.send(Out::Line(FromReader::Salts(url.to_owned())));
            })
        },
        valve_has: {
            let tx = out_tx.clone();
            let asked = Arc::clone(&asked);
            Box::new(move |url: &str| {
                let id = next_id.fetch_add(1, Ordering::Relaxed);
                let (answer_tx, answer_rx) = mpsc::channel();
                asked.lock().unwrap_or_else(PoisonError::into_inner).insert(id, answer_tx);
                let _ = tx.send(Out::Line(FromReader::AskValve { id, url: url.to_owned() }));
                let has = answer_rx.recv_timeout(VALVE_WAIT).unwrap_or(false);
                asked.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                has
            })
        },
    };

    let _ = out_tx.send(Out::Line(FromReader::Hello { version: VERSION.to_owned(), protocol: PROTOCOL }));
    let memory = Arc::new(GameMemory::start(args.enabled, args.allowed, args.exe, args.ended, hooks));
    {
        let memory = Arc::clone(&memory);
        std::thread::Builder::new()
            .name("stdout".to_owned())
            .spawn(move || write_out(&memory, &out_rx))
            .expect("the stdout thread starts");
    }
    // Send the first view, so the app has one before anything changes.
    let _ = out_tx.send(Out::Changed);

    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ToReader>(&line) {
            Ok(ToReader::SetEnabled(on)) => memory.set_enabled(on),
            Ok(ToReader::SetAllowed(on)) => memory.set_allowed(on),
            Ok(ToReader::SetSpeed(speed)) => memory.set_speed(speed),
            Ok(ToReader::Account(id)) => *account.lock().unwrap_or_else(PoisonError::into_inner) = id,
            Ok(ToReader::AddEnded(ended)) => {
                if memory.add_ended(ended) {
                    let _ = out_tx.send(Out::Changed);
                }
            }
            Ok(ToReader::Valve { id, has }) => {
                if let Some(answer) = asked.lock().unwrap_or_else(PoisonError::into_inner).remove(&id) {
                    let _ = answer.send(has);
                }
            }
            Ok(ToReader::Stop) => break,
            Err(e) => tracing::warn!("Reader: a line from the app not understood ({e})"),
        }
    }
    // stdin closed or the app sent stop: either way the app is done with us.
    memory.stop();
    tracing::info!("Reader: stopping");
    std::process::exit(0);
}

/// Writes stdout, one line a message. A burst of changes is written once,
/// as the view stands after the last of them, so a busy moment in the game
/// does not flood the app with views it would replace at once.
fn write_out(memory: &GameMemory, rx: &Receiver<Out>) {
    let stdout = std::io::stdout();
    while let Ok(first) = rx.recv() {
        let mut changed = false;
        let mut lines = Vec::new();
        for msg in std::iter::once(first).chain(rx.try_iter()) {
            match msg {
                Out::Changed => changed = true,
                Out::Line(line) => lines.push(line),
            }
        }
        if changed {
            lines.push(FromReader::View(Box::new(memory.view())));
        }
        let mut out = stdout.lock();
        for line in lines {
            let ok = serde_json::to_writer(&mut out, &line).is_ok() && out.write_all(b"\n").is_ok();
            if !ok {
                // the app closed our stdout: nobody is listening
                std::process::exit(0);
            }
        }
        if out.flush().is_err() {
            std::process::exit(0);
        }
    }
}
