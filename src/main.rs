use std::process::ExitCode;

fn main() -> ExitCode {
    if let Err(error) = agentlab::install_signal_handlers() {
        eprintln!("agentlab: {error:#}");
        return ExitCode::from(1);
    }
    ExitCode::from(agentlab::app::run(
        std::env::args().skip(1).collect(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    ))
}
