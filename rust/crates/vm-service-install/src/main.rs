//! `vm-service-install` — install or remove the `com.wezzard.vm-service` agent.

fn main() {
    if let Err(error) = vm_service_install::main_entry() {
        eprintln!("vm-service installation refused: {error}");
        std::process::exit(1);
    }
}
