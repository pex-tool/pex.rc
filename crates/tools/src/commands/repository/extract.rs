// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::ffi::OsStr;
use std::fmt::{Display, Formatter, Write as _};
use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::anyhow;
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use clap::Args;
use flate2::Compression;
use flate2::write::GzEncoder;
use fs_err as fs;
use indexmap::IndexSet;
use interpreter::{Interpreter, InterpreterConstraints, SearchPath, SelectionStrategy};
use log::warn;
use pex::{
    Layout,
    Pex,
    PexPath,
    RawPexInfo,
    collect_loose_user_source,
    collect_zipped_user_source_indexes,
};
use repackage::{WheelOptions, repackage_wheels};
use scripts::IdentifyInterpreter;
use tar::Header;
use zip::{CompressionMethod, ZipArchive};

#[derive(Args)]
pub(crate) struct ExtractArgs {
    /// The path to extract distribution as wheels to.
    #[arg(short = 'f', long, visible_aliases = ["find-links", "repo"])]
    dest_dir: PathBuf,

    /// Also extract a wheel for the PEX file sources.
    #[arg(short = 'D', long, default_value_t = false)]
    sources: bool,

    /// Use the current system time to generate timestamps for the extracted distributions.
    ///
    /// Otherwise, Pex will use midnight on January 1, 1980. By using system time, the extracted
    /// distributions will not be reproducible, meaning that if you were to re-run extraction
    /// against the same PEX file then the newly extracted distributions would not be byte-for-byte
    /// identical distributions extracted in prior runs.
    #[arg(long, default_value_t = false, verbatim_doc_comment)]
    use_system_time: bool,

    /// Serve the `--find-links` repo.
    #[arg(long, default_value_t = false)]
    serve: bool,

    /// The port to serve the --find-links repo on.
    #[arg(long)]
    port: Option<u16>,

    /// The path of a file to write the `<pid>:<port>` of the find links server to.
    #[arg(long)]
    pid_file: Option<PathBuf>,

    /// The maximum time to wait in (fractional) seconds for the server to start up.
    ///
    /// If this timeout is reached, the command will exit with an error instead of
    /// waiting indefinitely. The wait is indefinite by default.
    #[arg(long, default_value_t = 0.0, verbatim_doc_comment)]
    timeout: f32,
}

pub(crate) fn extract(python: Option<&Path>, pex: Pex, args: ExtractArgs) -> anyhow::Result<()> {
    let timestamp = if args.use_system_time {
        None
    } else {
        Some(
            Utc.with_ymd_and_hms(1980, 1, 1, 0, 0, 0)
                .single()
                .expect("This is an unambiguous date."),
        )
    };
    let options = WheelOptions::new(CompressionMethod::Deflated, None, timestamp);
    repackage_wheels(&pex, &options, &args.dest_dir)?;
    let pex_info = pex.info.raw();
    let pex_path = PexPath::from_pex_info(pex_info, true);
    for additional_pex in pex_path.load_pexes()? {
        repackage_wheels(&additional_pex, &options, &args.dest_dir)?;
    }

    if args.sources || args.serve {
        let mut scripts = pex.scripts()?;
        let identify_interpreter = IdentifyInterpreter::read(&mut scripts)?;
        let interpreter = if let Some(python) = python {
            Interpreter::load(python, &identify_interpreter)?
        } else {
            InterpreterConstraints::try_from(&pex_info.interpreter_constraints)?
                .iter_possibly_compatible_python_exes(
                    SelectionStrategy::Newest,
                    SearchPath::from_env()?,
                    true,
                )?
                .filter_map(|python_exe| Interpreter::load(&python_exe, &identify_interpreter).ok())
                .next()
                .ok_or_else(|| {
                    anyhow!(
                        "Failed to find a Python interpreter to serve the find links repo at \
                        {path} with.",
                        path = args.dest_dir.display()
                    )
                })?
        };
        let pex_path = pex.path;
        if args.sources {
            extract_sdist(pex_path, pex.layout, pex_info, &args.dest_dir, timestamp)?;
        }
        if args.serve {
            serve(
                pex_path,
                &interpreter,
                &args.dest_dir,
                args.port,
                args.pid_file.as_deref(),
                args.timeout,
            )?;
        }
    }
    Ok(())
}

fn extract_sdist(
    pex_path: &Path,
    layout: Layout,
    pex_info: &RawPexInfo,
    dest_dir: &Path,
    timestamp: Option<DateTime<Utc>>,
) -> anyhow::Result<()> {
    let project_name = pex_path.file_stem().expect("A PEX always has a file name");
    let version = format!("0.0.0+{code_hash}", code_hash = pex_info.code_hash);
    let pnav = format!(
        "{project_name}-{version}",
        project_name = project_name.display(),
    );
    let sdist_path = dest_dir.join(format!("{pnav}.tar.gz"));

    let (tar_tmp_file, tar_tmp_path) = tempfile::NamedTempFile::new_in(dest_dir)?.into_parts();
    let mut tar = tar::Builder::new(GzEncoder::new(tar_tmp_file, Compression::default()));
    let top_dir = PathBuf::from(pnav);
    let src_dir = top_dir.join("src");
    let sources = match layout {
        Layout::Packed | Layout::Loose => {
            add_loose_source(&mut tar, &src_dir, pex_path, timestamp)?
        }
        Layout::ZipApp => add_zipped_source(
            &mut tar,
            &src_dir,
            ZipArchive::new(File::open(pex_path)?)?,
            timestamp,
        )?,
    };
    add_sdist_files(
        &mut tar,
        &top_dir,
        project_name,
        &version,
        pex_path,
        pex_info,
        sources,
        timestamp,
    )?;
    tar.finish()?;

    tar_tmp_path.persist(sdist_path)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add_sdist_files(
    tar: &mut tar::Builder<GzEncoder<File>>,
    top_dir: &Path,
    project_name: &OsStr,
    version: &str,
    pex_path: &Path,
    pex_info: &RawPexInfo,
    sources: Sources,
    timestamp: Option<DateTime<Utc>>,
) -> anyhow::Result<()> {
    add_file(
        tar,
        top_dir,
        "pyproject.toml",
        timestamp,
        Cow::Borrowed(
            r#"
[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"
"#,
        ),
    )?;

    let interpreter_constraints =
        InterpreterConstraints::try_from(pex_info.interpreter_constraints.as_slice())?
            .into_constraints();
    let mut python_requires = String::new();
    if interpreter_constraints.len() == 1
        && let Some(version_specifiers) = interpreter_constraints[0].version_specifiers()
    {
        write!(&mut python_requires, "{version_specifiers}")?;
    } else if !pex_info.interpreter_constraints.is_empty() {
        struct DisplayIcs<'a>(&'a Vec<&'a str>);
        impl<'a> Display for DisplayIcs<'a> {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                for (idx, ic) in self.0.iter().enumerate() {
                    writeln!(f, "{index}. {ic}", index = idx + 1)?;
                }
                Ok(())
            }
        }

        warn!(
            "Omitting `python_requires` for {project_name} sdist since {pex} has multiple \
            interpreter constraints:\n{interpreter_constraints}",
            project_name = project_name.display(),
            pex = pex_path.display(),
            interpreter_constraints = DisplayIcs(&pex_info.interpreter_constraints)
        )
    }

    let mut pkg_info = String::new();
    writeln!(&mut pkg_info, "Metadata-Version: 2.1")?;
    writeln!(
        &mut pkg_info,
        "Name: {project_name}",
        project_name = project_name.display()
    )?;
    writeln!(&mut pkg_info, "Version: {version}")?;
    if !python_requires.is_empty() {
        writeln!(&mut pkg_info, "Requires-Python: {python_requires}")?;
    }
    for requirement in &pex_info.requirements {
        writeln!(&mut pkg_info, "Requires-Dist: {requirement}")?;
    }
    add_file(tar, top_dir, "PKG-INFO", timestamp, Cow::Owned(pkg_info))?;

    struct IniList<'a>(&'static str, Vec<Cow<'a, str>>);
    impl<'a> Display for IniList<'a> {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            if self.1.is_empty() {
                return Ok(());
            }
            writeln!(f, "{name} =", name = self.0)?;
            for item in self.1.iter() {
                writeln!(f, "    {item}")?;
            }
            Ok(())
        }
    }
    let py_modules = IniList(
        "py_modules",
        sources.modules.into_iter().map(Cow::Owned).collect(),
    );
    let packages = IniList(
        "packages",
        sources.packages.into_iter().map(Cow::Owned).collect(),
    );
    let install_requires = IniList("install_requires", pex_info.requirements.to_vec());

    let mut console_scripts = Vec::with_capacity(1);
    if let Some(entry_point) = pex_info.entry_point
        && entry_point.contains(":")
        && let Some(name) = project_name.to_str()
    {
        console_scripts.push(Cow::Owned(format!("{name} = {entry_point}")))
    }
    let entry_points = IniList("console_scripts", console_scripts);

    add_file(
        tar,
        top_dir,
        "setup.cfg",
        timestamp,
        Cow::Owned(format!(
            r#"
[metadata]
name = {name}
version = {version}

[options]
zip_safe = False
{py_modules}
{packages}
package_dir =
    =src
include_package_data = True

{python_requires}
{install_requires}

[options.entry_points]
{entry_points}
"#,
            name = project_name.display()
        )),
    )?;

    add_file(
        tar,
        top_dir,
        "setup.py",
        timestamp,
        Cow::Borrowed("import setuptools; setuptools.setup()"),
    )?;
    add_file(
        tar,
        top_dir,
        "MANIFEST.in",
        timestamp,
        Cow::Borrowed("recursive-include src *"),
    )?;
    Ok(())
}

fn add_file(
    tar: &mut tar::Builder<GzEncoder<File>>,
    top_dir: &Path,
    path: impl AsRef<Path>,
    timestamp: Option<DateTime<Utc>>,
    content: Cow<'_, str>,
) -> anyhow::Result<()> {
    let size = u64::try_from(content.as_ref().len())?;
    let header = create_header(top_dir.join(path), size, false, timestamp)?;
    tar.append(&header, Cursor::new(content.as_ref()))?;
    Ok(())
}

fn create_header(
    path: impl AsRef<Path>,
    size: u64,
    is_executable: bool,
    timestamp: Option<DateTime<Utc>>,
) -> anyhow::Result<Header> {
    let mut header = tar::Header::new_ustar();
    header.set_path(path.as_ref())?;
    header.set_size(size);
    header.set_mode(if is_executable { 0o755 } else { 0o644 });
    if let Some(timestamp) = timestamp {
        header.set_mtime(u64::try_from(timestamp.timestamp())?)
    }
    header.set_cksum();
    Ok(header)
}

struct Sources {
    modules: IndexSet<String>,
    packages: IndexSet<String>,
}

impl Sources {
    fn new() -> Self {
        Self {
            modules: IndexSet::new(),
            packages: IndexSet::new(),
        }
    }
}

fn add_zipped_source(
    tar: &mut tar::Builder<impl Write>,
    src_dir: &Path,
    mut pex: ZipArchive<File>,
    timestamp: Option<DateTime<Utc>>,
) -> anyhow::Result<Sources> {
    let mut sources = Sources::new();
    for index in collect_zipped_user_source_indexes(&pex) {
        let entry = pex.by_index(index)?;
        if entry.is_file() {
            if !entry.name().contains("/") && entry.name().ends_with(".py") {
                sources.modules.insert(
                    entry
                        .name()
                        .strip_suffix(".py")
                        .expect("We confirmed the file name ended with .py above")
                        .to_string(),
                );
            } else {
                let mut package = String::new();
                let mut last_component_len = 0;
                for component in entry.name().split("/") {
                    last_component_len = component.len();
                    if !package.is_empty() {
                        last_component_len += 1;
                        package.push('.')
                    }
                    package.push_str(component);
                }
                package.truncate(package.len() - last_component_len);
                sources.packages.insert(package);
            }
        }
        let mut header = tar::Header::new_ustar();
        header.set_path(src_dir.join(entry.name()))?;
        header.set_size(entry.size());
        if let Some(unix_mode) = entry.unix_mode() {
            header.set_mode(unix_mode)
        }
        if let Some(timestamp) = timestamp {
            header.set_mtime(u64::try_from(timestamp.timestamp())?);
        } else if let Some(last_modified) = entry.last_modified()
            && let Ok(last_modified) = NaiveDateTime::try_from(last_modified)
            && let Ok(mtime) = u64::try_from(last_modified.and_utc().timestamp())
        {
            header.set_mtime(mtime);
        }
        header.set_cksum();
        tar.append(&header, entry)?;
    }
    Ok(sources)
}

fn add_loose_source(
    tar: &mut tar::Builder<impl Write>,
    src_dir: &Path,
    pex: &Path,
    timestamp: Option<DateTime<Utc>>,
) -> anyhow::Result<Sources> {
    let mut sources = Sources::new();
    for entry in collect_loose_user_source(pex)? {
        let entry_relpath = entry
            .path()
            .strip_prefix(pex)
            .expect("Walker paths of a PEX directory are always sub-paths");
        if entry.file_type().is_file()
            && !entry_relpath.as_os_str().as_encoded_bytes().contains(&b'/')
            && let Some(file_name) = entry_relpath.file_name()
            && file_name.as_encoded_bytes().ends_with(b".py")
        {
            sources.modules.insert(
                entry_relpath
                    .as_os_str()
                    .to_str()
                    .ok_or_else(|| {
                        anyhow!(
                            "Python file name is not UTF-8: {module}",
                            module = entry.path().display()
                        )
                    })?
                    .strip_suffix(".py")
                    .expect("We confirmed the file name ended with .py above")
                    .to_string(),
            );
        } else if entry.file_type().is_dir() {
            let mut package = String::new();
            for component in entry_relpath.components() {
                if let Component::Normal(name) = component {
                    if !package.is_empty() {
                        package.push('.')
                    }
                    package.push_str(name.to_str().ok_or_else(|| {
                        anyhow!(
                            "Python package path is not UTF-8: {module}",
                            module = entry.path().display()
                        )
                    })?);
                }
            }
            sources.packages.insert(package);
            continue;
        }
        let dst = src_dir.join(entry_relpath);
        if timestamp.is_some() {
            let size = entry.metadata()?.len();
            let is_executable = platform::is_executable(entry.path())?;
            let header = create_header(dst, size, is_executable, timestamp)?;
            tar.append(&header, File::open(entry.path())?)?
        } else {
            tar.append_path_with_name(entry.path(), dst)?
        }
    }
    Ok(sources)
}

fn serve(
    pex_path: &Path,
    interpreter: &Interpreter,
    root_dir: &Path,
    port: Option<u16>,
    pid_file: Option<&Path>,
    timeout: f32,
) -> anyhow::Result<()> {
    let module = if interpreter.details.version.major == 3 {
        "http.server"
    } else {
        "SimpleHTTPServer"
    };
    let mut child = Command::new(&interpreter.details.path)
        // N.B.: Running Python in unbuffered mode here is critical to being able to read stdout.
        .arg("-u")
        .args(["-m", module])
        .arg(port.unwrap_or_default().to_string())
        .current_dir(root_dir)
        .stdout(Stdio::piped())
        .spawn()?;
    let pid = child.id();
    let stdout = child.stdout.take().expect("We requested a stdout pipe.");
    let port_matcher = regex::Regex::new(r"^Serving HTTP on \S+ port (?P<port>\d+)\D")?;
    let (send, recv) = oneshot::channel::<anyhow::Result<u16>>();
    thread::spawn(move || {
        let mut stdout_lines = BufReader::new(stdout).lines();
        if let Some(line) = stdout_lines.next() {
            match line {
                Ok(line) => {
                    if let Some(captures) = port_matcher.captures(&line)
                        && let Some(needle) = captures.name("port")
                    {
                        send.send(needle.as_str().parse::<u16>().map_err(|err| {
                            anyhow!("Failed to parse HTTP server startup port: {err}")
                        }))?;
                    } else {
                        send.send(Err(anyhow!("Failed to start find-links HTTP server.")))?;
                    }
                }
                Err(err) => send.send(Err(anyhow!(
                    "Failed to read 1 line of output from {module} startup: {err}"
                )))?,
            }
        } else {
            send.send(Err(anyhow!(
                "Expected to read a least 1 line of output from {module} startup."
            )))?;
        }

        while let Some(Ok(line)) = stdout_lines.next() {
            eprintln!("{line}");
        }
        Ok::<_, anyhow::Error>(())
    });
    let port = if timeout > 0.0 {
        recv.recv_timeout(Duration::from_secs_f32(timeout))?
    } else {
        recv.recv()?
    }?;
    eprintln!(
        "Serving find-links repo of {pex} via {find_links} at http://localhost:{port}",
        pex = pex_path.display(),
        find_links = root_dir.display(),
    );

    if let Some(pid_file) = pid_file {
        fs::write(pid_file, format!("{pid}:{port}"))?;
    }
    if child.wait().is_err() {
        child.kill()?
    }
    Ok(())
}
