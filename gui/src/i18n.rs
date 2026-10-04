//! Translation layer. Every visible string comes from a Fluent file by key, through
//! [`tr!`](crate::tr): `tr!("render-button")`, `tr!("changes-pending", count = n)`.
//! English is built in and is the fallback for keys a translation lacks.
//!
//! Numbers passed as arguments are formatted here too (digit grouping, padding), so
//! translations control how counters and times look.

use fluent_bundle::concurrent::FluentBundle;
use fluent_bundle::{FluentArgs, FluentResource, FluentValue};
use std::sync::{LazyLock, RwLock};
use unic_langid::LanguageIdentifier;

/// (language tag, Fluent source); the first one is the fallback
const LANGUAGES: &[(&str, &str)] = &[("en", include_str!("../i18n/en.ftl"))];

struct Catalog {
    /// selected language first, then the fallback
    bundles: Vec<FluentBundle<FluentResource>>,
}

static CATALOG: LazyLock<RwLock<Catalog>> = LazyLock::new(|| RwLock::new(Catalog::new(LANGUAGES[0].0)));

impl Catalog {
    fn new(tag: &str) -> Self {
        let (fallback, rest) = LANGUAGES.split_first().expect("a fallback language");
        let mut bundles: Vec<_> = rest.iter().filter(|(lang, _)| *lang == tag).map(|(lang, src)| bundle(lang, src)).collect();
        bundles.push(bundle(fallback.0, fallback.1));
        Self { bundles }
    }

    fn format(&self, key: &str, args: Option<&FluentArgs>) -> String {
        for b in &self.bundles {
            if let Some(pattern) = b.get_message(key).and_then(|m| m.value()) {
                let mut errors = Vec::new();
                let text = b.format_pattern(pattern, args, &mut errors);
                debug_assert!(errors.is_empty(), "message {key}: {errors:?}");
                return text.into_owned();
            }
        }
        debug_assert!(false, "missing message {key}");
        key.to_string()
    }
}

fn bundle(tag: &str, source: &str) -> FluentBundle<FluentResource> {
    let lang: LanguageIdentifier = tag.parse().expect("language tag");
    let resource = FluentResource::try_new(source.to_string()).unwrap_or_else(|(_, e)| panic!("{tag}.ftl: {e:?}"));
    let mut b = FluentBundle::new_concurrent(vec![lang]);
    b.add_resource(resource).unwrap_or_else(|e| panic!("{tag}.ftl: {e:?}"));
    b.add_builtins().expect("builtins");
    // isolation marks only matter for right-to-left text, which is not supported;
    // egui would draw them as boxes
    b.set_use_isolating(false);
    b.set_formatter(Some(format_number::<',', '.', _>));
    b
}

/// Select the UI language (falls back to English for unknown tags and missing keys).
#[allow(dead_code)]
pub fn set_language(tag: &str) {
    *CATALOG.write().unwrap() = Catalog::new(tag);
}

pub fn tr(key: &str, args: Option<&FluentArgs>) -> String {
    CATALOG.read().unwrap().format(key, args)
}

/// Numbers with digit grouping and the `NUMBER()` options fluent-bundle leaves out
/// (`minimumIntegerDigits`, `useGrouping`, fraction digits).
fn format_number<const GROUP: char, const DECIMAL: char, M>(value: &FluentValue, _: &M) -> Option<String> {
    let FluentValue::Number(n) = value else { return None };
    let o = &n.options;
    let digits = o.maximum_fraction_digits.or(o.minimum_fraction_digits).unwrap_or(if n.value.fract() == 0.0 { 0 } else { 3 });
    let text = format!("{:.*}", digits, n.value.abs());
    let (int, mut frac) = text.split_once('.').unwrap_or((&text, ""));
    let min_frac = o.minimum_fraction_digits.unwrap_or(0);
    while frac.len() > min_frac && frac.ends_with('0') {
        frac = &frac[..frac.len() - 1];
    }
    let int = format!("{int:0>width$}", width = o.minimum_integer_digits.unwrap_or(1));
    let mut out = String::new();
    if n.value < 0.0 && text.bytes().any(|b| (b'1'..=b'9').contains(&b)) {
        out.push('-');
    }
    for (i, c) in int.chars().enumerate() {
        if o.use_grouping && i > 0 && (int.len() - i) % 3 == 0 {
            out.push(GROUP);
        }
        out.push(c);
    }
    if !frac.is_empty() {
        out.push(DECIMAL);
        out.push_str(frac);
    }
    Some(out)
}

/// `tr!("key")` or `tr!("key", name = value, ...)`; values are numbers or strings.
#[macro_export]
macro_rules! tr {
    ($key:literal) => {
        $crate::i18n::tr($key, None)
    };
    ($key:literal, $($name:ident = $value:expr),+ $(,)?) => {{
        let mut args = fluent_bundle::FluentArgs::new();
        $(args.set(stringify!($name), $value);)+
        $crate::i18n::tr($key, Some(&args))
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    fn ftl_keys(source: &str) -> BTreeSet<String> {
        source
            .lines()
            .filter(|l| l.starts_with(|c: char| c.is_ascii_alphabetic()))
            .filter_map(|l| l.split_once('=').map(|(k, _)| k.trim().to_string()))
            .collect()
    }

    /// keys of every `tr!` call in the sources (this file only defines the layer)
    fn used_keys(dir: &Path, keys: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                used_keys(&path, keys);
            } else if path.extension().is_some_and(|e| e == "rs") && !path.ends_with("i18n.rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                for part in text.split("tr!(\"").skip(1) {
                    keys.insert(part.split('"').next().unwrap().to_string());
                }
            }
        }
    }

    #[test]
    fn every_language_parses() {
        for (tag, source) in LANGUAGES {
            bundle(tag, source);
        }
    }

    #[test]
    fn english_has_exactly_the_used_keys() {
        let defined = ftl_keys(LANGUAGES[0].1);
        let mut used = BTreeSet::new();
        used_keys(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut used);
        let missing: Vec<_> = used.difference(&defined).collect();
        let unused: Vec<_> = defined.difference(&used).collect();
        assert!(missing.is_empty(), "keys used in code but missing from en.ftl: {missing:?}");
        assert!(unused.is_empty(), "keys in en.ftl that no code uses: {unused:?}");
    }

    #[test]
    fn translations_only_have_known_keys() {
        let english = ftl_keys(LANGUAGES[0].1);
        for (tag, source) in &LANGUAGES[1..] {
            let extra: Vec<_> = ftl_keys(source).difference(&english).cloned().collect();
            assert!(extra.is_empty(), "{tag}.ftl has keys English lacks: {extra:?}");
        }
    }

    #[test]
    fn plurals_and_numbers() {
        let source = "
changes = { $count ->
    [one] { $count } change
   *[other] { $count } changes
}
padded = { NUMBER($n, minimumIntegerDigits: 2) }
fraction = { NUMBER($n, minimumFractionDigits: 2) }
";
        let catalog = Catalog { bundles: vec![bundle("en", source)] };
        let fmt = |key: &str, name: &str, value: f64| {
            let mut args = FluentArgs::new();
            args.set(name.to_string(), value);
            catalog.format(key, Some(&args))
        };
        assert_eq!(fmt("changes", "count", 1.0), "1 change");
        assert_eq!(fmt("changes", "count", 1209.0), "1,209 changes");
        assert_eq!(fmt("changes", "count", -1234567.25), "-1,234,567.25 changes");
        assert_eq!(fmt("padded", "n", 7.0), "07");
        assert_eq!(fmt("fraction", "n", 2.5), "2.50");
        assert_eq!(fmt("fraction", "n", -0.001), "0.00");
    }
}
