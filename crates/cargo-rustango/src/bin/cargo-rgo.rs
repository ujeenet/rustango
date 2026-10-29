//! `cargo rgo …` — the `cargo rustango …` scaffolder under a short name.

#[path = "../main.rs"]
mod scaffolder;

fn main() -> std::process::ExitCode {
    scaffolder::main()
}
