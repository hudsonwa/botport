//! The port spec (botport.toml; the old name grok2hermes.toml works the same): which Hermes-side templates own which parts of the profile.
//! Relative paths resolve against `base` (default: the spec file's directory).
use crate::core::util::*;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Default, Debug)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub profile: Option<String>,
    pub base: Option<PathBuf>,
    /// Installer-owned skills category under <profile>/skills/
    pub skill_category: Option<String>,
    /// Grok skills to import, by folder name; absent = all, `[]` = none. Excluded skills are invisible to every
    /// command: not imported, not in the sync digest, never pending/conflict/deleted, never sent back by h2g.
    pub skills_include: Option<Vec<String>>,
    /// Text for <category>/DESCRIPTION.md; `{names}` = the category's skill names, sorted, comma-separated
    pub category_description: Option<String>,
    /// Put every installed skill outside the category into config `skills.disabled`
    #[serde(default)]
    pub disable_other_skills: bool,
    pub memory_skill: Option<MemorySkill>,
    /// `{{KEY}}` substitutions in templates
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    pub soul: Option<Template>,
    pub config: Option<Config>,
    pub user: Option<Seed>,
    pub cron: Option<Cron>,
    pub battery: Option<PathBuf>,
    pub hermes_export: Option<HermesExport>,
}

/// Hermes -> Grok direction (`export-hermes`, `sync --direction h2g|both`).
#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct HermesExport {
    /// where `sync` writes the Hermes-side bundle (relative to the spec file's folder)
    pub out: Option<PathBuf>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct MemorySkill {
    pub name: String,
    pub header: Option<PathBuf>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub template: PathBuf,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub template: PathBuf,
    /// Dotted key paths other owners set; never compared or written (Hermes-side values survive)
    #[serde(default)]
    pub preserve: Vec<String>,
    /// Dotted key path -> YAML file whose value at that path replaces the template's (e.g. blocks another profile
    /// owns: `web = "{{HERMES_HOME}}/profiles/other-bot/config.yaml"`). Missing in that file: the template value stays.
    #[serde(default)]
    pub inherit: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Seed {
    pub seed: PathBuf,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Cron {
    pub file: PathBuf,
}

impl Spec {
    /// Built-in vars: `{{SPEC_DIR}}` (the spec's directory) and `{{HERMES_HOME}}`; absolute var values are
    /// normalized lexically (`a/b/..` -> `a`), so `REPO = "{{SPEC_DIR}}/../.."` names the checkout itself.
    pub fn load(p: &Path, hermes_home: &Path) -> Res<Spec> {
        let mut s: Spec =
            toml::from_str(&read_str(p)?).map_err(|e| format!("{}: {e}", p.display()))?;
        let dir = p.parent().map(Path::to_path_buf).unwrap_or_default();
        let dir = if dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dir
        };
        let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
        let builtin = [
            ("SPEC_DIR", dir.to_string_lossy().to_string()),
            ("HERMES_HOME", hermes_home.to_string_lossy().to_string()),
        ];
        for (k, v) in builtin {
            s.vars.entry(k.to_string()).or_insert(v);
        }
        let raw = s.vars.clone();
        for v in s.vars.values_mut() {
            for (k, b) in &raw {
                if k == "SPEC_DIR" || k == "HERMES_HOME" {
                    *v = v.replace(&format!("{{{{{k}}}}}"), b);
                }
            }
            if v.starts_with('/') {
                *v = normalize(v);
            }
        }
        if let Some(c) = &mut s.config {
            let vars = s.vars.clone();
            for f in c.inherit.values_mut() {
                for (k, b) in &vars {
                    *f = f.replace(&format!("{{{{{k}}}}}"), b);
                }
            }
        }
        let base = match &s.base {
            Some(b) => dir.join(expand(&b.to_string_lossy())),
            None => dir.clone(),
        };
        let fix = |x: &mut PathBuf| *x = base.join(expand(&x.to_string_lossy()));
        if let Some(m) = &mut s.memory_skill {
            if let Some(h) = &mut m.header {
                fix(h);
            }
        }
        if let Some(t) = &mut s.soul {
            fix(&mut t.template);
        }
        if let Some(c) = &mut s.config {
            fix(&mut c.template);
        }
        if let Some(u) = &mut s.user {
            fix(&mut u.seed);
        }
        if let Some(c) = &mut s.cron {
            fix(&mut c.file);
        }
        if let Some(b) = &mut s.battery {
            fix(b);
        }
        if let Some(h) = &mut s.hermes_export {
            if let Some(o) = &mut h.out {
                *o = dir.join(expand(&o.to_string_lossy()));
            }
        }
        s.base = Some(base);
        Ok(s)
    }

    pub fn skill_allowed(&self, name: &str) -> bool {
        self.skills_include
            .as_ref()
            .is_none_or(|l| l.iter().any(|n| n == name))
    }

    pub fn category(&self, bot_name: &str) -> String {
        self.skill_category
            .clone()
            .unwrap_or_else(|| format!("grok-{}", slug(bot_name)))
    }

    pub fn subst(&self, text: &str) -> String {
        let mut t = text.to_string();
        for (k, v) in &self.vars {
            t = t.replace(&format!("{{{{{k}}}}}"), v);
        }
        t
    }
}

/// Lexical path normalization: drop `.`, resolve `..` (no filesystem access).
pub fn normalize(p: &str) -> String {
    let mut out: Vec<&str> = vec![];
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    format!("/{}", out.join("/"))
}

pub fn slug(s: &str) -> String {
    let s: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let s = s
        .split('-')
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if s.is_empty() {
        "bot".into()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_and_subst() {
        assert_eq!(slug("Acme Bot!"), "acme-bot");
        let s = Spec {
            vars: [("REPO".to_string(), "/r".to_string())].into(),
            ..Default::default()
        };
        assert_eq!(s.subst("at {{REPO}}/x {{OTHER}}"), "at /r/x {{OTHER}}");
    }

    #[test]
    fn builtin_vars_and_normalize() {
        assert_eq!(normalize("/a/b/../c/./d/.."), "/a/c");
        let d = std::env::temp_dir().join(format!("g2h-spec-{}", std::process::id()));
        std::fs::create_dir_all(d.join("x/y")).unwrap();
        std::fs::write(d.join("x/y/s.toml"), "[vars]\nREPO = \"{{SPEC_DIR}}/../..\"\n[config]\ntemplate = \"c.yaml\"\ninherit = { web = \"{{HERMES_HOME}}/r.yaml\" }\n").unwrap();
        let s = Spec::load(&d.join("x/y/s.toml"), Path::new("/hh")).unwrap();
        assert_eq!(
            s.vars["REPO"],
            std::fs::canonicalize(&d).unwrap().to_string_lossy()
        );
        assert_eq!(s.config.unwrap().inherit["web"], "/hh/r.yaml");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Spec>("nope = 1").is_err());
    }
}
