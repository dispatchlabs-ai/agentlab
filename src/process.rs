use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;

pub const DEFAULT_EXTERNAL_TIMEOUT_SECONDS: u64 = 30 * 60;
pub const MAX_EXTERNAL_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_RUN_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_DIFF_REQUEST_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_DIFF_FILE_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_DIFF_PATCH_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_COMMAND_METADATA_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_IGNORE_RULE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Default)]
pub struct BoundedCapture {
    pub bytes: Vec<u8>,
    pub total_bytes: u64,
    pub truncated: bool,
}

impl BoundedCapture {
    pub fn push(&mut self, chunk: &[u8], limit: usize) -> usize {
        self.total_bytes = self.total_bytes.saturating_add(chunk.len() as u64);
        let retained = limit.saturating_sub(self.bytes.len()).min(chunk.len());
        self.bytes.extend_from_slice(&chunk[..retained]);
        self.truncated |= retained < chunk.len();
        retained
    }
}

pub fn read_bounded(mut source: impl Read, limit: usize) -> io::Result<BoundedCapture> {
    let mut capture = BoundedCapture::default();
    let mut buffer = [0_u8; 8192];
    loop {
        let size = source.read(&mut buffer)?;
        if size == 0 {
            return Ok(capture);
        }
        capture.push(&buffer[..size], limit);
    }
}

pub struct BoundedCommandOutput {
    pub status: ExitStatus,
    pub stdout: BoundedCapture,
    pub stderr: BoundedCapture,
}

pub fn output_bounded(command: &mut Command, limit: usize) -> io::Result<BoundedCommandOutput> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    isolate_process_group(command);
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("capture command stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("capture command stderr pipe"))?;
    let stdout_reader = thread::spawn(move || read_bounded(stdout, limit));
    let stderr_reader = thread::spawn(move || read_bounded(stderr, limit));
    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            let _ = terminate_process_group(&mut child);
            let _ = child.wait();
            return Err(error);
        }
    };
    let _ = terminate_process_group(&mut child);
    let stdout = stdout_reader
        .join()
        .map_err(|_| io::Error::other("command stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| io::Error::other("command stderr reader panicked"))??;
    Ok(BoundedCommandOutput {
        status,
        stdout,
        stderr,
    })
}

pub fn isolate_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
}

/// Kill the complete process group created by `isolate_process_group`. This is
/// also called after a successful direct-child exit so descendants cannot keep
/// output pipes open or continue running behind AgentLab's back.
pub fn terminate_process_group(child: &mut Child) -> io::Result<()> {
    #[cfg(unix)]
    {
        let raw_pid =
            i32::try_from(child.id()).map_err(|_| io::Error::other("child PID overflow"))?;
        let pid = rustix::process::Pid::from_raw(raw_pid)
            .ok_or_else(|| io::Error::other("child PID was zero"))?;
        match rustix::process::kill_process_group(pid, rustix::process::Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(not(unix))]
    {
        match child.kill() {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_capture_drains_but_retains_only_the_limit() {
        let input = vec![b'x'; 32];
        let captured = read_bounded(input.as_slice(), 7).unwrap();
        assert_eq!(captured.bytes, vec![b'x'; 7]);
        assert_eq!(captured.total_bytes, 32);
        assert!(captured.truncated);
    }
}
