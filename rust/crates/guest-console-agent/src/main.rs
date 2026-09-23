//! `guest-console-agent <linux|macos> <probe|serve>`.

fn main() {
    // `args_os` avoids panicking on a non-UTF-8 argument; an unrecognized
    // argument is rejected with `arguments_invalid` like any other bad input.
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    std::process::exit(guest_console_agent::serve::run_main(&args));
}
