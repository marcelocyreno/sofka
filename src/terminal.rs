use std::io::{self, Write};
use std::process::Stdio;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Suspend the TUI for an interactive command, then restore its terminal modes.
/// Call this only from the main loop, with the current mouse capture state.
pub fn suspend_and_run(
    terminal: &mut ratatui::DefaultTerminal,
    argv: &[String],
    captured: bool,
) -> io::Result<()> {
    if argv.is_empty() {
        return Ok(());
    }
    // Protect the parent before normal terminal input can generate signals.
    #[cfg(unix)]
    let _signals = SignalGuard::new()?;
    if captured {
        let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
    }
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
    let result = run_command(argv);
    // Set the modes directly. ratatui::init would install another panic hook.
    let _ = enable_raw_mode();
    let _ = crossterm::execute!(io::stdout(), EnterAlternateScreen);
    if captured {
        let _ = crossterm::execute!(io::stdout(), EnableMouseCapture);
    }
    let _ = terminal.clear();
    result
}

const ERROR_LIMIT: usize = 16 * 1024;

fn run_command(argv: &[String]) -> io::Result<()> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(run_command_async(argv))
            })
            .join()
            .map_err(|_| io::Error::other("Command runner failed."))?
    })
}

async fn run_command_async(argv: &[String]) -> io::Result<()> {
    use tokio::io::AsyncReadExt;
    let mut child = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut tail = Vec::new();
    let mut buffer = [0; 4096];
    let mut status = None;
    let mut closed = false;
    let mut deadline = tokio::time::Instant::now();
    loop {
        tokio::select! {
            result = child.wait(), if status.is_none() => {
                status = Some(result?);
                // A descendant can retain stderr after the command exits.
                deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(100);
            }
            result = stderr.read(&mut buffer), if !closed => {
                match result {
                    Ok(0) => closed = true,
                    Ok(n) => {
                        let _ = io::stderr().write_all(&buffer[..n]);
                        tail.extend_from_slice(&buffer[..n]);
                        if tail.len() > ERROR_LIMIT { tail.drain(..tail.len() - ERROR_LIMIT); }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {},
                    Err(_) => closed = true,
                }
            }
            _ = tokio::time::sleep_until(deadline), if status.is_some() => break,
        }
        if status.is_some() && closed {
            break;
        }
    }
    let status = status.expect("command has exited");
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "Command failed ({status}).\n{}",
            String::from_utf8_lossy(&tail).trim()
        )))
    }
}

#[cfg(unix)]
struct SignalGuard {
    saved: Vec<(libc::c_int, libc::sigaction)>,
}

#[cfg(unix)]
impl SignalGuard {
    fn new() -> io::Result<Self> {
        // A caught handler resets to the default on exec. SIG_IGN would also
        // make the child ignore Ctrl-C. Keep both processes on the terminal.
        extern "C" fn catch_signal(_: libc::c_int) {}

        let mut guard = Self {
            saved: Vec::with_capacity(2),
        };
        for signal in [libc::SIGINT, libc::SIGQUIT] {
            // SAFETY: Both structures are initialized before use. The handler
            // has the required ABI and does not access memory or call functions.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                let mut previous: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = catch_signal as *const () as libc::sighandler_t;
                action.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(signal, &action, &mut previous) == -1 {
                    return Err(io::Error::last_os_error());
                }
                guard.saved.push((signal, previous));
            }
        }
        Ok(guard)
    }
}

#[cfg(unix)]
impl Drop for SignalGuard {
    fn drop(&mut self) {
        for (signal, previous) in self.saved.iter().rev() {
            // SAFETY: Restore the valid action saved for this signal.
            unsafe {
                libc::sigaction(*signal, previous, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
