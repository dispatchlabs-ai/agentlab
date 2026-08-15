use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(agentlab::app::run(
        std::env::args().skip(1).collect(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    ))
}
