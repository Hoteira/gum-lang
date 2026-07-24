static MODULES: &[(&str, &str)] = &[("gum.defaults", include_str!("../../std/defaults.gum"))];

pub fn is_std_path(path: &str) -> bool {
    path.starts_with("gum.")
}

pub fn lookup(module: &str) -> Option<&'static str> {
    let key = module.to_ascii_lowercase();
    MODULES.iter().find(|(k, _)| *k == key).map(|(_, src)| *src)
}

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

pub fn known_modules() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = MODULES.iter().map(|(k, _)| *k).collect();
    v.sort_unstable();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            split_module("gum.defaults"),
            Some(("gum.defaults".to_string(), None))
        );
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
