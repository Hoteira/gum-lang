// The standard library, compiled into the binary rather than read off disk.
// gumc is a single executable with no install step and no search path: a `use gum.*` resolves the same way from any directory, on any machine, with nothing to point at.

// A `use` path is a module followed by the symbol wanted out of it, so `gum.defaults.Account` is the class Account from module gum.defaults.
// Keys are the module path lowercased; module names are case-insensitive, symbols are matched against the module's own declarations.
static MODULES: &[(&str, &str)] = &[("gum.defaults", include_str!("../../std/defaults.gum"))];

pub fn is_std_path(path: &str) -> bool {
    path.starts_with("gum.")
}

pub fn lookup(module: &str) -> Option<&'static str> {
    let key = module.to_ascii_lowercase();
    MODULES.iter().find(|(k, _)| *k == key).map(|(_, src)| *src)
}

// Splits a use path into the longest prefix that names a module, plus the symbol after it.
// Tried longest-first so a module whose name happens to prefix another still wins, and so `use gum.defaults` with no symbol is a module import of the whole thing.
pub fn split_module(path: &str) -> Option<(String, Option<String>)> {
    if lookup(path).is_some() {
        return Some((path.to_string(), None));
    }
    let (head, sym) = path.rsplit_once('.')?;
    if lookup(head).is_some() {
        return Some((head.to_string(), Some(sym.to_string())));
    }
    None
}

// Every module path, for the "available" line on a bad import.
pub fn known_modules() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = MODULES.iter().map(|(k, _)| *k).collect();
    v.sort_unstable();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    // The spellings contracts actually write. Each names a symbol out of gum.defaults, not a file, which is the whole point of split_module.
    #[test]
    fn the_paths_contracts_write_all_split_into_module_and_symbol() {
        for (p, want) in [
            ("gum.defaults.Account", "Account"),
            ("gum.defaults.Block", "Block"),
            ("gum.defaults.String", "String"),
            ("gum.defaults.Message", "Message"),
            ("gum.defaults.crypto", "crypto"),
            ("gum.defaults.vec", "vec"),
        ] {
            let (m, s) = split_module(p).unwrap_or_else(|| panic!("{} did not split", p));
            assert_eq!(m, "gum.defaults", "{}", p);
            assert_eq!(s.as_deref(), Some(want), "{}", p);
        }
    }

    #[test]
    fn a_bare_module_path_has_no_symbol() {
        assert_eq!(split_module("gum.defaults"), Some(("gum.defaults".to_string(), None)));
    }

    #[test]
    fn an_unknown_module_does_not_split() {
        assert!(split_module("gum.nope.Thing").is_none());
        assert!(split_module("gum.defaults.Account.deeper").is_none());
    }

    #[test]
    fn every_embedded_module_is_non_empty() {
        for (k, src) in MODULES {
            assert!(!src.trim().is_empty(), "{} embedded empty", k);
        }
    }
}
