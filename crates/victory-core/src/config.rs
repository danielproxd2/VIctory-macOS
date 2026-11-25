//! Where the user's files live.
//!
//! Resolution order for each value: environment variable, then the config
//! file (`VICTORY_CONFIG`, else `./victory.toml`), then the default.
//! See `victory.example.toml` at the repository root.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::policy::ensure_outside_repo;

#[derive(Debug, Default, Clone)]
pub struct Config {
    /// The user's dump of Grand Theft Auto VI.
    pub game_dir: Option<PathBuf>,
    /// Root for all derived data. Always outside the repository.
    pub cache_dir: PathBuf,
    /// What titles write and must keep (persistent local storage). Never
    /// inside the cache or a cache directory of the host, which may be
    /// deleted at any time.
    pub data_dir: PathBuf,
    /// Other titles used to exercise platform-level code.
    pub test_titles: Vec<PathBuf>,
    /// The user's copy of AMD FidelityFX for PC (the folder with its
    /// upscaler DLL), which replaces a title's console build.
    pub fidelityfx_dir: Option<PathBuf>,
    /// The config file that was read, if any.
    pub source: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    paths: FilePaths,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePaths {
    game_dir: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    #[serde(default)]
    test_titles: Vec<PathBuf>,
    fidelityfx_dir: Option<PathBuf>,
}

impl Config {
    /// Loads configuration from the environment and the config file.
    pub fn load() -> Result<Self> {
        // An empty directory keeps `victory.toml` relative, as it is printed.
        Self::load_from(Path::new(""), |k| std::env::var_os(k))
    }

    /// [`Config::load`] with the working directory and the environment
    /// passed in. A relative `VICTORY_CONFIG` is taken from `cwd`.
    fn load_from(cwd: &Path, env: impl Fn(&str) -> Option<OsString>) -> Result<Self> {
        let explicit = explicit_config_path(&env).map(|p| cwd.join(p));
        let path = explicit.clone().unwrap_or_else(|| cwd.join("victory.toml"));
        let file = if path.is_file() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            Some((path, text))
        } else if let Some(p) = explicit {
            anyhow::bail!(
                "VICTORY_CONFIG points at {}, which does not exist",
                p.display()
            );
        } else {
            None
        };
        let (source, parsed) = match file {
            Some((path, text)) => {
                let parsed: FileConfig =
                    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                (Some(path), parsed)
            }
            None => (None, FileConfig::default()),
        };
        Self::resolve(parsed.paths, source, env)
    }

    /// The configuration of an environment with no config file.
    #[cfg(test)]
    pub(crate) fn defaults_of(env: impl Fn(&str) -> Option<OsString>) -> Result<Self> {
        Self::resolve(FilePaths::default(), None, env)
    }

    fn resolve(
        file: FilePaths,
        source: Option<PathBuf>,
        env: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Self> {
        let env_path = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let test_titles = match env("VICTORY_TEST_TITLES") {
            Some(v) if !v.is_empty() => std::env::split_paths(&v)
                .filter(|p| !p.as_os_str().is_empty())
                .collect(),
            _ => file.test_titles,
        };
        let cache_dir = env_path("VICTORY_CACHE_DIR")
            .or(file.cache_dir)
            .map_or_else(|| default_dir(DefaultDir::Cache, &env), Ok)?;
        let data_dir = env_path("VICTORY_DATA_DIR")
            .or(file.data_dir)
            .map_or_else(|| default_dir(DefaultDir::Data, &env), Ok)?;
        Ok(Config {
            game_dir: env_path("VICTORY_GAME_DIR").or(file.game_dir),
            cache_dir,
            data_dir,
            test_titles,
            fidelityfx_dir: env_path("VICTORY_FIDELITYFX_DIR").or(file.fidelityfx_dir),
            source,
        })
    }

    /// The game directory, or a helpful error if it is not configured.
    pub fn require_game_dir(&self) -> Result<&Path> {
        self.game_dir.as_deref().context(
            "no game directory configured: set VICTORY_GAME_DIR or [paths].game_dir in \
             victory.toml (see victory.example.toml)",
        )
    }

    /// Returns `<cache>/<sub>`, creating it, after checking the asset policy.
    pub fn cache_subdir(&self, sub: &str) -> Result<PathBuf> {
        subdir(&self.cache_dir, sub)
    }

    /// Returns `<data>/<sub>`, creating it, after checking the asset policy.
    pub fn data_subdir(&self, sub: &str) -> Result<PathBuf> {
        subdir(&self.data_dir, sub)
    }
}

/// The config file explicitly named by the environment, if any. An empty
/// `VICTORY_CONFIG` means unset, like the other path variables.
fn explicit_config_path(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    env("VICTORY_CONFIG")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn subdir(root: &Path, sub: &str) -> Result<PathBuf> {
    let dir = root.join(sub);
    ensure_outside_repo(&dir)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Which of the two default directories is asked for.
#[derive(Clone, Copy)]
enum DefaultDir {
    Cache,
    Data,
}

/// Where a directory goes when neither the environment nor the file names it.
///
/// With `LOCALAPPDATA` (Windows, and Wine): `%LOCALAPPDATA%/victory/cache`
/// and `%LOCALAPPDATA%/victory/data`. Elsewhere the cache follows the XDG
/// cache directory (`XDG_CACHE_HOME`, else `~/.cache`), and the data, which
/// must outlive the cache, follows the XDG data directory: `XDG_DATA_HOME`,
/// else `~/Library/Application Support` on macOS and `~/.local/share` on
/// other hosts. Empty variables count as unset.
fn default_dir(which: DefaultDir, env: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf> {
    let var = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let (name, xdg, home_sub): (_, _, &[&str]) = match which {
        DefaultDir::Cache => ("cache", "XDG_CACHE_HOME", &[".cache"]),
        DefaultDir::Data if cfg!(target_os = "macos") => {
            ("data", "XDG_DATA_HOME", &["Library", "Application Support"])
        }
        DefaultDir::Data => ("data", "XDG_DATA_HOME", &[".local", "share"]),
    };
    let base = var("LOCALAPPDATA")
        .or_else(|| var(xdg))
        .or_else(|| var("HOME").map(|home| home_sub.iter().fold(home, |p, part| p.join(part))))
        .context("cannot determine a user directory; set VICTORY_CACHE_DIR and VICTORY_DATA_DIR")?;
    Ok(base.join("victory").join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `HOME`, which the defaults need, plus one more variable.
    fn home_and(name: &str, value: OsString) -> impl Fn(&str) -> Option<OsString> {
        let name = name.to_owned();
        move |k| match k {
            "HOME" => Some(OsString::from("home")),
            k if k == name => Some(value.clone()),
            _ => None,
        }
    }

    #[test]
    fn environment_overrides_file() {
        let file = FilePaths {
            game_dir: Some("from-file".into()),
            cache_dir: Some("cache-from-file".into()),
            ..Default::default()
        };
        let env = home_and("VICTORY_GAME_DIR", "from-env".into());
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.game_dir.unwrap(), PathBuf::from("from-env"));
        assert_eq!(cfg.cache_dir, PathBuf::from("cache-from-file"));
    }

    #[test]
    fn empty_env_values_are_ignored() {
        let file = FilePaths {
            game_dir: Some("from-file".into()),
            ..Default::default()
        };
        let env = home_and("VICTORY_GAME_DIR", OsString::new());
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.game_dir.unwrap(), PathBuf::from("from-file"));
    }

    #[test]
    fn empty_test_titles_env_is_ignored() {
        let file = FilePaths {
            test_titles: vec!["from-file".into()],
            ..Default::default()
        };
        let env = home_and("VICTORY_TEST_TITLES", OsString::new());
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.test_titles, vec![PathBuf::from("from-file")]);
    }

    #[test]
    fn test_titles_env_splits_paths() {
        let value = std::env::join_paths(["first", "second"]).unwrap();
        let file = FilePaths::default();
        let env = home_and("VICTORY_TEST_TITLES", value);
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(
            cfg.test_titles,
            vec![PathBuf::from("first"), PathBuf::from("second")]
        );
    }

    #[test]
    fn empty_config_env_is_ignored() {
        let empty = |k: &str| (k == "VICTORY_CONFIG").then(OsString::new);
        assert_eq!(explicit_config_path(empty), None);
        let set = |k: &str| (k == "VICTORY_CONFIG").then(|| OsString::from("custom.toml"));
        assert_eq!(
            explicit_config_path(set),
            Some(PathBuf::from("custom.toml"))
        );
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../victory.example.toml");
        let parsed: FileConfig = toml::from_str(text).unwrap();
        // It is copied to victory.toml as it stands (setup.sh): no game is
        // configured in it, since a game_dir that is set and is not there
        // fails `victory config`, and the example's could be there on no
        // machine. The line is in it, commented out.
        assert_eq!(parsed.paths.game_dir, None);
        assert!(text.contains("\n# game_dir = '"), "{text}");

        // The macOS section is commented out; its `key = value` lines must
        // be a valid [paths] table that names all three folders on Z:.
        let (_, macos) = text
            .split_once("# --- macOS")
            .expect("the example has a macOS section");
        let mut table = String::from("[paths]\n");
        for line in macos.lines().filter_map(|l| l.strip_prefix("# ")) {
            if line.contains(" = '") {
                table.push_str(line);
                table.push('\n');
            }
        }
        let parsed: FileConfig = toml::from_str(&table).unwrap();
        for dir in [
            parsed.paths.game_dir,
            parsed.paths.cache_dir,
            parsed.paths.data_dir,
        ] {
            let dir = dir.expect("the macOS section names game, cache and data");
            let dir = dir.to_str().unwrap();
            assert!(dir.starts_with(r"Z:\Users\"), "{dir}");
        }
    }

    /// An environment made of the given pairs and nothing else.
    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn missing_explicit_config_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env_of(&[("VICTORY_CONFIG", "absent.toml"), ("HOME", "home")]);
        let err = Config::load_from(tmp.path(), env).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("VICTORY_CONFIG points at"), "{text}");
        assert!(text.contains("absent.toml"), "{text}");
        assert!(text.contains("does not exist"), "{text}");
    }

    #[test]
    fn missing_default_config_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::load_from(tmp.path(), env_of(&[("HOME", "home")])).unwrap();
        assert_eq!(cfg.source, None);
        assert_eq!(cfg.game_dir, None);
    }

    /// An empty `VICTORY_CONFIG` names no file: the working directory's
    /// `victory.toml` is read if there is one, and none is not an error.
    #[test]
    fn an_empty_explicit_config_is_the_default_one() {
        let tmp = tempfile::tempdir().unwrap();
        let env = || env_of(&[("VICTORY_CONFIG", ""), ("HOME", "home")]);
        let cfg = Config::load_from(tmp.path(), env()).unwrap();
        assert_eq!(cfg.source, None);
        let path = tmp.path().join("victory.toml");
        std::fs::write(&path, "[paths]\ngame_dir = 'game'\ncache_dir = 'c'\n").unwrap();
        let cfg = Config::load_from(tmp.path(), env()).unwrap();
        assert_eq!(cfg.source, Some(path));
        assert_eq!(cfg.game_dir, Some(PathBuf::from("game")));
    }

    #[test]
    fn config_file_is_read_from_the_working_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("victory.toml");
        std::fs::write(&path, "[paths]\ngame_dir = 'game'\ncache_dir = 'c'\n").unwrap();
        let cfg = Config::load_from(tmp.path(), env_of(&[("HOME", "home")])).unwrap();
        assert_eq!(cfg.source, Some(path));
        assert_eq!(cfg.game_dir, Some(PathBuf::from("game")));
        assert_eq!(cfg.cache_dir, PathBuf::from("c"));
    }

    #[test]
    fn unknown_key_is_rejected_with_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("other.toml");
        std::fs::write(&path, "[paths]\ngame_dri = 'typo'\n").unwrap();
        let env = env_of(&[("VICTORY_CONFIG", "other.toml"), ("HOME", "home")]);
        let err = Config::load_from(tmp.path(), env).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains(&format!("parsing {}", path.display())),
            "{text}"
        );
        assert!(text.contains("unknown field `game_dri`"), "{text}");
    }

    #[test]
    fn malformed_toml_names_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("victory.toml");
        std::fs::write(&path, "[paths\ngame_dir = ").unwrap();
        let err = Config::load_from(tmp.path(), env_of(&[("HOME", "home")])).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains(&format!("parsing {}", path.display())),
            "{text}"
        );
    }

    #[test]
    fn default_dirs_follow_localappdata_then_xdg_then_home() {
        let dirs = |pairs: &[(&str, &str)]| {
            let cfg = Config::resolve(FilePaths::default(), None, env_of(pairs)).unwrap();
            (cfg.cache_dir, cfg.data_dir)
        };
        let all = [
            ("LOCALAPPDATA", "local"),
            ("XDG_CACHE_HOME", "xc"),
            ("XDG_DATA_HOME", "xd"),
            ("HOME", "home"),
        ];
        let local = Path::new("local").join("victory");
        assert_eq!(dirs(&all), (local.join("cache"), local.join("data")));
        assert_eq!(
            dirs(&all[1..]),
            (
                Path::new("xc").join("victory").join("cache"),
                Path::new("xd").join("victory").join("data")
            )
        );
        let home = Path::new("home");
        let data_root = if cfg!(target_os = "macos") {
            home.join("Library").join("Application Support")
        } else {
            home.join(".local").join("share")
        };
        assert_eq!(
            dirs(&all[3..]),
            (
                home.join(".cache").join("victory").join("cache"),
                data_root.join("victory").join("data")
            )
        );
        // An empty variable counts as unset.
        assert_eq!(
            dirs(&[
                ("LOCALAPPDATA", ""),
                ("XDG_DATA_HOME", ""),
                ("HOME", "home")
            ])
            .1,
            data_root.join("victory").join("data")
        );
    }

    #[test]
    fn no_home_is_an_error() {
        for env in [env_of(&[]), env_of(&[("HOME", "")])] {
            let err = Config::resolve(FilePaths::default(), None, env).unwrap_err();
            assert!(
                format!("{err:#}").contains("cannot determine a user directory"),
                "{err:#}"
            );
        }
        // Naming both directories needs no user directory.
        let env = env_of(&[("VICTORY_CACHE_DIR", "c"), ("VICTORY_DATA_DIR", "d")]);
        Config::resolve(FilePaths::default(), None, env).unwrap();
    }

    #[test]
    fn default_data_dir_is_not_under_the_cache_root() {
        // Off Windows the cache root is `XDG_CACHE_HOME`, else `~/.cache`.
        for (pairs, cache_root) in [
            (&[("HOME", "home")][..], Path::new("home").join(".cache")),
            (
                &[("XDG_CACHE_HOME", "xc"), ("HOME", "home")][..],
                PathBuf::from("xc"),
            ),
        ] {
            let cfg = Config::resolve(FilePaths::default(), None, env_of(pairs)).unwrap();
            assert!(cfg.cache_dir.starts_with(&cache_root), "{cfg:?}");
            assert!(!cfg.data_dir.starts_with(&cache_root), "{cfg:?}");
        }
        // With LOCALAPPDATA the two are siblings.
        let env = env_of(&[("LOCALAPPDATA", "local")]);
        let cfg = Config::resolve(FilePaths::default(), None, env).unwrap();
        assert!(!cfg.data_dir.starts_with(&cfg.cache_dir), "{cfg:?}");
    }

    #[test]
    fn cache_inside_repo_is_refused() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("cache");
        let cfg = Config {
            cache_dir: inside,
            ..Default::default()
        };
        assert!(cfg.cache_subdir("x").is_err());
    }

    #[test]
    fn data_inside_repo_is_refused() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
        let cfg = Config {
            data_dir: inside,
            ..Default::default()
        };
        assert!(cfg.data_subdir("x").is_err());
    }
}
