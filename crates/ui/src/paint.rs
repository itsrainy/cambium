//! ANSI, or nothing at all.

use crate::event::Style;

#[derive(Copy, Clone, Debug)]
pub struct Paint {
    on: bool,
}

impl Paint {
    pub fn new(on: bool) -> Self {
        Self { on }
    }

    pub fn is_on(&self) -> bool {
        self.on
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

    pub fn style(&self, s: &str, style: Style) -> String {
        match style {
            Style::Plain => s.to_string(),
            Style::Dim => self.dim(s),
            Style::Bold => self.bold(s),
            Style::Cyan => self.cyan(s),
            Style::Green => self.green(s),
            Style::Yellow => self.yellow(s),
        }
    }
}
