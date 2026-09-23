//! `console-worker` executable entry point.

fn main() {
    std::process::exit(console_worker::main_impl());
}
