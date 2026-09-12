//! The little that every renderer needs to know about the terminal.

use std::io::IsTerminal;

pub fn stdout_tty() -> bool {
    std::io::stdout().is_terminal()
}

pub fn stderr_tty() -> bool {
    std::io::stderr().is_terminal()
}

/// Colour is for a terminal only, and never when NO_COLOR is set.
pub fn colour(is_tty: bool) -> bool {
    is_tty && std::env::var_os("NO_COLOR").is_none()
}

pub struct Paint {
    on: bool,
}

impl Paint {
    pub fn new(on: bool) -> Self {
        Self { on }
    }
    fn wrap(&self, code: &str, s: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn dim(&self, s: &str) -> String {
        self.wrap("2", s)
    }
    pub fn bold(&self, s: &str) -> String {
        self.wrap("1", s)
    }
    pub fn green(&self, s: &str) -> String {
        self.wrap("32", s)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.wrap("33", s)
    }
    pub fn cyan(&self, s: &str) -> String {
        self.wrap("36", s)
    }
}

/// A screen that a renderer needing a real terminal refuses to draw on.
pub fn require_tty(what: &str) -> bool {
    if stdout_tty() {
        return true;
    }
    eprintln!(
        "ui-feel {what}: this demo draws on a terminal, and stdout is not one. \
         Run it from a terminal, or under `script -q /dev/null ui-feel {what}`. \
         Falling back to the plain renderer so the run still completes."
    );
    false
}
