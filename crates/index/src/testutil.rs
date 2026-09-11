//! Fixtures for the inline tests. The integration tests have their own copy
//! in `tests/support`, since a test binary cannot reach a `cfg(test)` item
//! of the library.

use crate::loads::FileText;

/// A tiny in-memory checkout: repo-relative path to contents.
pub(crate) struct Files(pub(crate) Vec<(&'static str, &'static str)>);

impl FileText for Files {
    fn text(&self, path: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, t)| t.to_string())
    }
}
