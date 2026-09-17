//! Rule definitions and the directory-level `.commentlint.toml` configuration.
//!
//! Configuration is layered. The defaults compiled in from
//! `data/defaults.yaml` are the base layer. Every `.commentlint.toml` found
//! between the working directory and the directory containing a file is then
//! applied in order, outermost first, so the configuration closest to the
//! file being evaluated has the final say.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The name of the configuration file looked up in each directory.
pub const CONFIG_FILE_NAME: &str = ".commentlint.toml";

/// The default threshold below which a Noul answer counts as "false".
pub const DEFAULT_THRESHOLD: f64 = 0.7;

/// The default model sent to TypeSafe.ai.
pub const DEFAULT_MODEL: &str = "jev-latest";

/// The default minimum number of words a comment needs before it is evaluated.
pub const DEFAULT_MIN_WORDS: usize = 3;

/// Yes/no criteria for a Noul rule.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Criteria {
    /// What a "yes" answer means.
    #[serde(rename = "true")]
    pub yes: String,
    /// What a "no" answer means.
    #[serde(rename = "false")]
    pub no: String,
}

/// A single Noul rule, as written in configuration or built in.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Rule {
    /// The yes/no question asked about each comment.
    pub instructions: String,
    /// Descriptions of the yes and no outcomes.
    pub criteria: Criteria,
    /// The comment fails this rule when the returned probability is below
    /// this value. Falls back to the file-level `threshold`, then 0.5.
    #[serde(default)]
    pub threshold: Option<f64>,
}

/// The shape of both `data/defaults.yaml` and `.commentlint.toml`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    /// Rule ids to disable for this directory and its descendants.
    #[serde(default)]
    pub disable: Vec<String>,
    /// Rule ids to re-enable after an outer directory disabled them.
    #[serde(default)]
    pub enable: Vec<String>,
    /// Additional rules, or overrides of default rules by id.
    #[serde(default)]
    pub rules: BTreeMap<String, Rule>,
    /// Default threshold for rules that do not set their own.
    #[serde(default)]
    pub threshold: Option<f64>,
    /// The TypeSafe.ai model name.
    #[serde(default)]
    pub model: Option<String>,
    /// Comments with fewer words than this are not evaluated.
    #[serde(default)]
    pub min_words: Option<usize>,
    /// Comment prefixes that mark tool directives; replaces the default list.
    #[serde(default)]
    pub ignore_prefixes: Option<Vec<String>>,
}

/// The fully resolved configuration that applies to one file.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    /// Enabled rules, keyed by id, in a stable order.
    pub rules: BTreeMap<String, Rule>,
    pub threshold: f64,
    pub model: String,
    pub min_words: usize,
    pub ignore_prefixes: Vec<String>,
}

impl Resolved {
    /// The effective threshold for a rule.
    pub fn threshold_for(&self, rule: &Rule) -> f64 {
        rule.threshold.unwrap_or(self.threshold)
    }

    /// Whether a cleaned comment should be sent for evaluation at all.
    pub fn should_evaluate(&self, text: &str) -> bool {
        let words = text.split_whitespace().count();
        if words < self.min_words {
            return false;
        }
        let head = text.trim_start();
        !self.ignore_prefixes.iter().any(|p| head.starts_with(p.as_str()))
    }
}

/// The default rules and settings, embedded from `data/defaults.yaml` at
/// compile time.
pub const DEFAULTS_YAML: &str = include_str!("../data/defaults.yaml");

/// Parses the embedded defaults.
pub fn defaults() -> Result<ConfigFile> {
    parse_yaml(DEFAULTS_YAML).context("the built-in data/defaults.yaml is invalid")
}

/// Parses a `.commentlint.toml` file.
pub fn parse(contents: &str) -> Result<ConfigFile> {
    let cfg: ConfigFile = toml::from_str(contents)?;
    validate(&cfg)?;
    Ok(cfg)
}

/// Parses YAML with the same schema as `.commentlint.toml`.
pub fn parse_yaml(contents: &str) -> Result<ConfigFile> {
    let cfg: ConfigFile = serde_yaml_ng::from_str(contents)?;
    validate(&cfg)?;
    Ok(cfg)
}

fn validate(cfg: &ConfigFile) -> Result<()> {
    for (id, rule) in &cfg.rules {
        if rule.instructions.trim().is_empty() {
            bail!("rule `{id}` has empty instructions");
        }
        if let Some(t) = rule.threshold
            && !(0.0..=1.0).contains(&t)
        {
            bail!("rule `{id}` threshold {t} is outside 0..=1");
        }
    }
    if let Some(t) = cfg.threshold
        && !(0.0..=1.0).contains(&t)
    {
        bail!("threshold {t} is outside 0..=1");
    }
    Ok(())
}

/// Applies an ordered list of configuration layers. The first layer is
/// normally the packaged defaults; later layers override earlier ones.
pub fn resolve(layers: &[ConfigFile]) -> Resolved {
    let mut known: BTreeMap<String, Rule> = BTreeMap::new();
    let mut enabled: BTreeMap<String, ()> = BTreeMap::new();
    let mut threshold = DEFAULT_THRESHOLD;
    let mut model = DEFAULT_MODEL.to_string();
    let mut min_words = DEFAULT_MIN_WORDS;
    let mut ignore_prefixes: Vec<String> = Vec::new();

    for layer in layers {
        for (id, rule) in &layer.rules {
            known.insert(id.clone(), rule.clone());
            enabled.insert(id.clone(), ());
        }
        for id in &layer.disable {
            enabled.remove(id);
        }
        for id in &layer.enable {
            if known.contains_key(id) {
                enabled.insert(id.clone(), ());
            }
        }
        if let Some(t) = layer.threshold {
            threshold = t;
        }
        if let Some(m) = &layer.model {
            model = m.clone();
        }
        if let Some(w) = layer.min_words {
            min_words = w;
        }
        if let Some(p) = &layer.ignore_prefixes {
            ignore_prefixes = p.clone();
        }
    }

    let rules = known.into_iter().filter(|(id, _)| enabled.contains_key(id)).collect();
    Resolved {
        rules,
        threshold,
        model,
        min_words,
        ignore_prefixes,
    }
}

/// Loads and caches the layered configuration for directories.
pub struct Loader {
    root: PathBuf,
    defaults: ConfigFile,
    files: HashMap<PathBuf, Option<ConfigFile>>,
    resolved: HashMap<PathBuf, Resolved>,
}

impl Loader {
    /// Creates a loader that applies `defaults` first and stops searching
    /// upward for `.commentlint.toml` files at `root` (normally the working
    /// directory). Callers normally pass [`defaults()`].
    pub fn new(root: PathBuf, defaults: ConfigFile) -> Self {
        Self {
            root,
            defaults,
            files: HashMap::new(),
            resolved: HashMap::new(),
        }
    }

    /// Resolves the configuration for the directory containing `file`.
    pub fn for_file(&mut self, file: &Path) -> Result<Resolved> {
        let dir = match file.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        self.for_dir(&dir)
    }

    /// Resolves the configuration for a directory.
    pub fn for_dir(&mut self, dir: &Path) -> Result<Resolved> {
        let abs = absolute(dir)?;
        if let Some(r) = self.resolved.get(&abs) {
            return Ok(r.clone());
        }
        let root = absolute(&self.root)?;

        // Walk from the directory up to the root, collecting config files,
        // then apply them outermost first.
        let mut chain: Vec<PathBuf> = Vec::new();
        let mut cur = Some(abs.as_path());
        while let Some(d) = cur {
            chain.push(d.to_path_buf());
            if d == root || !d.starts_with(&root) {
                break;
            }
            cur = d.parent();
        }
        chain.reverse();

        let mut layers = vec![self.defaults.clone()];
        for d in &chain {
            if let Some(cfg) = self.file_for(d)? {
                layers.push(cfg);
            }
        }
        let resolved = resolve(&layers);
        self.resolved.insert(abs, resolved.clone());
        Ok(resolved)
    }

    fn file_for(&mut self, dir: &Path) -> Result<Option<ConfigFile>> {
        if let Some(c) = self.files.get(dir) {
            return Ok(c.clone());
        }
        let path = dir.join(CONFIG_FILE_NAME);
        let cfg = match std::fs::read_to_string(&path) {
            Ok(s) => Some(parse(&s).with_context(|| format!("invalid {}", path.display()))?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
        };
        self.files.insert(dir.to_path_buf(), cfg.clone());
        Ok(cfg)
    }
}

fn absolute(p: &Path) -> Result<PathBuf> {
    let abs = std::path::absolute(p).with_context(|| format!("cannot resolve {}", p.display()))?;
    // Normalise `.` and `..` components without touching the filesystem so
    // that paths compare equal regardless of how they were written.
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> ConfigFile {
        super::defaults().unwrap()
    }

    #[test]
    fn embedded_defaults_define_active_voice() {
        let r = resolve(&[defaults()]);
        let rule = &r.rules["active_voice"];
        assert_eq!(rule.instructions, "Is the text written in the active voice?");
        assert_eq!(r.threshold, 0.5);
        assert_eq!(r.model, "jev-latest");
        assert_eq!(r.min_words, 3);
        assert!(r.ignore_prefixes.contains(&"noqa".to_string()));
    }

    #[test]
    fn no_layers_means_no_rules() {
        let r = resolve(&[]);
        assert!(r.rules.is_empty());
        assert_eq!(r.threshold, DEFAULT_THRESHOLD);
    }

    #[test]
    fn disable_then_reenable() {
        let outer = parse("disable = [\"active_voice\"]").unwrap();
        let inner = parse("enable = [\"active_voice\"]").unwrap();
        assert!(resolve(&[defaults(), outer.clone()]).rules.is_empty());
        assert_eq!(resolve(&[defaults(), outer, inner]).rules.len(), 1);
    }

    #[test]
    fn nearest_layer_has_final_say() {
        let outer = parse("threshold = 0.9\nmin_words = 1").unwrap();
        let inner = parse("threshold = 0.2").unwrap();
        let r = resolve(&[defaults(), outer, inner]);
        assert_eq!(r.threshold, 0.2);
        assert_eq!(r.min_words, 1);
        // A nearer layer can redefine a default rule wholesale.
        let override_rule =
            parse("[rules.active_voice]\ninstructions = \"Custom?\"\ncriteria = { true = \"y\", false = \"n\" }")
                .unwrap();
        let r = resolve(&[defaults(), override_rule]);
        assert_eq!(r.rules["active_voice"].instructions, "Custom?");
    }

    #[test]
    fn custom_rule_and_threshold() {
        let cfg = parse(
            r#"
threshold = 0.7
[rules.no_jargon]
instructions = "Is the text free of jargon?"
criteria = { true = "plain words", false = "jargon" }
threshold = 0.2
"#,
        )
        .unwrap();
        let r = resolve(&[defaults(), cfg]);
        assert_eq!(r.rules.len(), 2);
        let rule = &r.rules["no_jargon"];
        assert_eq!(r.threshold_for(rule), 0.2);
        assert_eq!(r.threshold_for(&r.rules["active_voice"]), 0.7);
    }

    #[test]
    fn rejects_unknown_keys_and_bad_thresholds() {
        assert!(parse("bogus = 1").is_err());
        assert!(parse("threshold = 1.5").is_err());
        assert!(parse_yaml("threshold: 1.5").is_err());
        assert!(parse_yaml("bogus: 1").is_err());
    }

    #[test]
    fn skips_short_and_directive_comments() {
        let r = resolve(&[defaults()]);
        assert!(!r.should_evaluate("noqa: E501"));
        assert!(!r.should_evaluate("fixme"));
        assert!(r.should_evaluate("The cache holds parsed trees."));
    }
}
