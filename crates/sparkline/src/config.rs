//! `~/.config/sparkline/config.toml`
//!
//! ```toml
//! default_profile = "local"
//! edit_mode = "vi"        # or "emacs" (default)
//! max_rows = 1000         # rows fetched/shown per query; 0 = unlimited
//!
//! [profiles.local]
//! remote = "sc://localhost:15002"
//!
//! [profiles.prod]
//! # ${VAR} is expanded from the environment, so tokens can stay out of the file.
//! remote = "sc://spark.example.com:443/;token=${SPARK_TOKEN}"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

pub const DEFAULT_REMOTE: &str = "sc://localhost:15002";
pub const DEFAULT_MAX_ROWS: usize = 1000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EditMode {
    #[default]
    Emacs,
    Vi,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub default_profile: Option<String>,
    pub edit_mode: EditMode,
    pub max_rows: Option<usize>,
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub remote: String,
}

impl Config {
    /// Loads `path`, or the default location. A missing default file is fine;
    /// a missing explicit file is an error.
    pub fn load(path: Option<&Path>) -> Result<Config> {
        let (path, explicit) = match path {
            Some(p) => (p.to_path_buf(), true),
            None => match config_dir() {
                Some(d) => (d.join("config.toml"), false),
                None => return Ok(Config::default()),
            },
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => {
                return Ok(Config::default())
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Resolves the remote to connect to. Precedence: `--remote`, `--profile`,
    /// `SPARK_REMOTE` (if it's an `sc://` URL), `default_profile`, localhost.
    pub fn resolve_remote(
        &self,
        remote: Option<&str>,
        profile: Option<&str>,
        env_remote: Option<&str>,
    ) -> Result<String> {
        if let Some(r) = remote {
            return Ok(r.to_string());
        }
        if let Some(name) = profile {
            return self.profile_remote(name);
        }
        if let Some(r) = env_remote.filter(|r| r.starts_with("sc://")) {
            return Ok(r.to_string());
        }
        if let Some(name) = &self.default_profile {
            return self.profile_remote(name);
        }
        Ok(DEFAULT_REMOTE.to_string())
    }

    fn profile_remote(&self, name: &str) -> Result<String> {
        let Some(p) = self.profiles.get(name) else {
            let known: Vec<_> = self.profiles.keys().map(String::as_str).collect();
            if known.is_empty() {
                bail!("unknown profile {name:?} (no profiles defined in config.toml)");
            }
            bail!("unknown profile {name:?} (known: {})", known.join(", "));
        };
        expand_env(&p.remote).with_context(|| format!("profile {name:?}"))
    }
}

/// Expands `${VAR}` from the environment.
pub fn expand_env(s: &str) -> Result<String> {
    expand_with(s, |k| std::env::var(k).ok())
}

fn expand_with(s: &str, get: impl Fn(&str) -> Option<String>) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(end) = after.find('}') else {
            bail!("unterminated ${{ in {s:?}")
        };
        let name = &after[..end];
        match get(name) {
            Some(v) => out.push_str(&v),
            None => bail!("environment variable {name} is not set"),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// `$XDG_CONFIG_HOME/sparkline` or `~/.config/sparkline` (also on macOS,
/// where that's what terminal users expect).
pub fn config_dir() -> Option<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

/// `$XDG_DATA_HOME/sparkline` or `~/.local/share/sparkline`.
pub fn data_dir() -> Option<PathBuf> {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

fn xdg_dir(var: &str, fallback: &str) -> Option<PathBuf> {
    let base = std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(fallback)))?;
    Some(base.join("sparkline"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        toml::from_str(
            r#"
            default_profile = "a"
            edit_mode = "vi"
            max_rows = 50
            [profiles.a]
            remote = "sc://a:1"
            [profiles.b]
            remote = "sc://b:2/;token=${TOKEN}"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn parses() {
        let c = cfg();
        assert_eq!(c.edit_mode, EditMode::Vi);
        assert_eq!(c.max_rows, Some(50));
        assert_eq!(c.profiles.len(), 2);
    }

    #[test]
    fn precedence() {
        let c = cfg();
        assert_eq!(
            c.resolve_remote(Some("sc://x"), Some("a"), Some("sc://env"))
                .unwrap(),
            "sc://x"
        );
        assert_eq!(
            c.resolve_remote(None, Some("a"), Some("sc://env")).unwrap(),
            "sc://a:1"
        );
        assert_eq!(
            c.resolve_remote(None, None, Some("sc://env")).unwrap(),
            "sc://env"
        );
        // pyspark-style `local[*]` in SPARK_REMOTE is ignored.
        assert_eq!(
            c.resolve_remote(None, None, Some("local[*]")).unwrap(),
            "sc://a:1"
        );
        assert_eq!(
            Config::default().resolve_remote(None, None, None).unwrap(),
            DEFAULT_REMOTE
        );
        assert!(c.resolve_remote(None, Some("nope"), None).is_err());
    }

    #[test]
    fn env_expansion() {
        let get = |k: &str| (k == "TOKEN").then(|| "s3cret".to_string());
        assert_eq!(
            expand_with("sc://b/;token=${TOKEN}", get).unwrap(),
            "sc://b/;token=s3cret"
        );
        assert!(expand_with("${MISSING}", get).is_err());
        assert!(expand_with("${OPEN", get).is_err());
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("max_rowz = 1").is_err());
    }
}
