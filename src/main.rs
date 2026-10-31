use armem::{Model, litmus, report, run};
use std::process::ExitCode;

const USAGE: &str = "\
usage: armem [options] FILE.litmus...

Explores every behaviour of a litmus test under the ARMv8 memory model and
compares it with x86-TSO, flagging outcomes that only ARM allows.

options:
  -m, --model LIST   comma-separated models: arm, tso, sc (default: arm,tso)
  -f, --fuel N       max instructions per thread per execution (default: 200)
  -s, --summary      print one line per test instead of full reports
  -h, --help         show this help
";

fn main() -> ExitCode {
    let mut models = vec![Model::Arm, Model::Tso];
    let mut fuel = 200;
    let mut summary = false;
    let mut files = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{}", USAGE);
                return ExitCode::SUCCESS;
            }
            "-s" | "--summary" => summary = true,
            "-m" | "--model" => {
                let Some(v) = args.next() else {
                    eprintln!("--model needs an argument");
                    return ExitCode::from(2);
                };
                models.clear();
                for m in v.split(',') {
                    models.push(match m.trim().to_ascii_lowercase().as_str() {
                        "arm" | "aarch64" => Model::Arm,
                        "tso" | "x86" => Model::Tso,
                        "sc" => Model::Sc,
                        _ => {
                            eprintln!("unknown model '{}'", m);
                            return ExitCode::from(2);
                        }
                    });
                }
            }
            "-f" | "--fuel" => match args.next().and_then(|v| v.parse().ok()) {
                Some(n) => fuel = n,
                None => {
                    eprintln!("--fuel needs a number");
                    return ExitCode::from(2);
                }
            },
            _ if a.starts_with('-') => {
                eprintln!("unknown option '{}'\n{}", a, USAGE);
                return ExitCode::from(2);
            }
            _ => files.push(a),
        }
    }
    if files.is_empty() {
        eprint!("{}", USAGE);
        return ExitCode::from(2);
    }
    let mut failed = false;
    for f in &files {
        let src = match std::fs::read_to_string(f) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{}: {}", f, e);
                failed = true;
                continue;
            }
        };
        let test = match litmus::parse(&src) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{}: {}", f, e);
                failed = true;
                continue;
            }
        };
        let results: Vec<_> = models.iter().map(|&m| (m, run(&test, m, fuel))).collect();
        if summary {
            println!("{}", report::summary(&test, &results));
        } else {
            print!("{}", report::full(&test, &results));
        }
    }
    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
