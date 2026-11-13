//! The supervisor as its own binary, for tests and trial runs. GitEZ itself
//! runs it as `gitez supervise …`.

use std::process::ExitCode;

fn main() -> ExitCode {
    gitez_supervisor::cli::main(std::env::args_os().skip(1))
}
