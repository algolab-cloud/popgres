//! Signals during `popgres run`.
//!
//! They are counted, not obeyed on the spot: dying on the first Ctrl-C would
//! leave the database running, the one thing `run` promises never to do. A
//! first signal during startup lets startup finish so teardown can follow; a
//! second is the user insisting, and exits immediately. Once the command is
//! running, `run` itself decides what each signal means for the child.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::commands::emit_event;

/// A repeat of the same signal this soon is one keypress delivered twice —
/// by the terminal to the whole process group, and again by a wrapper such
/// as the npm launcher — not the user insisting.
const ECHO_WINDOW: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Interrupt,
    Terminate,
    // Windows only delivers Ctrl-C.
    #[cfg_attr(not(unix), allow(dead_code))]
    Hangup,
}

impl Kind {
    /// The shell's 128+N convention for a process ended by this signal.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Hangup => 129,
            Self::Interrupt => 130,
            Self::Terminate => 143,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Hangup => "SIGHUP",
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
        }
    }
}

/// Every signal so far: how many, and the latest.
#[derive(Debug, Clone, Copy, Default)]
pub struct Received {
    pub count: u32,
    pub last: Option<Kind>,
}

pub struct Signals {
    received: watch::Receiver<Received>,
    starting: Arc<AtomicBool>,
}

impl Signals {
    /// Take over SIGINT, SIGTERM and SIGHUP (Ctrl-C on Windows) for the rest
    /// of the process.
    pub fn listen(json: bool) -> Self {
        let (sender, received) = watch::channel(Received::default());
        let starting = Arc::new(AtomicBool::new(true));
        let still_starting = Arc::clone(&starting);
        tokio::spawn(async move {
            let Some(mut listener) = Listener::new() else {
                return;
            };
            let mut previous: Option<(Kind, Instant)> = None;
            while let Some(kind) = listener.recv().await {
                let now = Instant::now();
                if is_echo(previous, kind, now) {
                    continue;
                }
                previous = Some((kind, now));
                sender.send_modify(|received| {
                    received.count += 1;
                    received.last = Some(kind);
                });
                if !still_starting.load(Ordering::SeqCst) {
                    continue;
                }
                if sender.borrow().count == 1 {
                    emit_event(
                        json,
                        serde_json::json!({ "event": "interrupted", "signal": kind.name() }),
                        "popgres: interrupted — finishing startup so the database can be torn \
                         down (interrupt again to exit immediately)",
                    );
                } else {
                    emit_event(
                        json,
                        serde_json::json!({ "event": "aborted", "signal": kind.name() }),
                        "popgres: interrupted again — exiting now; anything already started \
                         is left behind (`popgres down` stops it)",
                    );
                    std::process::exit(kind.exit_code());
                }
            }
        });
        Self { received, starting }
    }

    /// Everything that has arrived so far.
    pub fn received(&self) -> Received {
        *self.received.borrow()
    }

    /// Startup is over: from here on the command's lifecycle handles signals.
    pub fn end_startup(&self) {
        self.starting.store(false, Ordering::SeqCst);
    }

    /// Resolves once `count` signals in all have arrived — never, if this
    /// platform could not install the handlers.
    pub async fn reached(&self, count: u32) -> Received {
        let mut received = self.received.clone();
        let reached = received
            .wait_for(|received| received.count >= count)
            .await
            .map(|received| *received);
        match reached {
            Ok(received) => received,
            Err(_) => std::future::pending().await,
        }
    }
}

fn is_echo(previous: Option<(Kind, Instant)>, kind: Kind, now: Instant) -> bool {
    previous.is_some_and(|(previous, at)| previous == kind && now.duration_since(at) < ECHO_WINDOW)
}

#[cfg(unix)]
struct Listener {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Listener {
    fn new() -> Option<Self> {
        use tokio::signal::unix::{signal, SignalKind};
        Some(Self {
            interrupt: signal(SignalKind::interrupt()).ok()?,
            terminate: signal(SignalKind::terminate()).ok()?,
            hangup: signal(SignalKind::hangup()).ok()?,
        })
    }

    async fn recv(&mut self) -> Option<Kind> {
        tokio::select! {
            Some(()) = self.interrupt.recv() => Some(Kind::Interrupt),
            Some(()) = self.terminate.recv() => Some(Kind::Terminate),
            Some(()) = self.hangup.recv() => Some(Kind::Hangup),
            else => None,
        }
    }
}

#[cfg(windows)]
struct Listener {
    ctrl_c: tokio::signal::windows::CtrlC,
}

#[cfg(windows)]
impl Listener {
    fn new() -> Option<Self> {
        Some(Self {
            ctrl_c: tokio::signal::windows::ctrl_c().ok()?,
        })
    }

    async fn recv(&mut self) -> Option<Kind> {
        self.ctrl_c.recv().await.map(|()| Kind::Interrupt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quick_repeat_of_the_same_signal_is_an_echo() {
        let at = Instant::now();
        let first = Some((Kind::Interrupt, at));
        assert!(is_echo(
            first,
            Kind::Interrupt,
            at + Duration::from_millis(5)
        ));
        // A different signal, or the same one a human-scale moment later,
        // is a genuine second request.
        assert!(!is_echo(
            first,
            Kind::Terminate,
            at + Duration::from_millis(5)
        ));
        assert!(!is_echo(
            first,
            Kind::Interrupt,
            at + Duration::from_secs(1)
        ));
        assert!(!is_echo(None, Kind::Interrupt, at));
    }

    #[test]
    fn exit_codes_follow_the_shell_convention() {
        assert_eq!(Kind::Interrupt.exit_code(), 130);
        assert_eq!(Kind::Terminate.exit_code(), 143);
        assert_eq!(Kind::Hangup.exit_code(), 129);
    }
}
