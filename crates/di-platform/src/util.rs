//! Running external commands with a timeout.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

pub struct Output {
    pub stdout: Vec<u8>,
}

/// Run a command, returning its exit code and stdout whatever the exit status
/// (smartctl reports problems in bits of a non-zero status and still prints data).
pub fn run_status(program: &str, args: &[&str], timeout: Duration) -> Result<(i32, Vec<u8>)> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("running {program}"))?;
    let mut stdout = child.stdout.take().unwrap();
    let out_thread = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = stdout.read_to_end(&mut v);
        v
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{program} {} timed out after {}s", args.join(" "), timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok((status.code().unwrap_or(-1), out_thread.join().unwrap_or_default()))
}

/// Run a command, failing if it exits non-zero or runs longer than `timeout`.
pub fn run(program: &str, args: &[&str], timeout: Duration) -> Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {program}"))?;

    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out_thread = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = stdout.read_to_end(&mut v);
        v
    });
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{program} {} timed out after {}s", args.join(" "), timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    if !status.success() {
        let msg = if stderr.trim().is_empty() {
            String::from_utf8_lossy(&stdout).trim().to_string()
        } else {
            stderr.trim().to_string()
        };
        bail!("{program} {} failed: {msg}", args.join(" "));
    }
    Ok(Output { stdout })
}
