use armem::elf::Elf;
use armem::image::{Image, LoadOptions};
use armem::{Model, litmus, report, run};
use std::process::ExitCode;

const USAGE: &str = "\
usage: armem [options] FILE.litmus...

Explores every behavior of a litmus test under the ARMv8 memory model and
compares it with x86-TSO, flagging outcomes that only ARM allows.

options:
  -m, --model LIST   comma-separated models: arm, tso, sc (default: arm,tso)
  -f, --fuel N       max instructions per thread per execution (default: 200)
  -s, --summary      print one line per test instead of full reports
      --lse2         model FEAT_LSE2 (16-byte atomic LDP/STP, atomic
                     unaligned accesses within a 16-byte block)
      --sample N     follow N random paths through the ARM model's
                     executions instead of exploring them all (for tests
                     too big to explore exhaustively; may miss outcomes)
      --seed N       random seed for --sample (default: 0)
      --max-memories N
                     stop the ARM search after N promise memories (the
                     report warns that other final states may be possible)
      --no-por       explore every order of writes, without partial-order
                     reduction (slower; for checking the reduction)
      --no-dynamic-por
                     prune memories only by where the threads may access
                     memory, not also by what they may do after each one
                     (slower; for checking the dynamic reduction)
      --no-private   give every write to memory a place in the order of
                     writes, even to memory only one thread accesses
                     (slower; for checking that optimization)
      --no-live      keep registers that are never read again in the
                     states explored (slower; for checking that
                     optimization)
      --no-filter    certify every candidate promise, even those the
                     thread's own exploration shows cannot certify
                     (slower; for checking that optimization)
      --check-filter certify those candidates too, and stop with an error
                     if one certifies
      --elf FILE     start from the memory of an AArch64 ELF file (an
                     object file is relocated first); its global variables
                     can be named in the tests
      --elf-base N   where an object file's sections are placed
                     (default: 0x10000)
  -h, --help         show this help
";

fn main() -> ExitCode {
    let mut models = vec![Model::Arm, Model::Tso];
    let mut opts = armem::Options::default();
    let mut summary = false;
    let mut files = Vec::new();
    let mut elf_file = None;
    let mut load = LoadOptions::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{}", USAGE);
                return ExitCode::SUCCESS;
            }
            "-s" | "--summary" => summary = true,
            "--lse2" => opts.lse2 = true,
            "--no-por" => opts.por = false,
            "--no-dynamic-por" => opts.dynamic_por = false,
            "--no-private" => opts.private = false,
            "--no-live" => opts.live = false,
            "--no-filter" => opts.filter = false,
            "--check-filter" => opts.check_filter = true,
            "--sample" | "--seed" | "--max-memories" => match args.next().and_then(|v| v.parse().ok()) {
                Some(n) if a == "--sample" => opts.sample = Some(n),
                Some(n) if a == "--max-memories" => opts.max_memories = Some(n),
                Some(n) => opts.seed = n,
                None => {
                    eprintln!("{} needs a number", a);
                    return ExitCode::from(2);
                }
            },
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
            "--elf" => match args.next() {
                Some(f) => elf_file = Some(f),
                None => {
                    eprintln!("--elf needs a file");
                    return ExitCode::from(2);
                }
            },
            "--elf-base" => match args.next().and_then(|v| litmus_int(&v)) {
                Some(n) => load.base = n,
                None => {
                    eprintln!("--elf-base needs an address");
                    return ExitCode::from(2);
                }
            },
            "-f" | "--fuel" => match args.next().and_then(|v| v.parse().ok()) {
                Some(n) => opts.fuel = n,
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
    let preload = match &elf_file {
        None => litmus::Preload::default(),
        Some(f) => match load_elf(f, &load) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{}: {}", f, e);
                return ExitCode::FAILURE;
            }
        },
    };
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
        let test = match litmus::parse_with(&src, &preload) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{}: {}", f, e);
                failed = true;
                continue;
            }
        };
        let results: Vec<_> = models.iter().map(|&m| (m, run(&test, m, &opts))).collect();
        if summary {
            println!("{}", report::summary(&test, &results));
        } else {
            print!("{}", report::full(&test, &results));
        }
    }
    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

fn litmus_int(s: &str) -> Option<u64> {
    armem::asm::parse_int(s).ok().and_then(|v| u64::try_from(v).ok())
}

fn load_elf(path: &str, opts: &LoadOptions) -> Result<litmus::Preload, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let elf = Elf::parse(&data)?;
    Image::load(&elf, opts)?.preload()
}
