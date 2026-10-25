//! [i18n] The language of etcds's messages - errors, log lines, the command line's help.
//!
//! The first of: [set_lang]; the `lang` value of the logger given to
//! [crate::cluster::EtcdNode::init] - `o!("lang" => "ru")`, so an embedder already
//! handing over its logger needs nothing more; the environment's LANG; English. A
//! locale such as `ru_RU.UTF-8` is taken by its language, a language without its
//! messages is English, and an empty one is none.
//!
//! The messages are in locales/<language>.yml, one file per language, each with every
//! key on the same line: en, ru, and - not reviewed - fr, de, zh (Simplified Chinese), ja. An error's starts with its code,
//! `ETCD-Ennn`, the same in every language, so it can be looked up whatever the
//! language - a client of the gRPC API gets the server's language, and the code.
//!
//! The language is the process's, not a call's: one node, one language.

use std::sync::RwLock;

/// set_lang's
static LANG: RwLock<Option<String>> = RwLock::new(None);
/// the logger's, read by EtcdNode::init
static LOGGER_LANG: RwLock<Option<String>> = RwLock::new(None);

/// The language of the messages from here on: `en`, `ru`, `fr`, `de`, `zh`, `ja`, or a
/// locale of one (`ru_RU.UTF-8`). Wins over the logger's and LANG; an empty one is
/// none, and leaves them to it.
pub fn set_lang(lang: &str) {
    if let Ok(mut l) = LANG.write() {
        *l = Some(language(lang));
    }
}

/// The logger's language, or none - when the node is given a logger without it
pub(crate) fn set_logger_lang(lang: Option<String>) {
    if let Ok(mut l) = LOGGER_LANG.write() {
        *l = lang.map(|l| language(&l));
    }
}

/// The language the messages are in now
pub fn lang() -> String {
    choose(
        LANG.read().ok().and_then(|l| l.clone()),
        LOGGER_LANG.read().ok().and_then(|l| l.clone()),
        std::env::var("LANG").ok(),
    )
}

/// the first language given - an empty one is not given
fn choose(set: Option<String>, logger: Option<String>, env: Option<String>) -> String {
    let given = |l: &String| !l.is_empty();
    set.filter(given)
        .or(logger.filter(given))
        .or_else(|| env.map(|e| language(&e)).filter(given))
        .unwrap_or_else(|| "en".to_string())
}

/// a locale's language: its first two letters, lower case
fn language(locale: &str) -> String {
    locale.trim().chars().take(2).collect::<String>().to_lowercase()
}

/// The logger's `lang` value, as o!("lang" => ...) set it
pub(crate) fn lang_of(logger: &slog::Logger) -> Option<String> {
    struct Lang(Option<String>);
    impl slog::Serializer for Lang {
        fn emit_arguments(&mut self, key: slog::Key, val: &std::fmt::Arguments) -> slog::Result {
            if key == "lang" {
                self.0 = Some(val.to_string());
            }
            Ok(())
        }
    }
    let mut found = Lang(None);
    let rs = slog::record_static!(slog::Level::Info, "");
    let args = format_args!("");
    let _ = slog::KV::serialize(logger.list(), &slog::Record::new(&rs, &args, slog::b!()), &mut found);
    found.0
}

/// [i18n] The message `key` (locales/<language>.yml) in the language of [lang], its %{name}
/// placeholders filled from `args`: tr! for a crate that has not the messages - the
/// etcds binary's
pub fn message(key: &str, args: &[(&str, &str)]) -> String {
    let mut m = rust_i18n::t!(key, locale = &lang()).into_owned();
    for (name, value) in args {
        m = m.replace(&format!("%{{{}}}", name), value);
    }
    m
}

/// [i18n] The message `key` in the language of [lang], each %{name} placeholder filled
macro_rules! tr {
    ($key:expr $(, $name:ident = $value:expr)* $(,)?) => {
        rust_i18n::t!($key, locale = &$crate::i18n::lang() $(, $name = $value)*).into_owned()
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [i18n] set_lang over the logger's, the logger's over LANG, LANG over English
    #[test]
    fn the_language_is_the_first_one_given() {
        let s = |v: &str| Some(v.to_string());
        assert_eq!(choose(s("de"), s("fr"), s("ru_RU.UTF-8")), "de");
        assert_eq!(choose(None, s("fr"), s("ru_RU.UTF-8")), "fr");
        assert_eq!(choose(None, None, s("ru_RU.UTF-8")), "ru");
        assert_eq!(choose(None, None, None), "en");
        assert_eq!(choose(None, None, s("")), "en");
        // an empty one is none: it leaves the language to the next
        assert_eq!(choose(s(""), s("fr"), s("ru_RU.UTF-8")), "fr");
        assert_eq!(choose(s(""), s(""), s("ru_RU.UTF-8")), "ru");
        assert_eq!(language("JA_jp"), "ja");
    }

    /// [i18n] a language without messages, or a key without one in it, is English
    #[test]
    fn a_message_falls_back_to_english() {
        let en = rust_i18n::t!("peer.half", locale = "en", connected = 1, total = 3).into_owned();
        assert!(en.starts_with("ETCD-E"), "{}", en);
        assert_eq!(rust_i18n::t!("peer.half", locale = "xx", connected = 1, total = 3).into_owned(), en);
        assert_ne!(rust_i18n::t!("peer.half", locale = "ru", connected = 1, total = 3).into_owned(), en);
    }

    #[test]
    fn the_logger_carries_its_language() {
        let root = slog::Logger::root(slog::Discard, slog::o!("app" => "etcd"));
        assert_eq!(lang_of(&root), None);
        assert_eq!(lang_of(&root.new(slog::o!("lang" => "ja"))), Some("ja".to_string()));
    }

    /// the languages, one file each: locales/<language>.yml
    const LANGUAGES: [&str; 6] = ["de", "en", "fr", "ja", "ru", "zh"];

    /// locales/<language>.yml's keys, in the file's order, with their messages
    fn file(lang: &str) -> Vec<(String, String)> {
        let f = format!("{}/locales/{}.yml", env!("CARGO_MANIFEST_DIR"), lang);
        let src = std::fs::read_to_string(&f).unwrap_or_else(|e| panic!("{}: {}", f, e));
        // serde_json's map is sorted: the order is the file's lines
        let order: Vec<&str> = src.lines().filter(|l| !l.starts_with('#') && !l.starts_with(' ') && !l.is_empty())
            .filter_map(|l| l.split(':').next()).filter(|k| *k != "_version").collect();
        let y: serde_json::Value = serde_saphyr::from_str(&src).unwrap_or_else(|e| panic!("{}: {}", f, e));
        let y = y.as_object().unwrap();
        assert_eq!(order.len() + 1, y.len(), "{}: a key twice, or not one to a line", f);
        order.iter().map(|k| (k.to_string(), y[*k].as_str().unwrap_or_else(|| panic!("{}: {}", f, k)).to_string())).collect()
    }

    /// locales/*.yml, as key => language => message
    fn messages() -> std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> {
        let mut all: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> = Default::default();
        for l in LANGUAGES {
            for (k, m) in file(l) {
                all.entry(k).or_default().insert(l.to_string(), m);
            }
        }
        all
    }

    /// [i18n] one file per language and no other, each with the same keys on the same
    /// lines as English's - a message is found at one line number in every language
    #[test]
    fn every_language_has_its_file_with_the_keys_in_order() {
        let dir = format!("{}/locales", env!("CARGO_MANIFEST_DIR"));
        let mut files: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|f| f.unwrap().file_name().into_string().unwrap()).collect();
        files.sort();
        assert_eq!(files, LANGUAGES.map(|l| format!("{}.yml", l)));
        // each key, with its line number
        let keys = |l: &str| std::fs::read_to_string(format!("{}/{}.yml", dir, l)).unwrap().lines().enumerate()
            .filter(|(_, line)| !line.starts_with('#') && !line.starts_with("_version"))
            .filter_map(|(n, line)| line.split_once(':').map(|(k, _)| (n, k.to_string())))
            .collect::<Vec<_>>();
        for l in LANGUAGES {
            assert_eq!(keys(l), keys("en"), "{}: a key on another line than English's", l);
        }
    }

    /// [i18n] every message in every language, with the same placeholders; an error's
    /// code the same in every language, and no two errors with one code
    #[test]
    fn every_message_is_in_every_language() {
        let placeholders = |m: &str| {
            let mut p: Vec<String> = m.split("%{").skip(1).map(|r| r.split('}').next().unwrap().to_string()).collect();
            p.sort();
            p
        };
        let mut codes = std::collections::BTreeMap::new();
        for (key, langs) in messages() {
            let en = &langs["en"];
            assert_eq!(langs.keys().map(String::as_str).collect::<Vec<_>>(), ["de", "en", "fr", "ja", "ru", "zh"], "{}", key);
            let code = en.split(' ').next().filter(|c| c.starts_with("ETCD-E"));
            for (l, m) in &langs {
                assert_eq!(placeholders(m), placeholders(en), "{} {}", key, l);
                assert_eq!(m.split(' ').next().filter(|c| c.starts_with("ETCD-E")), code, "{} {}: the code", key, l);
            }
            if let Some(c) = code {
                assert!(codes.insert(c.to_string(), key.clone()).is_none(), "{} for {} and {}", c, codes[c], key);
            }
        }
        assert!(codes.len() > 20, "{}", codes.len());
    }

    /// `module.name`: lower case, `_` and `.` - not this test's own code, nor include_str!'s path
    fn is_key(k: &str) -> bool {
        k.contains('.') && !k.starts_with('.') && k.chars().all(|c| c.is_ascii_lowercase() || c == '_' || c == '.')
    }

    /// [i18n] every key the code asks for is in locales/, and every message there asked for
    #[test]
    fn every_key_used_has_its_message_and_every_message_a_use() {
        let all = messages();
        let mut used = std::collections::BTreeSet::new();
        let mut files = vec![std::path::PathBuf::from(format!("{}/src", env!("CARGO_MANIFEST_DIR")))];
        while let Some(f) = files.pop() {
            if f.is_dir() {
                files.extend(std::fs::read_dir(&f).unwrap().map(|e| e.unwrap().path()));
                continue;
            }
            let src = std::fs::read_to_string(&f).unwrap();
            // each call - tr!, or message( in the binary - the key, a string literal, first
            for call in src.split("tr!(").skip(1).chain(src.split("message(").skip(1)) {
                let Some(key) = call.trim_start().strip_prefix('"').and_then(|r| r.split('"').next())
                    .filter(|k| is_key(k)) else { continue };
                assert!(all.contains_key(key), "{}: no message {}", f.display(), key);
                used.insert(key.to_string());
            }
        }
        // the command line's help: one per argument, asked for by the argument's id (cli.rs)
        let unused: Vec<&String> = all.keys().filter(|k| !used.contains(*k) && !k.starts_with("help.")).collect();
        assert!(unused.is_empty(), "messages nothing asks for: {:?}", unused);
    }
}
