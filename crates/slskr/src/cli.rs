//! Command line. Everything else is configured in the web UI, `slskr.toml` or
//! `SLSKR_<SECTION>_<KEY>` variables, so four flags are parsed by hand.

use std::{ffi::OsString, path::PathBuf};

const HELP: &str = "\
Soulseek client daemon with a web UI

Usage: slskr [OPTIONS]

Options:
      --data-dir <DIR>  Data directory: configuration, databases, logs and downloads
                        [default: $XDG_DATA_HOME/slskr or ~/.local/share/slskr]
                        [env: SLSKR_DATA_DIR]
      --config <FILE>   Configuration file [default: <DATA_DIR>/slskr.toml]
                        [env: SLSKR_CONFIG]
      --no-connect      Do not connect to the Soulseek server (overrides soulseek.connect)
      --web-dir <DIR>   Serve the web UI from DIR (e.g. the trunk output dist/) instead of
                        the built-in copy [env: SLSKR_WEB_DIR]
  -h, --help            Print help
  -V, --version         Print version

Most settings are changed in the web UI. Keys set in <DATA_DIR>/slskr.toml or as
SLSKR_<SECTION>_<KEY> environment variables (e.g. SLSKR_WEB_LISTEN=0.0.0.0:5031) override
it; see docs/CONFIGURATION.md.
";

#[derive(Debug, Default)]
pub struct Cli {
    pub data_dir: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub no_connect: bool,
    pub web_dir: Option<PathBuf>,
}

/// Why parsing stopped instead of returning a [`Cli`].
#[derive(Debug, PartialEq, Eq)]
pub enum Exit {
    Help,
    Version,
    Error(String),
}

impl Cli {
    /// Parses the process arguments and `SLSKR_DATA_DIR`, `SLSKR_CONFIG` and `SLSKR_WEB_DIR`
    /// (flags win). Prints help or the version and exits 0, or prints an error and exits 2.
    pub fn parse() -> Self {
        match Self::parse_from(std::env::args_os(), |name| std::env::var_os(name)) {
            Ok(cli) => cli,
            Err(Exit::Help) => {
                print!("{HELP}");
                std::process::exit(0);
            }
            Err(Exit::Version) => {
                println!("slskr {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            Err(Exit::Error(message)) => {
                eprintln!(
                    "error: {message}\n\nUsage: slskr [OPTIONS]\n\nFor more information, try '--help'."
                );
                std::process::exit(2);
            }
        }
    }

    /// Parses `args` (the first is the program name) with `env` for the variables. Empty
    /// variables count as unset.
    pub fn parse_from(
        args: impl IntoIterator<Item = impl Into<OsString>>,
        env: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Self, Exit> {
        let mut cli = Self::default();
        let mut args = args.into_iter().map(Into::into).skip(1);
        while let Some(arg) = args.next() {
            let text = arg
                .to_str()
                .ok_or_else(|| Exit::Error(format!("invalid argument {arg:?}")))?;
            let (name, inline) = match text.split_once('=') {
                Some((name, value)) if name.starts_with("--") => {
                    (name, Some(OsString::from(value)))
                }
                _ => (text, None),
            };
            let slot = match name {
                "-h" | "--help" => return Err(Exit::Help),
                "-V" | "--version" => return Err(Exit::Version),
                "--no-connect" => {
                    if inline.is_some() {
                        return Err(Exit::Error("'--no-connect' takes no value".into()));
                    }
                    if std::mem::replace(&mut cli.no_connect, true) {
                        return Err(repeated(name));
                    }
                    continue;
                }
                "--data-dir" => &mut cli.data_dir,
                "--config" => &mut cli.config,
                "--web-dir" => &mut cli.web_dir,
                _ => return Err(Exit::Error(format!("unexpected argument '{text}'"))),
            };
            let value = inline
                .or_else(|| args.next())
                .ok_or_else(|| Exit::Error(format!("'{name}' needs a value")))?;
            if slot.replace(value.into()).is_some() {
                return Err(repeated(name));
            }
        }
        for (slot, name) in [
            (&mut cli.data_dir, "SLSKR_DATA_DIR"),
            (&mut cli.config, "SLSKR_CONFIG"),
            (&mut cli.web_dir, "SLSKR_WEB_DIR"),
        ] {
            if slot.is_none() {
                *slot = env(name).filter(|value| !value.is_empty()).map(Into::into);
            }
        }
        Ok(cli)
    }
}

fn repeated(name: &str) -> Exit {
    Exit::Error(format!("'{name}' cannot be used multiple times"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, Exit> {
        Cli::parse_from(args.iter().copied(), |_| None)
    }

    #[test]
    fn parse_command_line() {
        let cli = parse(&[
            "slskr",
            "--data-dir",
            "/tmp/slskr",
            "--config=/etc/slskr.toml",
            "--no-connect",
            "--web-dir",
            "dist",
        ])
        .unwrap();
        assert_eq!(cli.data_dir.as_deref(), Some(Path::new("/tmp/slskr")));
        assert_eq!(cli.config.as_deref(), Some(Path::new("/etc/slskr.toml")));
        assert!(cli.no_connect);
        assert_eq!(cli.web_dir.as_deref(), Some(Path::new("dist")));
        for rejected in [
            &["slskr", "--app-dir", "/x"][..],
            &["slskr", "--set", "a=b"],
            &["slskr", "-g"],
            &["slskr", "positional"],
            &["slskr", "--config"],
            &["slskr", "--no-connect=yes"],
            &["slskr", "--web-dir", "a", "--web-dir", "b"],
        ] {
            assert!(
                matches!(parse(rejected), Err(Exit::Error(_))),
                "{rejected:?}"
            );
        }
        assert_eq!(parse(&["slskr", "--help"]).unwrap_err(), Exit::Help);
        assert_eq!(parse(&["slskr", "-V"]).unwrap_err(), Exit::Version);
    }

    #[test]
    fn environment_fills_missing_flags() {
        let env = |name: &str| match name {
            "SLSKR_DATA_DIR" => Some("/env/data".into()),
            "SLSKR_CONFIG" => Some("".into()),
            "SLSKR_WEB_DIR" => Some("/env/web".into()),
            _ => None,
        };
        let cli = Cli::parse_from(["slskr", "--web-dir", "dist"], env).unwrap();
        assert_eq!(cli.data_dir.as_deref(), Some(Path::new("/env/data")));
        assert_eq!(cli.config, None);
        assert_eq!(cli.web_dir.as_deref(), Some(Path::new("dist")));
    }
}
