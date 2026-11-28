//! The app's words, from the core's string tables (7 languages).

use podbag_desktop_core::strings::{self, LANGUAGES};

/// The language shown, and lookups in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tr {
    pub lang: &'static str,
}

impl Tr {
    pub fn new(lang: &'static str) -> Tr {
        Tr { lang }
    }

    /// The text for `key`; English when the language lacks it, the key when both do.
    pub fn t(&self, key: &str) -> String {
        strings::text(self.lang, key, None)
    }

    /// The text for `key` with each `{name}` filled in.
    pub fn tv(&self, key: &str, vars: &[(&str, &str)]) -> String {
        strings::fill(self.lang, key, vars)
    }

    /// The text for `key` with `{n}` filled in.
    pub fn tn(&self, key: &str, n: usize) -> String {
        strings::text(self.lang, key, Some(n))
    }
}

/// Each language with its own name, in the order of the settings menu.
pub fn language_names() -> Vec<(&'static str, String)> {
    LANGUAGES
        .iter()
        .map(|l| (*l, strings::text(l, "language.name", None)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_come_from_the_tables() {
        let en = Tr::new("en");
        assert_eq!(en.t("nav.containers"), "Containers");
        assert_eq!(en.tv("done.stop", &[("name", "web")]), "Stopped web");
        assert_eq!(
            en.tn("confirm.stopVm", 2),
            "Stop the VM? Containers that will stop: 2."
        );
        assert_eq!(Tr::new("ko").t("nav.containers"), "컨테이너");
        let names = language_names();
        assert_eq!(names.len(), 7);
        assert_eq!(names[0], ("en", "English".to_string()));
    }
}
