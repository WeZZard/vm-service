//! `machine-service-install` — install or remove the `com.wezzard.machine-service` agent.

fn main() {
    if let Err(error) = machine_service_install::main_entry() {
        eprintln!("machine-service installation refused: {error}");
        std::process::exit(1);
    }
}
