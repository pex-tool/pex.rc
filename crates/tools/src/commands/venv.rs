// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{anyhow, bail};
use cache::CacheDir;
use clap::builder::PossibleValue;
use clap::{Args, ValueEnum};
use fs_err as fs;
use interpreter::SearchPath;
use log::warn;
use pep508_rs::PackageName;
use pex::{Layout, Pex, PexPath};
use resolver::CollectWheelMetadata;
use shell_quote::Quote;
use venv::virtualenv::FileSystemLinker;
use venv::{Provenance, Virtualenv, venv_pex};

#[derive(Clone)]
struct BinPath(pex::BinPath);

impl BinPath {
    fn into_inner(self) -> pex::BinPath {
        self.0
    }
}

impl ValueEnum for BinPath {
    fn value_variants<'a>() -> &'a [Self] {
        &[
            BinPath(pex::BinPath::False),
            BinPath(pex::BinPath::Append),
            BinPath(pex::BinPath::Prepend),
        ]
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        Some(PossibleValue::new(self.0.as_str()))
    }
}

#[derive(Clone)]
struct InstallScope(venv_pex::InstallScope);

impl InstallScope {
    fn into_inner(self) -> venv_pex::InstallScope {
        self.0
    }
}

impl AsRef<str> for InstallScope {
    fn as_ref(&self) -> &str {
        match self.0 {
            venv_pex::InstallScope::All => "all",
            venv_pex::InstallScope::Deps => "deps",
            venv_pex::InstallScope::Srcs => "srcs",
        }
    }
}

impl TryFrom<&str> for InstallScope {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> anyhow::Result<Self> {
        Ok(Self(match value {
            "all" => venv_pex::InstallScope::All,
            "deps" => venv_pex::InstallScope::Deps,
            "srcs" => venv_pex::InstallScope::Srcs,
            _ => bail!("Not a recognized InstallScope value: {value}"),
        }))
    }
}

impl ValueEnum for InstallScope {
    fn value_variants<'a>() -> &'a [Self] {
        &[
            InstallScope(venv_pex::InstallScope::All),
            InstallScope(venv_pex::InstallScope::Deps),
            InstallScope(venv_pex::InstallScope::Srcs),
        ]
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        Some(PossibleValue::new(self.0.as_str()))
    }
}

#[derive(Clone, Eq, PartialEq, ValueEnum)]
enum RemoveScope {
    Pex,
    All,
}

#[derive(Args)]
pub(crate) struct VenvArgs {
    /// The scope of code contained in the Pex that is installed in the venv.
    #[arg(
        long,
        value_enum,
        default_value_t = InstallScope(venv_pex::InstallScope::All),
        long_help = "\
The scope of code contained in the Pex that is installed in the venv.
By default, all code is installed and this is generally what you want. However, in some situations
it's beneficial to split the venv installation into deps and srcs steps. This is particularly useful
when installing a PEX in a container image. See
https://docs.pex-tool.org/recipes.html#pex-app-in-a-container for more information."
    )]
    scope: InstallScope,

    /// Remove the PEX after creating a venv from it if the 'pex' value is specified; otherwise,
    /// remove the PEX and the PEX_ROOT if the 'all' value is specified.
    #[arg(long, value_enum)]
    remove: Option<RemoveScope>,

    /// Add the venv bin dir to the PATH in the __main__.py script.
    #[arg(short = 'b', long, value_enum)]
    bin_path: Option<BinPath>,

    /// Give the venv access to the system site-packages dir.
    #[arg(long, default_value_t = false)]
    system_site_packages: bool,

    /// Don't rewrite Python script shebangs in the venv to pass `-I` (or `-sE`).
    #[arg(
        long,
        default_value_t = false,
        long_help = "\
Don't rewrite Python script shebangs in the venv to pass `-I` (or `-sE`) to the interpreter.
This can be used to enable running the venv PEX itself or its Python scripts with a custom
`PYTHONPATH`.
"
    )]
    non_hermetic_scripts: bool,

    /// Compile all `.py` files in the venv.
    #[arg(long, default_value_t = false)]
    compile: bool,

    /// If the venv directory already exists, overwrite it.
    #[arg(short = 'f', long, default_value_t = false)]
    force: bool,

    /// Add pip to the venv.
    #[arg(long, default_value_t = false)]
    pip: bool,

    /// A custom prompt for the venv activation scripts to use.
    #[arg(long)]
    prompt: Option<String>,

    /// Don't error if population of the ven-v encounters distributions in the PEX file with colliding files, just emit a warning.
    #[arg(long, default_value_t = false)]
    collisions_ok: bool,

    /// DEPRECATED: Create the venv using copies of system files instead of symlinks (ignored).
    #[arg(long, default_value_t = false)]
    copies: bool,

    /// DEPRECATED: Create the venv using copies of distributions instead of links or symlinks
    /// (ignored).
    #[arg(long, default_value_t = false)]
    site_packages_copies: bool,

    /// The directory to create the venv in.
    #[arg()]
    venv_dir: PathBuf,
}

fn powershell_quote(value: &str) -> String {
    // N.B.: Stolen from https://github.com/python/cpython/blob/88e378cc1cd55429e08268a8da17e54ede104fb5/Lib/venv/__init__.py#L498-L506
    // This should satisfy PowerShell quoting rules [1], unless the quoted string is
    // passed directly to Windows native commands [2].
    // [1]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_quoting_rules
    // [2]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_parsing#passing-arguments-that-contain-quote-characters
    format!("'{value}'", value = value.replace("'", "''"))
}

struct InstallScopeState {
    state_file: PathBuf,
    prior_state: Option<InstallScope>,
}

impl InstallScopeState {
    fn load(venv_dir: &Path) -> anyhow::Result<Self> {
        let state_file = venv_dir.join(".pex-venv-scope");
        let prior_state = match fs::read_to_string(&state_file) {
            Ok(contents) => Some(InstallScope::try_from(contents.trim())?),
            Err(err) => match err.kind() {
                ErrorKind::NotFound => None,
                _ => bail!("Failed to read state file from prior venv: {err}"),
            },
        };
        Ok(Self {
            state_file,
            prior_state,
        })
    }

    fn is_partial_install(&self) -> bool {
        matches!(
            self.prior_state,
            Some(InstallScope(
                venv_pex::InstallScope::Deps | venv_pex::InstallScope::Srcs
            ))
        )
    }

    fn save(&self, mut install_scope: InstallScope) -> anyhow::Result<()> {
        if let Some(prior_state) = self.prior_state.as_ref()
            && ((prior_state.0 == venv_pex::InstallScope::Srcs
                && install_scope.0 == venv_pex::InstallScope::Deps)
                || (prior_state.0 == venv_pex::InstallScope::Deps
                    && install_scope.0 == venv_pex::InstallScope::Srcs))
        {
            install_scope = InstallScope(venv_pex::InstallScope::All)
        }
        Ok(fs::write(&self.state_file, install_scope.as_ref())?)
    }
}

pub(crate) fn create(python: Option<&Path>, pex: Pex, args: VenvArgs) -> anyhow::Result<()> {
    let search_path = SearchPath::from_env()?;
    let pex_path = PexPath::from_pex_info(pex.info.raw(), true);
    let additional_pexes = pex_path.load_pexes()?;

    let resolved_wheel_metadata = if args.pip {
        Some(CollectWheelMetadata::new())
    } else {
        None
    };
    let resolve = pex.resolve(
        python,
        additional_pexes.iter(),
        search_path,
        resolved_wheel_metadata.clone(),
    )?;
    let pip = if let Some(resolved_wheel_metadata) = resolved_wheel_metadata {
        let pip = PackageName::from_str("pip")?;
        if let Some(pip_wheel) = resolved_wheel_metadata
            .into_collected()?
            .iter()
            .filter_map(|metadata| {
                if metadata.project_name == pip {
                    Some(metadata.file_name)
                } else {
                    None
                }
            })
            .next()
        {
            let message = format_args!(
                "You asked for --pip to be installed in the venv at {venv_dir},\n\
                but the PEX at {pex} already contains {pip_wheel}",
                venv_dir = args.venv_dir.display(),
                pex = pex.path.display()
            );
            if args.collisions_ok {
                warn!("{message}\nUsing {pip_wheel} from the PEX.");
                false
            } else {
                bail!(
                    "{message}\nConsider re-running either without --pip or with --collisions-ok."
                )
            }
        } else {
            true
        }
    } else {
        false
    };

    let mut scripts = pex.scripts()?;

    if args.force && args.venv_dir.exists() {
        fs::remove_dir_all(&args.venv_dir)?;
    }
    fs::create_dir_all(&args.venv_dir)?;

    let install_scope_state = InstallScopeState::load(&args.venv_dir)?;
    let venv = if install_scope_state.is_partial_install() && !args.force {
        Virtualenv::load(Cow::Borrowed(&args.venv_dir), &mut scripts)?
    } else {
        let venv = Virtualenv::create(
            resolve.interpreter,
            Cow::Borrowed(&args.venv_dir),
            FileSystemLinker(),
            &mut scripts,
            args.system_site_packages,
            pip,
            args.prompt.as_deref(),
        )?;
        venv.create_additional_pythons()?;
        venv
    };

    let prompt = args
        .prompt
        .as_deref()
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "({venv_name})",
                venv_name = args
                    .venv_dir
                    .file_name()
                    .or_else(|| pex.path.file_name())
                    .unwrap_or_else(|| OsStr::new("venv"))
                    .display()
            )
        });
    let venv_dir = args
        .venv_dir
        .as_os_str()
        .to_os_string()
        .into_string()
        .map_err(|err| anyhow!("Venv path must be valid UTF-8: {err}", err = err.display()))?;
    for activation_script in scripts.activation_scripts()? {
        let file_name: &OsStr = activation_script.file_name.as_ref();
        let (prompt, venv_dir) = match Path::new(file_name).extension() {
            Some(ext) if ext.as_encoded_bytes() == b"bat" => (prompt.clone(), venv_dir.clone()),
            Some(ext) if ext.as_encoded_bytes() == b"fish" => (
                shell_quote::Fish::quote(&prompt),
                shell_quote::Fish::quote(&venv_dir),
            ),
            Some(ext) if ext.as_encoded_bytes() == b"ps1" => {
                (powershell_quote(&prompt), powershell_quote(&venv_dir))
            }
            _ => (
                String::from_utf8(shell_quote::Sh::quote_vec(&prompt))?,
                String::from_utf8(shell_quote::Sh::quote_vec(&venv_dir))?,
            ),
        };
        let contents = activation_script
            .contents
            .replace("__PEXRC_VENV_PROMPT__", &prompt)
            .replace("__PEXRC_VENV_DIR__", &venv_dir);

        fs::write(venv.script_path(file_name), contents)?;
    }

    let shebang_arg = if args.non_hermetic_scripts {
        None
    } else {
        Some(venv.interpreter.hermetic_args())
    };

    let scope = args.scope.into_inner();
    let provenance = Arc::new(Provenance::new(format!(
        "populating venv at {venv_dir} for {pex}",
        pex = pex.path.display()
    )));
    venv_pex::populate(
        &venv,
        venv.interpreter.details.path.as_ref(),
        shebang_arg,
        &pex,
        resolve.wheels,
        &mut scripts,
        args.bin_path.map(BinPath::into_inner),
        scope,
        provenance.clone(),
    )?;
    for (pex, wheels) in resolve.additional_wheels {
        venv_pex::populate_user_code_and_wheels(
            &venv,
            venv.interpreter.details.path.as_ref(),
            shebang_arg,
            pex,
            wheels,
            false,
            scope,
            provenance.clone(),
        )?;
    }
    if let Some(collision_report) = Arc::try_unwrap(provenance)
        .expect("Provenance use is complete")
        .into_collision_report()?
    {
        if args.collisions_ok {
            warn!("{collision_report}");
        } else {
            bail!("{collision_report}");
        }
    }

    if args.compile {
        let exit_status = Command::new(&venv.interpreter.details.path)
            .args(["-m", "compileall"])
            .arg(&args.venv_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?
            .wait()?;
        if !exit_status.success() {
            warn!("Ignoring compile error: {exit_status}")
        }
    }

    if let Some(scope) = args.remove {
        match pex.layout {
            Layout::Loose | Layout::Packed => fs::remove_dir_all(pex.path)?,
            Layout::ZipApp => fs::remove_file(pex.path)?,
        }
        if scope == RemoveScope::All {
            let pex_root = if let Some(root) = pex.info.raw().pex_root.as_deref() {
                Path::new(root)
            } else {
                CacheDir::root()?
            };
            if pex_root.exists() {
                fs::remove_dir_all(pex_root)?;
            }
        }
    }
    install_scope_state.save(InstallScope(scope))?;
    Ok(())
}
