//! Command-line front end for the solver.
//!
//!   depsolve [FILE]    solve one JSON problem (from FILE or stdin), print the outcome;
//!                      solve time is reported on stderr
//!   depsolve --jsonl   solve one JSON problem per stdin line, print one outcome per
//!                      line, each with a "seconds" field

use std::io::{BufRead, Read, Write};
use std::time::Instant;

fn main() {
    let argument = std::env::args().nth(1);
    if argument.as_deref() == Some("--jsonl") {
        return run_jsonl();
    }

    let mut input = String::new();
    let read = match argument {
        Some(path) => std::fs::read_to_string(&path).map(|text| input = text),
        None => std::io::stdin().read_to_string(&mut input).map(|_| ()),
    };
    if let Err(err) = read {
        eprintln!("depsolve: cannot read problem: {err}");
        std::process::exit(2);
    }

    let start = Instant::now();
    let output = depsolve_rust::solve_json(&input);
    eprintln!("solved in {:.6}s", start.elapsed().as_secs_f64());
    println!("{output}");
}

fn run_jsonl() {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in std::io::stdin().lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                eprintln!("depsolve: cannot read problem: {err}");
                std::process::exit(2);
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let start = Instant::now();
        let output = depsolve_rust::solve_json(&line);
        let seconds = start.elapsed().as_secs_f64();

        let mut outcome: serde_json::Value = serde_json::from_str(&output).expect("solver emits valid JSON");
        outcome["seconds"] = seconds.into();
        writeln!(out, "{outcome}").and_then(|_| out.flush()).expect("stdout is writable");
    }
}
