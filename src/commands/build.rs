// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fmt::{Debug, Display, Formatter, Write as _};
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, Write};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::{Arc, LazyLock};
use std::{env, io, mem, process};

use anyhow::{Context, anyhow, bail};
use boot::{inject_boot, sh_boot_buffer, write_boot, write_sh_boot_shebang};
use build_system::{ProjectDir, VenvBuilder, build_wheel};
use cache::{CacheDir, DigestingWriter, Fingerprint, HashOptions, Key, atomic_file};
use clap::Args;
use const_format::concatcp;
use digest::Digest;
use enumset::enum_set;
use fs_err as fs;
use fs_err::File;
use indexmap::{IndexMap, IndexSet, indexmap};
use interpreter::{
    Interpreter,
    InterpreterConstraint,
    InterpreterConstraints,
    SearchPath,
    SelectionStrategy,
    VersionSpecificity,
};
use ouroboros::self_referencing;
use pep440_rs::VersionSpecifiers;
use pep508_rs::{MarkerTree, Requirement, VerbatimUrl, VersionOrUrl};
use pex::{
    BinPath,
    DEPS_DIR,
    DEPS_ZIP_DIR,
    InheritPath,
    InterpreterSelectionStrategy,
    PEX_INFO_FILE,
    RawPexInfo,
    SRCS_DIR,
    SRCS_ZIP_DIR,
};
use platform::mark_executable;
use python_platform::{
    PYTHON_PLATFORM_LONG_HELP,
    PlatformDetails,
    PythonImplementation,
    PythonPlatform as _,
};
use python_proxy::ProxySource;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use regex::Regex;
use repackage::{WheelOptions, recompress_zipped_whl_to_file};
use resolver::dependency_configuration::DependencyConfiguration;
use resolver::resolve_wheels;
use scripts::{IdentifyInterpreter, Scripts};
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use target::SimplifiedTarget;
use tempfile::NamedTempFile;
use tracing::{debug_span, instrument, warn};
use url::Url;
use venv::install::{populate_whl_zip, write_pex_extra_sys_path_support_files};
use venv::{
    InstallPaths,
    InstalledWheel,
    Provenance,
    PythonProxyLinker,
    Virtualenv,
    collect_installed_wheels,
};
use wheel::{EntryPoints, MetadataDirs, MetadataReader, WheelFile, WheelMetadata};
use zip::result::ZipError;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};
use zip_ext::ZipArchiveExt;

use crate::VERSION;
use crate::compression_method::CompressionArgs;
use crate::embeds::{
    AVAILABLE_TARGETS,
    Binary,
    CLIB_BY_TARGET,
    PROXY_BY_TARGET,
    PROXYW_BY_TARGET,
    read_proxy_content,
};
use crate::interpreter_selection::{InterpreterSelection, InterpreterSelectionArgs};
use crate::target::{PythonPlatform, RequiredTargets};

#[self_referencing]
struct Virtualenvs<'a> {
    venvs: Vec<Virtualenv<'a>>,
    #[borrows(venvs)]
    #[covariant]
    platforms: Vec<Platform<'this>>,
}

enum Repository<'a> {
    Venvs(Virtualenvs<'a>),
    Wheels(Vec<PathBuf>),
}

impl<'a> Repository<'a> {
    fn venvs(venvs: Vec<PathBuf>) -> anyhow::Result<Self> {
        let venvs = venvs
            .into_par_iter()
            .map(|path| Virtualenv::load(Cow::Owned(path), &mut Scripts::Embedded))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self::Venvs(Virtualenvs::new(venvs, |venvs| {
            venvs
                .iter()
                .map(|venv| Platform::Interpreter(Cow::Borrowed(&venv.interpreter)))
                .collect::<Vec<_>>()
        })))
    }

    fn wheels(wheels: Vec<PathBuf>) -> anyhow::Result<Self> {
        let mut wheel_files = Vec::with_capacity(wheels.len());
        for wheel in wheels {
            if wheel.is_dir() {
                for entry in wheel.read_dir()? {
                    let entry = entry?;
                    if entry.file_type()?.is_file()
                        && entry.file_name().as_encoded_bytes().ends_with(b".whl")
                    {
                        wheel_files.push(entry.path())
                    }
                }
            } else {
                wheel_files.push(wheel)
            }
        }
        Ok(Self::Wheels(wheel_files))
    }
}

#[derive(Copy, Clone)]
enum ParseState {
    FindTopLevelComments,
    InMultilineString(&'static str),
}

impl ParseState {
    fn advance(self, line: &str) -> ParseState {
        let mut parse_state = self;
        let mut index = 0;
        loop {
            let remaining_content = &line[index..];
            if remaining_content.is_empty() {
                return parse_state;
            }
            match parse_state {
                ParseState::FindTopLevelComments => {
                    if let Some(start_token) = {
                        let mut start_token = None;
                        for token in [r#"""""#, "'''"] {
                            if let Some(start) = remaining_content.find(token) {
                                index += start + token.len();
                                start_token = Some(token);
                                break;
                            }
                        }
                        start_token
                    } {
                        parse_state = ParseState::InMultilineString(start_token)
                    } else {
                        return parse_state;
                    }
                }
                ParseState::InMultilineString(end_token) => {
                    if let Some(end) = remaining_content.find(end_token) {
                        index += end + end_token.len();
                        parse_state = ParseState::FindTopLevelComments
                    } else {
                        return parse_state;
                    }
                }
            }
        }
    }
}

struct Comment<'a> {
    content: &'a str,
    start_line: usize,
}

struct Indexes {
    start_idx: usize,
    start_line: usize,
    end_idx: usize,
    end_line: usize,
}

impl Indexes {
    fn start(start_idx: usize, start_line: usize, line: &str) -> Self {
        Self {
            start_idx,
            start_line,
            end_idx: start_idx + line.len(),
            end_line: start_line,
        }
    }

    fn to_comment<'a>(&self, text: &'a str) -> Comment<'a> {
        Comment {
            content: &text[self.start_idx..self.end_idx],
            start_line: self.start_line,
        }
    }
}

#[instrument(level = "debug", skip(code))]
fn parse_top_level_comments(code: &str) -> Vec<Comment<'_>> {
    // N.B.: This is all to avoid ever falling into the PEP-723 recommended regex known hole:
    //  https://packaging.python.org/en/latest/specifications/inline-script-metadata/#specification

    let mut top_level_comments = vec![];
    let mut comment_indexes: Option<Indexes> = None;

    let mut parse_state = ParseState::FindTopLevelComments;
    let mut current_idx = 0;
    for (idx, content) in code.split_inclusive('\n').enumerate() {
        let line = idx + 1;
        match (parse_state, comment_indexes.take()) {
            (ParseState::FindTopLevelComments, Some(mut indexes)) => {
                if content.starts_with('#') {
                    if line == indexes.end_line + 1 {
                        indexes.end_idx += content.len();
                        indexes.end_line += 1;
                        comment_indexes = Some(indexes)
                    } else {
                        comment_indexes = Some(Indexes::start(current_idx, line, content))
                    }
                } else {
                    top_level_comments.push(indexes.to_comment(code));
                    parse_state = parse_state.advance(content);
                }
            }
            (ParseState::FindTopLevelComments, None) => {
                if content.starts_with('#') {
                    comment_indexes = Some(Indexes::start(current_idx, line, content))
                } else {
                    parse_state = parse_state.advance(content);
                }
            }
            (ParseState::InMultilineString(_), Some(indexes)) => {
                top_level_comments.push(indexes.to_comment(code));
                parse_state = parse_state.advance(content);
            }
            _ => parse_state = parse_state.advance(content),
        }
        current_idx += content.len();
    }
    if let Some(indexes) = comment_indexes {
        top_level_comments.push(indexes.to_comment(code))
    }

    top_level_comments
}

struct ScriptBlock {
    content: String,
    start_line: usize,
    end_line: usize,
}

impl ScriptBlock {
    fn start(start_line: usize) -> Self {
        // N.B.: This trick gets downstream parsing of `content` by toml to report correct line
        // numbers on error. The column number will still be off by 2, but that is probably
        // easier to work out from context once on the line.
        let content = "\n".repeat(start_line);
        Self {
            content,
            start_line,
            end_line: start_line,
        }
    }

    fn lines(&self) -> impl Iterator<Item = (usize, &str)> {
        self.content
            .split_inclusive('\n')
            .enumerate()
            .map(|(idx, line)| (idx + 1, line))
            // N.B.: This strips out the content prefix we added in `Self::start`.
            .skip(self.start_line)
    }
}

static SCRIPT_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^# /// (?P<type>[a-zA-Z0-9-]+)$\s(?P<content>(^#(| .*)$\s)+)^# ///$")
        .expect("This is a known-good re.")
});

#[instrument(level = "debug", skip(code))]
fn parse_script_block(path: &Path, code: &str) -> anyhow::Result<Option<ScriptBlock>> {
    let mut script_blocks = vec![];
    for comment in parse_top_level_comments(code) {
        for capture in SCRIPT_BLOCK_RE.captures_iter(comment.content) {
            if let Some(script_type) = capture.name("type")
                && script_type.as_str() != "script"
            {
                continue;
            }
            let content_match = capture.name("content").expect("A capture was required.");
            let start_idx = content_match.start();
            let end_idx = content_match.end();
            let raw_content = &comment.content[start_idx..end_idx];
            let start_line = comment.start_line
                + comment.content[..start_idx]
                    .chars()
                    .filter(|c| *c == '\n')
                    .count()
                - 1;
            let mut script_block = ScriptBlock::start(start_line);
            for line in raw_content.split_inclusive('\n') {
                script_block.end_line += 1;
                if let Some(line) = line.strip_prefix("# ") {
                    script_block.content.push_str(line)
                } else {
                    script_block.content.push_str(&line[1..])
                }
            }
            script_block.end_line += 1; // For `# ///`
            script_blocks.push(script_block);
        }
    }

    if script_blocks.len() > 1 {
        let mut message = format!(
            "Found multiple PEP-723 script blocks in {path} but only one is allowed:\n",
            path = path.display()
        );
        for (index, comment) in script_blocks.into_iter().enumerate() {
            if index > 0 {
                writeln!(&mut message)?;
            }
            writeln!(&mut message, "Block {index}:", index = index + 1)?;
            writeln!(
                &mut message,
                "{line:>4} | # /// script",
                line = comment.start_line
            )?;
            for (line, text) in comment.lines() {
                if text.is_empty() {
                    write!(&mut message, "{line:>4} | #")?;
                } else {
                    write!(&mut message, "{line:>4} | # {text}")?;
                }
            }
            writeln!(&mut message, "{line:>4} | # ///", line = comment.end_line)?;
        }
        bail!(message)
    } else {
        Ok(script_blocks.into_iter().next())
    }
}

#[derive(Clone, Debug, Deserialize)]
struct ScriptMetadata {
    #[serde(default, rename = "requires-python")]
    requires_python: VersionSpecifiers,
    #[serde(default)]
    dependencies: Vec<Requirement<Url>>,
}

impl ScriptMetadata {
    fn is_empty(&self) -> bool {
        self.requires_python.is_empty() && self.dependencies.is_empty()
    }
}

struct PythonScript(String);

impl Deref for PythonScript {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.0.as_str()
    }
}

impl PythonScript {
    const EXE_PY: &str = "__pex_executable__.py";

    fn entry_point() -> &'static str {
        Self::EXE_PY
            .strip_suffix(".py")
            .expect("The constant ends in `.py`.")
    }

    fn write(self, dest_dir: &Path, code_hash: &mut Key) -> anyhow::Result<()> {
        code_hash.file_contents(Self::EXE_PY, self.0.as_bytes())?;
        fs::write(dest_dir.join(Self::EXE_PY), &self.0)?;
        Ok(())
    }

    fn inject(
        self,
        zip: &mut ZipWriter<impl Write + Seek>,
        file_options: SimpleFileOptions,
        code_hash: &mut Key,
    ) -> anyhow::Result<()> {
        code_hash.file_contents(Self::EXE_PY, self.0.as_bytes())?;
        zip.start_file(Self::EXE_PY, file_options)?;
        zip.write_all(self.0.as_bytes())?;
        Ok(())
    }
}

struct Exe {
    path: PathBuf,
    content: PythonScript,
    metadata: Option<ScriptMetadata>,
}

impl TryFrom<PathBuf> for Exe {
    type Error = anyhow::Error;

    fn try_from(path: PathBuf) -> anyhow::Result<Self> {
        let content = PythonScript(fs::read_to_string(&path)?);
        let metadata = if let Some(script_metadata) = parse_script_block(&path, &content)? {
            match toml::from_str::<ScriptMetadata>(&script_metadata.content) {
                Ok(metadata) => {
                    if metadata.is_empty() {
                        None
                    } else {
                        Some(metadata)
                    }
                }
                Err(err) => bail!(
                    "Failed to parse script metadata block found in {script} lines \
                    {start_line}-{end_line}:\n\
                    {err}",
                    script = path.display(),
                    start_line = script_metadata.start_line,
                    end_line = script_metadata.end_line,
                ),
            }
        } else {
            None
        };
        Ok(Self {
            path,
            content,
            metadata,
        })
    }
}

enum PexEntryPoint {
    EntryPoint(String),
    Exe(PythonScript),
    Script(String),
}

impl PexEntryPoint {
    #[instrument(level = "debug", skip_all)]
    fn resolve(
        self,
        pex_info: &mut RawPexInfo,
        wheels: &[FingerprintedWheel],
    ) -> anyhow::Result<Option<PythonScript>> {
        match self {
            PexEntryPoint::EntryPoint(ep) => pex_info.entry_point = Some(Cow::Owned(ep)),
            PexEntryPoint::Exe(python_script) => {
                pex_info.entry_point = Some(Cow::Borrowed(PythonScript::entry_point()));
                return Ok(Some(python_script));
            }
            PexEntryPoint::Script(script) => {
                let matches = wheels
                    .into_par_iter()
                    .map(|wheel| {
                        let wheel_file = wheel
                            .path
                            .file_name()
                            .and_then(OsStr::to_str)
                            .ok_or_else(|| {
                                anyhow!(
                                    "The wheel at {path} does not have a valid file name.",
                                    path = wheel.path.display()
                                )
                            })
                            .and_then(WheelFile::parse_file_name)?;
                        let mut whl = ZipArchive::new(File::open(&wheel.path)?)?;
                        let metadata_dirs = MetadataDirs::locate_in_zip(
                            &whl,
                            wheel.path.display(),
                            None,
                            &wheel_file.project_name,
                            &wheel_file.version,
                        )?;
                        match whl.by_name_ex(&format!(
                            "{dist_info_dir}/entry_points.txt",
                            dist_info_dir = metadata_dirs.dist_info_dir()
                        )) {
                            Ok(file) => {
                                let entry_points = EntryPoints::load(file)?;
                                if let Some(entry_point) = entry_points.script(&script) {
                                    Ok(Some((wheel, entry_point.to_string())))
                                } else {
                                    Ok(None)
                                }
                            }
                            Err(err)
                                if let Some(source) = err.source()
                                    && let Some(zip_error) = source.downcast_ref::<ZipError>()
                                    && matches!(zip_error, ZipError::FileNotFound) =>
                            {
                                Ok(None)
                            }
                            Err(err) => Err(anyhow!("{err}")),
                        }
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>();
                if matches.is_empty() {
                    pex_info.script = Some(Cow::Owned(script))
                } else {
                    let mut entry_points = IndexMap::with_capacity(matches.len());
                    for (wheel, entry_point) in matches {
                        entry_points
                            .entry(entry_point)
                            .or_insert_with(IndexSet::new)
                            .insert(&wheel.path);
                    }
                    if entry_points.len() > 1 {
                        let mut msg = format!(
                            "Found {count} conflicting entry point definitions for script {script}:\n",
                            count = entry_points.len()
                        );
                        for (index, (entry_point, wheel_paths)) in entry_points.iter().enumerate() {
                            writeln!(&mut msg, "{index}. {entry_point}:")?;
                            for wheel_path in wheel_paths {
                                write!(
                                    &mut msg,
                                    "   {wheel}",
                                    wheel = wheel_path
                                        .file_name()
                                        .expect("We already parsed a wheel file name to get here.")
                                        .display()
                                )?;
                            }
                        }
                        bail!(msg)
                    }
                    let (entry_point, _) = entry_points
                        .into_iter()
                        .next()
                        .expect("We ensured there was element with the checks above.");
                    pex_info.entry_point = Some(Cow::Owned(entry_point))
                }
            }
        }
        Ok(None)
    }
}
enum Shebang {
    Custom(String),
    EnvCompatible,
    ShBoot,
}

#[derive(Clone, Debug)]
struct KeyValue((String, String));

impl KeyValue {
    fn into_cow_tuple<'a>(self) -> (Cow<'a, str>, Cow<'a, str>) {
        let (key, value) = self.0;
        (Cow::Owned(key), Cow::Owned(value))
    }
}

impl FromStr for KeyValue {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        if let Some((key, value)) = s.split_once('=') {
            Ok(Self((key.to_owned(), value.to_owned())))
        } else {
            bail!("must be of the form `<key>=<value>`")
        }
    }
}

impl Deref for KeyValue {
    type Target = (String, String);

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

const PYTHON_PLATFORM_HELP: &str = "The Python platforms the built PEX will target at runtime.";

const COMPLETE_PYTHON_PLATFORM_LONG_HELP: &str = concatcp!(
    PYTHON_PLATFORM_HELP,
    r#"

If specified, the targets will be used to resolve any specified requirements from the
configured wheels. If required wheels are not present, the build will error.
"#,
    PYTHON_PLATFORM_LONG_HELP
);

const PEX_PATH_HELP: &str = concatcp!(
    "A '",
    platform::PATH_SEP,
    "' separated list of other PEX files to merge into the runtime environment."
);

const PEX_PATH_LONG_HELP: &str = concatcp!(
    PEX_PATH_HELP,
    r#"

N.B.: The paths specified must be valid paths at runtime. PEXes will not be merged at build time.
"#,
);

static CWD: LazyLock<anyhow::Result<PathBuf>> = LazyLock::new(|| Ok(env::current_dir()?));

fn getcwd() -> anyhow::Result<&'static Path> {
    Ok(CWD.as_ref().map_err(|err| anyhow!("{err}"))?)
}

#[derive(Clone, Debug)]
struct Source {
    prefix: PathBuf,
    suffix: PathBuf,
}

impl FromStr for Source {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let (prefix, suffix) = if let Some((source, subdirectory)) = s.rsplit_once('@') {
            let mut subdirectory = Cow::Borrowed(Path::new(subdirectory));
            if subdirectory.is_relative() {
                subdirectory = Cow::Owned(getcwd()?.join(subdirectory));
            }
            let prefix = subdirectory.normalize_lexically()?;
            (prefix, source)
        } else {
            let prefix = getcwd()?.to_path_buf();
            (prefix, s)
        };
        Ok(Source {
            prefix,
            suffix: suffix.split('.').collect(),
        })
    }
}

#[derive(Args, Debug)]
#[command(next_help_heading = "Contents")]
#[group(skip)]
struct Sources {
    /// Source code to include in the PEX.
    ///
    /// All files in the directory tree will be added to the root of the PEX `sys.path`; i.e.: they
    /// will be installed in the site packages directory of the PEX venv at runtime.
    #[arg(short = 'D', long, value_name = "DIR", verbatim_doc_comment)]
    sources_directory: Vec<PathBuf>,

    /// Add a package and all its sub-packages to the PEX.
    ///
    /// The package is expected to be found relative to the current directory. If the package is
    /// housed in a subdirectory, indicate that by appending `@<subdirectory>`. For example, to add
    /// the top-level package `foo` housed in the current directory, use `-P foo`. If the top-level
    /// `foo` package is in the `src` subdirectory use `-P foo@src`. If you wish to just use the
    /// `foo.bar` package in the `src` subdirectory, use `-P foo.bar@src`.
    #[arg(
        short = 'P',
        long = "package",
        value_name = "PACKAGE_SPEC",
        verbatim_doc_comment
    )]
    packages: Vec<Source>,

    /// Add an individual module to the PEX.
    ///
    /// The module is expected to be found relative to the current directory. If the module is
    /// housed in a subdirectory, indicate that by appending `@<subdirectory>`. For example, to add
    /// the top-level module `foo` housed in the current directory, use `-M foo`. If the top-level
    /// `foo` module is in the `src` subdirectory use `-M foo@src`. If you wish to just use the
    /// `foo.bar` module in the `src` subdirectory, use `-M foo.bar@src`.
    #[arg(
        short = 'M',
        long = "module",
        value_name = "MODULE_SPEC",
        verbatim_doc_comment
    )]
    modules: Vec<Source>,
}

impl Sources {
    #[instrument(level = "debug", skip_all)]
    fn copy(&self, dest_dir: &Path, code_hash: &mut Key) -> anyhow::Result<()> {
        let hash_options = HashOptions::new().path(true).contents(true);
        for (src, prefix) in self.sources()? {
            let dst = dest_dir.join(SRCS_DIR).join(src.strip_prefix(prefix)?);
            if src.is_dir() {
                fs::create_dir_all(dst)?;
            } else {
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                platform::reflink_or_copy(src, &dst)?;
                code_hash.file(dst, &hash_options, Some(dest_dir))?;
            }
        }
        Ok(())
    }

    #[instrument(level = "debug", skip_all)]
    fn inject(
        self,
        zip: &mut ZipWriter<impl Write + Seek>,
        file_options: SimpleFileOptions,
        code_hash: &mut Key,
    ) -> anyhow::Result<()> {
        let sources = self.sources()?;
        if sources.is_empty() {
            return Ok(());
        }
        zip.add_directory(SRCS_ZIP_DIR, SimpleFileOptions::DEFAULT)?;
        for (src, prefix) in sources {
            let dst = Path::new(SRCS_DIR).join(src.strip_prefix(prefix)?);
            if src.is_dir() {
                zip.add_directory_from_path(dst, SimpleFileOptions::DEFAULT)?;
            } else {
                zip.start_file_from_path(&dst, file_options)?;
                code_hash.file_stream(dst, &mut File::open(src)?, zip)?;
            }
        }
        Ok(())
    }

    #[instrument(level = "debug", skip_all)]
    fn sources(&self) -> anyhow::Result<Vec<(PathBuf, &Path)>> {
        let directories = self
            .sources_directory
            .iter()
            .map(|src| (src, None))
            .chain(
                self.packages
                    .iter()
                    .map(|source| (&source.prefix, Some(&source.suffix))),
            )
            .collect::<Vec<_>>();

        let mut sources = vec![];
        for (prefix, suffix) in directories {
            let src_dir = if let Some(suffix) = suffix {
                Cow::Owned(prefix.join(suffix))
            } else {
                Cow::Borrowed(prefix)
            };
            for entry in walkdir::WalkDir::new(src_dir.as_ref()) {
                let entry = entry?;
                if let Some(file_name) = entry.file_name().to_str() {
                    if file_name == "__pycache__" {
                        if entry.metadata()?.is_dir() {
                            continue;
                        }
                    } else if [".pyc", ".pyd", ".pyo"]
                        .into_iter()
                        .any(|ext| file_name.ends_with(ext))
                        && entry.metadata()?.is_file()
                    {
                        continue;
                    }
                }
                sources.push((entry.into_path(), prefix.as_path()));
            }
        }
        for module in self.modules.iter() {
            sources.push((
                module.prefix.join(&module.suffix).with_extension("py"),
                module.prefix.as_path(),
            ))
        }
        sources.sort();
        Ok(sources)
    }
}

fn parse_url(value: &str) -> anyhow::Result<Requirement<Url>> {
    let working_dir = getcwd()?;
    let mut requirement: Requirement<VerbatimUrl> = Requirement::parse(value, working_dir)?;
    Ok(match requirement.version_or_url.take() {
        Some(VersionOrUrl::Url(verbatim_url)) => Requirement {
            name: requirement.name,
            extras: requirement.extras,
            version_or_url: Some(VersionOrUrl::Url(verbatim_url.into_url())),
            marker: requirement.marker,
            origin: requirement.origin,
        },
        Some(VersionOrUrl::VersionSpecifier(version_specifiers)) => Requirement {
            name: requirement.name,
            extras: requirement.extras,
            version_or_url: Some(VersionOrUrl::VersionSpecifier(version_specifiers)),
            marker: requirement.marker,
            origin: requirement.origin,
        },
        None => Requirement {
            name: requirement.name,
            extras: requirement.extras,
            version_or_url: None,
            marker: requirement.marker,
            origin: requirement.origin,
        },
    })
}

#[derive(Args, Debug)]
#[group(skip)]
pub struct Build {
    /// Requirements to include in the PEX.
    ///
    /// If no requirements are specified, an empty hermetic PEX will be generated.
    #[arg(
        value_name = "REQUIREMENT",
        help_heading = "Contents",
        value_parser = parse_url,
        verbatim_doc_comment
    )]
    requirements: Vec<Requirement<Url>>,

    /// Venvs containing distributions to include in the PEX.
    ///
    /// There must be at least one installed distribution satisfying each direct requirement. If
    /// no targets are specified, the interpreter for each specified venv is considered a target. A
    /// full transitive closure is confirmed for each target.
    #[arg(
        long,
        visible_alias = "venv",
        value_name = "PATH",
        help_heading = "Contents",
        conflicts_with = "wheels",
        verbatim_doc_comment
    )]
    venvs: Vec<PathBuf>,

    /// Wheels (or directories containing wheels) to include in the PEX.
    ///
    /// There must be at least one wheel satisfying each direct requirement. If no targets are
    /// specified, that is the only check performed; otherwise a full transitive closure is
    /// confirmed for each specified target.
    #[arg(
        long,
        visible_alias = "wheel",
        value_name = "PATH",
        help_heading = "Contents",
        conflicts_with = "venvs",
        verbatim_doc_comment
    )]
    wheels: Vec<PathBuf>,

    #[command(flatten)]
    sources: Sources,

    /// Specifies a requirement to exclude from the built PEX.
    ///
    /// Any distribution included in the PEX's resolve that matches the requirement is excluded
    /// from the built PEX along with all of its transitive dependencies that are not also required
    /// by other non-excluded distributions. At runtime, the PEX will boot without checking the
    /// excluded dependencies are available (say, via `--inherit-path`).
    #[arg(long = "exclude", help_heading = "Contents", verbatim_doc_comment)]
    excluded: Vec<String>,

    /// Specifies a transitive requirement to override when resolving.
    ///
    /// Overrides can either modify an existing dependency on a project name by changing extras,
    /// version constraints or markers or else they can completely swap out the dependency for a
    /// dependency on another project altogether. For the former, simply supply the requirement you
    /// wish. For example, specifying `--override cowsay==5.0` will override any transitive
    /// dependency on cowsay that has any combination of extras, version constraints or markers with
    /// the requirement `cowsay==5.0`. To completely replace cowsay with another library altogether,
    /// you can specify an override like `--override cowsay=my-cowsay>2`. This will replace any
    /// transitive dependency on cowsay that has any combination of extras, version constraints or
    /// markers with the requirement `my-cowsay>2`.
    #[arg(long = "override", help_heading = "Contents", verbatim_doc_comment)]
    overridden: Vec<String>,

    /// Ignore requirement resolution solver errors when building PEXes and later invoking them.
    #[arg(long, help_heading = "Contents")]
    ignore_errors: bool,

    /// Ensure the PEX is built with included tools.
    ///
    /// If this `pexrc` does not include tools, the build will fail fast.
    #[arg(long, help_heading = "Contents")]
    include_tools: bool,

    #[arg(long, help_heading = "Contents", help = PEX_PATH_HELP, long_help = PEX_PATH_LONG_HELP)]
    pex_path: Option<OsString>,

    /// Set the entry point to `module` or `module:symbol`.
    ///
    /// If just specifying `module`, Pex behaves like `python -m`, e.g. `python -m http.server`.
    /// If specifying `module:symbol`, Pex assumes symbol is a 0-arg callable and imports that
    /// symbol and invokes it as if via `sys.exit(symbol())`.
    #[arg(
        short = 'e',
        visible_short_alias = 'm',
        long,
        help_heading = "Entry Point",
        conflicts_with_all = ["script", "exe"],
        verbatim_doc_comment
    )]
    entry_point: Option<String>,

    /// Set the entry point to the given script.
    ///
    /// The script must be either a console script, gui script or data script found in one of the
    /// distributions in the PEX. For example: `pexrc build -c cowsay --venv /venv/dir cowsay`.
    #[arg(
        short = 'c',
        long,
        visible_alias = "console-script",
        help_heading = "Entry Point",
        conflicts_with_all = ["entry_point", "exe"],
        verbatim_doc_comment
    )]
    script: Option<String>,

    /// Set the entry point to an existing local python script.
    ///
    /// For example: `pexrc -X build --exe bin/my-python-script`. If the script contains PEP-723
    /// `dependencies` metadata, add these dependencies as requirements, which will be combined with
    /// other requirements specified on the command line as positional arguments.
    ///
    /// If the script contains PEP-723 `requires-python` metadata, treat this as the primary
    /// `--interpreter-constraint` and ensure all interpreters implied by any explicit `--target` or
    /// `--interpreter-constraint` command line arguments comply or else fail.
    #[arg(
        long,
        visible_aliases = ["executable", "python-script"],
        help_heading = "Entry Point",
        conflicts_with_all = ["entry_point", "script"],
        verbatim_doc_comment
    )]
    exe: Option<PathBuf>,

    /// Specifies an environment variable to bind the path of a resource in the PEX.
    ///
    /// The binding is specified in the form `<env var name>=<resource rel path>`. For example
    /// `WINDOWS_X64_CONSOLE_TRAMPOLINE=pex/windows/stubs/uv-trampoline-x86_64-console.exe` would
    /// look up the path of the `pex/windows/stubs/uv-trampoline-x86_64-console.exe` file on the
    /// `sys.path` and bind its absolute path to the `WINDOWS_X64_CONSOLE_TRAMPOLINE` environment
    /// variable. N.B.: resource paths must use the Unix path separator of `/`. These will be
    /// converted to the runtime host path separator as needed.
    #[arg(
        long = "bind-resource-path",
        help_heading = "Entry Point",
        verbatim_doc_comment
    )]
    bind_resource_paths: Vec<KeyValue>,

    /// Do not strip `PEX_*` environment variables when executing the PEX.
    #[arg(long, help_heading = "Entry Point")]
    no_strip_pex_env: bool,

    /// Environment variables to freeze in to the application environment.
    #[arg(long, help_heading = "Entry Point")]
    inject_env: Vec<KeyValue>,

    /// Command line arguments to the application to freeze in.
    ///
    /// Arguments that have `{pex.env.<env var name>}` placeholders will have them replaced with
    /// the corresponding environment variable value if set and '' otherwise.
    #[arg(long, help_heading = "Entry Point", verbatim_doc_comment)]
    inject_args: Vec<String>,

    /// Command line arguments to the Python interpreter to freeze in.
    ///
    /// For example, `-u` to disable buffering of `sys.stdout` and `sys.stderr` or `-W <arg>` to
    /// control Python warnings.
    #[arg(long, help_heading = "Entry Point", verbatim_doc_comment)]
    inject_python_args: Vec<String>,

    /// Inherit the contents of `sys.path` (including site-packages, user site-packages and
    /// PYTHONPATH) running the pex.
    ///
    /// Possible values: `false` (does not inherit `sys.path`), `fallback` (inherits `sys.path`
    /// after packaged dependencies), `prefer` (inherits `sys.path` before packaged dependencies).
    #[arg(long, help_heading = "Virtual Environment", verbatim_doc_comment)]
    inherit_path: Option<InheritPath>,

    /// Specify the PEX cache root directory to be used when the generated PEX file boots.
    ///
    /// If unspecified, the PEX will use a `pexrc` subdirectory of the default user cache directory
    /// for the runtime OS; e.g.: `~/.cache/pexrc` on Linux, `~/Library/Caches/pexrc` on macOS and
    /// `~\AppData\Local\pexrc` on Windows.
    #[arg(long, help_heading = "Virtual Environment", verbatim_doc_comment)]
    runtime_pex_root: Option<PathBuf>,

    /// The maximum number of threads to use when installing dependencies on first boot.
    ///
    /// Byt default, all cores will be utilized. This can be made explicit with
    /// `--max-install-jobs 0`.
    #[arg(long, help_heading = "Virtual Environment", verbatim_doc_comment)]
    max_install_jobs: Option<usize>,

    /// Whether to add the PEX venv scripts dir to the `$PATH`.
    ///
    /// If `prepend` or `append` is specified, then all scripts and console scripts provided by
    /// distributions in the pex file will be added to the `$PATH` in the corresponding position.
    #[arg(long, help_heading = "Virtual Environment", verbatim_doc_comment)]
    venv_bin_path: Option<BinPath>,

    /// Don't rewrite Python script shebangs to use Python isolated mode.
    ///
    /// This can be useful to, for example, to enable running the venv PEX itself or its Python
    /// scripts with a custom `PYTHONPATH`.
    #[arg(long, help_heading = "Virtual Environment", verbatim_doc_comment)]
    non_hermetic_venv_scripts: bool,

    /// Give the PEX venv access to the system `site-packages` dir.
    #[arg(long, help_heading = "Virtual Environment")]
    venv_system_site_packages: bool,

    #[command(flatten)]
    interpreter_selection_args: InterpreterSelectionArgs,

    #[arg(
        long = "target",
        help_heading = "Targets",
        value_parser = PythonPlatform::parse,
        help=PYTHON_PLATFORM_HELP,
        long_help=COMPLETE_PYTHON_PLATFORM_LONG_HELP,
    )]
    targets: Vec<PythonPlatform>,

    #[command(flatten)]
    compression_args: CompressionArgs,

    /// Instead of building a zipapp PEX, build a packed PEX.
    ///
    /// A Packed PEX is a directory containing a top-level `pex` script / `__main__.py` with wheels
    /// and other needed assets as-is under that. This can be useful in situations where using
    /// rsync-style transfer to ship incremental updates to large PEXes as opposed to having to ship
    /// the whole PEX.
    #[arg(long, help_heading = "Layout", verbatim_doc_comment)]
    packed: bool,

    /// Instead of booting via a Python shebang, boot via a Posix `sh` shebang.
    ///
    /// When running the PEX file directly (on Unix), instead of using a `#!/usr/bin/env python`
    /// style shebang, use a specially crafted `#!/bin/sh ...` shebang header that performs initial
    /// boot interpreter discovery smartly. If your PEX will target systems with a Posix shell at
    /// `/bin/sh` (overwhelmingly common on unix systems), this is the most robust and
    /// lowest-latency boot mode for repeated runs (at ~O(1ms)).
    ///
    /// N.B.: Both the Python and `sh` shebang headers are safe, but ignored on Windows systems.
    /// For those, you must run the PEX via Python (`python PEX`, `py PEX`, etc.) or else use an
    /// extension scheme you register with windows (Setting up a `.pyz` association is common).
    #[arg(
        long,
        help_heading = "Boot Mode",
        conflicts_with = "python_shebang",
        verbatim_doc_comment
    )]
    sh_boot: bool,

    /// The shebang line (`#!...\n`) to boot the PEX with minus the `#!` and trailing newline.
    ///
    /// This overrides the default behavior, which picks an environment Python interpreter
    /// compatible with the one used to build the PEX file.
    #[arg(
        long,
        help_heading = "Boot Mode",
        conflicts_with = "sh_boot",
        verbatim_doc_comment
    )]
    python_shebang: Option<String>,

    // TODO: XXX: This is not currently wired up properly in a comprehensive way. It's currently
    //  scattershot and needs a re-think.
    /// Emit runtime warnings on stderr.
    ///
    /// By default, only emit them when PEX_VERBOSE is set.
    #[arg(long, help_heading = "Boot Mode", verbatim_doc_comment)]
    emit_warnings: bool,

    /// The name of the generated PEX file.
    ///
    /// Omitting this will run PEX immediately and not save it to a file.
    ///
    /// If the name contains the {platform} placeholder, the most-specific platform tags supported
    /// by the PEX will be substituted. For example, for a multi-platform Linux x86-64, Mac ARM PEX
    /// containing platform-specific wheels, `-o 'example-{platform}.pex'` might expand to a PEX
    /// filename of `example-cp314-cp314-macosx_11_0_arm64.manylinux2014_x86_64.pex`.
    #[arg(
        short = 'o',
        long,
        visible_alias = "output-file",
        help_heading = "Output",
        verbatim_doc_comment
    )]
    output: Option<PathBuf>,

    /// Pass through arguments for execution of ephemeral PEXes.
    #[arg(allow_hyphen_values = true, last = true)]
    extra_args: Vec<String>,
}

impl Build {
    pub fn execute(self) -> anyhow::Result<()> {
        if self.include_tools && !cfg!(feature = "tools") {
            bail!(
                "You requested the PEX `--include-tools` but this `pexrc` binary was not built \
                with PEX_TOOLS support!"
            )
        }
        if !self.extra_args.is_empty()
            && let Some(output) = self.output.as_deref()
        {
            bail!(
                "Extra args ({extra_args}) are only applicable for ephemeral PEXes.\n\
                This PEX would be generated to {path}.",
                extra_args = shlex::try_join(self.extra_args.iter().map(String::as_str))?,
                path = output.display()
            );
        }
        assert!(
            self.venvs.is_empty() || self.wheels.is_empty(),
            "We should never get here by arrangement of a mutex condition between venvs and wheels \
            via clap `conflicts_with`."
        );
        let repository = if !self.venvs.is_empty() {
            Some(Repository::venvs(self.venvs)?)
        } else if !self.wheels.is_empty() {
            Some(Repository::wheels(self.wheels)?)
        } else {
            None
        };

        assert!(
            !(self.sh_boot && self.python_shebang.is_some()),
            "We should never get here by arrangement of a mutex condition between sh_boot and \
            python_shebang via clap `conflicts_with`."
        );
        let shebang = if self.sh_boot {
            Shebang::ShBoot
        } else if let Some(shebang) = self.python_shebang {
            Shebang::Custom(shebang)
        } else {
            Shebang::EnvCompatible
        };

        let mut requirements = Requirements::try_from(self.requirements)?;
        let mut interpreter_selection = self.interpreter_selection_args.finalize();
        let entry_point = if let Some(entry_point) = self.entry_point {
            Some(PexEntryPoint::EntryPoint(entry_point))
        } else if let Some(exe) = self.exe {
            let exe = Exe::try_from(exe)?;
            if let Some(metadata) = exe.metadata {
                requirements.append(metadata.dependencies)?;
                interpreter_selection.merge(exe.path.display(), metadata.requires_python)?;
            }
            Some(PexEntryPoint::Exe(exe.content))
        } else {
            self.script.map(PexEntryPoint::Script)
        };

        let platforms = self
            .targets
            .into_iter()
            .map(Platform::try_from)
            .collect::<anyhow::Result<Vec<_>>>()?;
        check_valid_platforms(&interpreter_selection, &platforms)?;

        let pex_paths = self
            .pex_path
            .map(|pex_path| env::split_paths(&pex_path).map(Cow::Owned).collect());

        let mut pex_info = RawPexInfo {
            build_properties: indexmap! {
                "pex_version" => json!(concatcp!("rc ", VERSION)),
                "pexrc_version" => json!(VERSION),
            },
            emit_warnings: self.emit_warnings,
            pex_paths,
            requirements: requirements
                .iter()
                .map(ToString::to_string)
                .map(Cow::Owned)
                .collect(),
            excluded: into_vec_of_cow(self.excluded),
            overridden: into_vec_of_cow(self.overridden),
            ignore_errors: self.ignore_errors,
            inherit_path: self.inherit_path,
            interpreter_constraints: into_vec_of_cow(
                interpreter_selection
                    .constraints
                    .into_constraints()
                    .into_iter()
                    .map(|ic| ic.to_string()),
            ),
            interpreter_selection_strategy: interpreter_selection
                .selection_strategy
                .map(InterpreterSelectionStrategy::from),
            strip_pex_env: if self.no_strip_pex_env {
                Some(false)
            } else {
                None
            },
            inject_args: into_vec_of_cow(self.inject_args),
            inject_env: into_optional_index_map_of_cow_cow(self.inject_env),
            inject_python_args: into_vec_of_cow(self.inject_python_args),
            bind_resource_paths: into_optional_index_map_of_cow_cow(self.bind_resource_paths),
            pexrc_root: self.runtime_pex_root.map(Cow::Owned),
            max_install_jobs: self.max_install_jobs.map(|max| max as isize),
            venv_bin_path: self.venv_bin_path,
            venv_hermetic_scripts: !self.non_hermetic_venv_scripts,
            venv_system_site_packages: self.venv_system_site_packages,
            ..Default::default()
        };
        let wheel_options = self.compression_args.into_wheel_options(None);

        let dependency_configuration = DependencyConfiguration::parse(
            pex_info.excluded.as_slice(),
            pex_info.overridden.as_slice(),
        )?;
        let (_dest_dir_guard, fingerprinted_project_wheels, requirements) =
            if !requirements.urls.is_empty() {
                let Some(repository) = repository.as_ref() else {
                    let mut message = format!(
                        "Cannot build requested {projects} without either `--wheels` or `--venv`s \
                        specified to resolve build systems from:",
                        projects = if requirements.urls.len() == 1 {
                            "project"
                        } else {
                            "projects"
                        }
                    );
                    match requirements.urls.as_slice() {
                        [req] => write!(&mut message, " {req}", req = req.requirement)?,
                        reqs => {
                            for req in reqs {
                                writeln!(&mut message)?;
                                write!(&mut message, "- {req}", req = req.requirement)?;
                            }
                        }
                    }
                    bail!(message);
                };
                let dest_dir = if let Some(output) = self.output.as_deref()
                    && let Some(parent) = output.parent()
                {
                    tempfile::tempdir_in(parent)?
                } else {
                    tempfile::tempdir()?
                };
                let resolved_projects = resolve_url_requirements(
                    requirements,
                    &platforms,
                    interpreter_selection.search_path.as_ref(),
                    repository,
                    &wheel_options,
                    &dependency_configuration,
                    dest_dir.path(),
                )?;
                (
                    Some(dest_dir),
                    Some(resolved_projects.wheels),
                    resolved_projects.dependencies,
                )
            } else {
                (None, None, requirements.into_requirements())
            };
        let (preferred_python, mut wheels) =
            if fingerprinted_project_wheels.is_some() && requirements.is_empty() {
                (None, vec![])
            } else {
                resolve_wheel_files(
                    repository.as_ref(),
                    &platforms,
                    &requirements,
                    &wheel_options,
                    &dependency_configuration,
                    pex_info.ignore_errors,
                )?
            };
        if let Some(fingerprinted_project_wheels) = fingerprinted_project_wheels {
            wheels.extend(fingerprinted_project_wheels);
        }

        // N.B.: Determines shebang for PEX.
        let preferred_platform = platforms.first();

        build_pex(
            preferred_python,
            interpreter_selection.search_path,
            preferred_platform,
            wheels,
            &mut pex_info,
            self.packed,
            shebang,
            self.output,
            self.extra_args,
            entry_point,
            self.sources,
        )
    }
}

#[derive(Clone, Hash, Eq, PartialEq)]
struct GitRef(String);

impl GitRef {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for GitRef {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Hash, Eq, PartialEq)]
enum Scheme {
    File,
    Http,
    Https,
    Git(Option<GitRef>),
}

#[derive(Clone, Hash, Eq, PartialEq)]
struct UrlRequirement {
    url: Url,
    scheme: Scheme,
    requirement: Requirement<Url>,
}

impl UrlRequirement {
    fn to_file_path(&self) -> PathBuf {
        PathBuf::from(self.url.path().to_owned())
    }

    fn file_name(&self) -> anyhow::Result<&OsStr> {
        Path::new(self.url.path())
            .file_name()
            .ok_or_else(|| anyhow!("Cannot identify a file name from {}.", self.url))
    }
}

impl Display for UrlRequirement {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{req} @ {url}", req = self.requirement, url = self.url)
    }
}

enum CategorizedRequirement {
    DirectReference(UrlRequirement),
    Requirement(Requirement<Url>),
}

impl CategorizedRequirement {
    fn categorize(mut requirement: Requirement<Url>) -> anyhow::Result<Self> {
        match requirement.version_or_url.take() {
            Some(VersionOrUrl::Url(url)) => {
                let scheme = if let Some((vcs, _)) = url.scheme().split_once('+') {
                    match vcs {
                        "git" => {
                            let git_ref = if let Some((_, git_ref)) = url.path().rsplit_once('@') {
                                Some(GitRef(git_ref.to_string()))
                            } else {
                                None
                            };
                            Scheme::Git(git_ref)
                        }
                        _ => bail!(
                            "The direct reference VCS requirement {url} uses unsupported vcs \
                            '{vcs}'."
                        ),
                    }
                } else {
                    match url.scheme() {
                        "file" => Scheme::File,
                        "http" => Scheme::Http,
                        "https" => Scheme::Https,
                        other => bail!(
                            "The direct reference requirement {url} uses unsupported scheme \
                            '{other}'."
                        ),
                    }
                };
                Ok(Self::DirectReference(UrlRequirement {
                    url,
                    scheme,
                    requirement,
                }))
            }
            version => {
                requirement.version_or_url = version;
                Ok(Self::Requirement(requirement))
            }
        }
    }
}

struct Requirements {
    reqs: Vec<Requirement<Url>>,
    urls: Vec<UrlRequirement>,
}

impl Requirements {
    fn iter(&self) -> impl Iterator<Item = &Requirement<Url>> {
        self.urls
            .iter()
            .map(|url_req| &url_req.requirement)
            .chain(self.reqs.iter())
    }

    fn append(
        &mut self,
        requirements: impl IntoIterator<IntoIter = impl ExactSizeIterator<Item = Requirement<Url>>>,
    ) -> anyhow::Result<()> {
        for requirement in requirements {
            match CategorizedRequirement::categorize(requirement)? {
                CategorizedRequirement::DirectReference(url_requirement) => {
                    self.urls.push(url_requirement)
                }
                CategorizedRequirement::Requirement(requirement) => self.reqs.push(requirement),
            }
        }
        Ok(())
    }

    fn into_requirements(self) -> Vec<Requirement<Url>> {
        self.urls
            .into_iter()
            .map(|url_req| url_req.requirement)
            .chain(self.reqs)
            .collect()
    }
}

impl TryFrom<Vec<Requirement<Url>>> for Requirements {
    type Error = anyhow::Error;

    fn try_from(requirements: Vec<Requirement<Url>>) -> anyhow::Result<Self> {
        let mut reqs = Requirements {
            urls: Vec::with_capacity(requirements.len()),
            reqs: Vec::with_capacity(requirements.len()),
        };
        reqs.append(requirements)?;
        Ok(reqs)
    }
}

struct ResolvedProjects {
    wheels: IndexSet<FingerprintedWheel>,
    dependencies: Vec<Requirement<Url>>,
}

#[allow(clippy::too_many_arguments)]
#[instrument(level = "debug", skip_all)]
fn resolve_url_requirements(
    requirements: Requirements,
    platforms: &[Platform],
    search_path: Option<&SearchPath>,
    repository: &Repository,
    wheel_options: &WheelOptions,
    dependency_configuration: &DependencyConfiguration,
    dest_dir: &Path,
) -> anyhow::Result<ResolvedProjects> {
    let mut platform_details = HashSet::with_capacity(platforms.len());
    let mut interpreters = IndexSet::with_capacity(platforms.len());
    for platform in platforms {
        match platform {
            Platform::Details(platform) => {
                platform_details.insert(platform);
            }
            Platform::Interpreter(interpreter) => {
                interpreters.insert(interpreter.clone());
            }
        }
    }
    if let Repository::Venvs(venvs) = repository {
        for venv in venvs.borrow_venvs() {
            interpreters.insert(Cow::Borrowed(&venv.interpreter));
        }
    }
    if !platform_details.is_empty() {
        let mut ics = Vec::with_capacity(2 * platform_details.len());
        for platform in &platform_details {
            ics.push(InterpreterConstraint::matching_platform(
                platform,
                VersionSpecificity::MajorMinor,
            )?)
        }
        for platform in &platform_details {
            ics.push(InterpreterConstraint::matching_platform(
                platform,
                VersionSpecificity::Major,
            )?)
        }
        let search_path = if let Some(search_path) = search_path {
            search_path.clone()
        } else {
            SearchPath::from_env()?
        };
        let identification_script = IdentifyInterpreter::read(&mut Scripts::Embedded)?;
        let possibly_compatible_python_exes = InterpreterConstraints::from(ics)
            .iter_possibly_compatible_python_exes(SelectionStrategy::Newest, search_path, false)?
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|python_exe| Interpreter::load(&python_exe, &identification_script))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut major_only_match_interpreters =
            Vec::with_capacity(possibly_compatible_python_exes.len());
        let mut platforms_to_find_interpreters_for =
            platform_details.iter().copied().collect::<IndexSet<_>>();
        for interpreter in possibly_compatible_python_exes {
            let version = interpreter.details.version;
            let major_minor = [version.major, version.minor];
            let mut matched = false;
            for platform in &platform_details {
                let python_implementation = platform.python_implementation()?;
                if [python_implementation.major, python_implementation.minor] == major_minor {
                    matched = true;
                    platforms_to_find_interpreters_for.shift_remove(platform);
                }
            }
            if matched {
                interpreters.insert(Cow::Owned(interpreter));
            } else {
                major_only_match_interpreters.push(interpreter)
            }
        }
        let mut last_ditch_interpreters = Vec::with_capacity(major_only_match_interpreters.len());
        for interpreter in major_only_match_interpreters {
            let version = interpreter.details.version;
            let mut matched = false;
            for platform in &platform_details {
                if platforms_to_find_interpreters_for.contains(platform) {
                    continue;
                }
                let python_implementation = platform.python_implementation()?;
                if python_implementation.major == version.major {
                    matched = true;
                    platforms_to_find_interpreters_for.shift_remove(platform);
                }
            }
            if matched {
                interpreters.insert(Cow::Owned(interpreter));
            } else {
                last_ditch_interpreters.push(interpreter)
            }
        }
        if !platform_details.is_empty() {
            last_ditch_interpreters
                .into_iter()
                .next()
                .map(|interpreter| interpreters.insert(Cow::Owned(interpreter)));
        }
    }
    if interpreters.is_empty() {
        bail!("Failed to resolve any interpreters to build projects with!")
    }

    let mut resolved_projects = ResolvedProjects {
        wheels: IndexSet::with_capacity(requirements.urls.len()),
        dependencies: requirements.reqs,
    };

    let mut seen_urls = HashSet::new();
    let mut process_resolved_project =
        |project: ResolvedProject| -> anyhow::Result<Vec<UrlRequirement>> {
            let mut url_requirements = Vec::new();
            if resolved_projects.wheels.insert(project.wheel) {
                for dependency in project.dependencies {
                    match CategorizedRequirement::categorize(dependency)? {
                        CategorizedRequirement::DirectReference(url_requirement) => {
                            if !seen_urls.contains(&url_requirement.url) {
                                seen_urls.insert(url_requirement.url.clone());
                                url_requirements.push(url_requirement)
                            }
                        }
                        CategorizedRequirement::Requirement(dependency) => {
                            resolved_projects.dependencies.push(dependency)
                        }
                    }
                }
            }
            Ok(url_requirements)
        };

    let mut url_requirements = requirements.urls;
    while !url_requirements.is_empty() {
        for project in mem::take(&mut url_requirements)
            .into_iter()
            .flat_map(|url_requirement| {
                interpreters
                    .iter()
                    .filter_map(|interpreter| {
                        if url_requirement
                            .requirement
                            .marker
                            .evaluate(interpreter.platform_details().marker_env(), &[])
                        {
                            Some((url_requirement.clone(), interpreter))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|(url_requirement, interpreter)| {
                resolve_project(
                    url_requirement,
                    interpreter.as_ref(),
                    repository,
                    wheel_options,
                    dependency_configuration,
                    dest_dir,
                )
            })
            .collect::<anyhow::Result<Vec<_>>>()?
        {
            url_requirements.append(&mut process_resolved_project(project)?);
        }
    }
    Ok(resolved_projects)
}

struct BuildSystemVenvBuilder<'a> {
    interpreter: &'a Interpreter,
    repository: &'a Repository<'a>,
    wheel_options: &'a WheelOptions,
    dependency_configuration: &'a DependencyConfiguration,
    include_system_site_packages: bool,
}

impl<'a> VenvBuilder for BuildSystemVenvBuilder<'a> {
    fn create_at(
        &self,
        subject: &impl Display,
        venv_dir: PathBuf,
        requirements: &[Requirement<Url>],
    ) -> anyhow::Result<Virtualenv<'_>> {
        let (_, fingerprinted_wheels) = resolve_wheel_files(
            Some(self.repository),
            &[Platform::Interpreter(Cow::Borrowed(self.interpreter))],
            requirements,
            self.wheel_options,
            self.dependency_configuration,
            false,
        )?;
        let proxy_bytes = read_proxy_content(SimplifiedTarget::current()?, false)?;
        let proxy_source = ProxySource::Embedded(&proxy_bytes);
        let linker = PythonProxyLinker(&proxy_source);
        let mut scripts = Scripts::Embedded;
        let venv = Virtualenv::create(
            Cow::Borrowed(self.interpreter),
            Cow::Owned(venv_dir),
            linker,
            &mut scripts,
            self.include_system_site_packages,
            false,
            None,
        )?;
        let provenance = Arc::new(Provenance::new(format!(
            "populating PEP-517 build-system requires for {subject}"
        )));
        fingerprinted_wheels
            .into_par_iter()
            .try_for_each(|fingerprinted_wheel| {
                let wheel_file = WheelFile::parse_file_name(&fingerprinted_wheel.file_name)?;
                let whl_zip = ZipArchive::new(File::open(&fingerprinted_wheel.path)?.into_file())?;
                let metadata_dirs = wheel_file.metadata_dirs_from_zip(
                    &whl_zip,
                    fingerprinted_wheel.path.display(),
                    None,
                )?;
                populate_whl_zip(
                    &venv,
                    &venv.interpreter.details.path,
                    &fingerprinted_wheel.path,
                    Some(whl_zip),
                    wheel_file.raw_project_name,
                    &metadata_dirs,
                    &proxy_source,
                    provenance.clone(),
                )
            })?;
        if let Some(collision_report) = Arc::try_unwrap(provenance)
            .expect("Provenance use is complete")
            .into_collision_report()?
        {
            warn!("{collision_report}");
        }
        write_pex_extra_sys_path_support_files(&venv, &mut scripts)?;
        Ok(venv)
    }
}

struct Whl<D: Display, R: Read + Seek> {
    zip: ZipArchive<R>,
    zip_source: D,
}

impl<D: Display, R: Read + Seek> MetadataReader for Whl<D, R> {
    fn locate_dirs(&mut self, wheel_file: &WheelFile) -> anyhow::Result<MetadataDirs> {
        wheel_file.metadata_dirs_from_zip(&self.zip, &self.zip_source, None)
    }

    fn read(
        &mut self,
        metadata_dirs: &MetadataDirs,
        _wheel_file: &WheelFile,
        file_name: &str,
    ) -> anyhow::Result<String> {
        let dist_info_dir = metadata_dirs.dist_info_dir();
        Ok(io::read_to_string(
            self.zip
                .by_name_ex(&format!("{dist_info_dir}/{file_name}"))?,
        )?)
    }
}

struct ResolvedProject {
    wheel: FingerprintedWheel,
    dependencies: Vec<Requirement<Url>>,
}

fn is_tgz(path: &Path) -> bool {
    path.file_name()
        .and_then(|file_name| {
            path.file_prefix().map(|prefix| {
                let (_, full_extension) = file_name.split_at(prefix.len());
                [".tar.gz", ".tgz"]
                    .into_iter()
                    .any(|ext| full_extension == OsStr::new(ext))
            })
        })
        .unwrap_or_default()
}

fn has_ext(path: &Path, ext: &str) -> bool {
    path.extension()
        .map(|extension| extension == OsStr::new(ext))
        .unwrap_or_default()
}

fn is_whl(path: &Path) -> bool {
    has_ext(path, "whl")
}

fn is_zip(path: &Path) -> bool {
    has_ext(path, "zip")
}

#[instrument(level = "debug", skip_all, fields(url = %url))]
fn download_project(url: Url, dst: &mut impl Write) -> anyhow::Result<()> {
    let mut response = request::get(url)?.error_for_status()?;
    io::copy(&mut response, dst)?;
    Ok(())
}

#[instrument(level = "debug", skip_all, fields(url = %git_url))]
fn git_clone_project(
    git_url: Url,
    git_ref: Option<GitRef>,
    clone_dir: &Path,
) -> anyhow::Result<()> {
    let create_git_command = || {
        let mut cmd = Command::new("git");
        cmd.current_dir(clone_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    };
    let result = create_git_command()
        .arg("clone")
        .arg(git_url.as_str())
        .arg(".")
        .spawn()
        .and_then(|process| process.wait_with_output())
        .map_err(|err| anyhow!("Failed to clone {git_url}: {err}"))?;
    result.status.exit_ok().map_err(|err| {
        anyhow!(
            "Failed to clone {git_url}: {err}\n\
            Stderr from git:\n\
            {stderr}",
            stderr = String::from_utf8_lossy(&result.stderr).trim_end()
        )
    })?;
    if let Some(git_ref) = git_ref.as_ref() {
        let result = create_git_command()
            .args(["reset", "--hard"])
            .arg(git_ref.as_str())
            .spawn()
            .and_then(|process| process.wait_with_output())
            .map_err(|err| anyhow!("Failed to reset {git_url} to {git_ref}: {err}"))?;
        result.status.exit_ok().map_err(|err| {
            anyhow!(
                "Failed to reset {git_url} to {git_ref}: {err}\n\
                Stderr from git:\n\
                {stderr}",
                stderr = String::from_utf8_lossy(&result.stderr).trim_end()
            )
        })?;
    }
    Ok(())
}

fn move_top_dir(unpack_chroot: &Path, dest_dir: &Path) -> anyhow::Result<ProjectDir> {
    let entries = unpack_chroot
        .read_dir()?
        .map(|entry| Ok(entry?))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let top_dir = if entries.is_empty() {
        bail!("Empty top dir!")
    } else if entries.len() == 1 {
        let entry = entries.into_iter().next().expect("We confirmed 1 entry.");
        if !entry.metadata()?.is_dir() {
            bail!("Top entry is a file!")
        }
        entry.path()
    } else {
        struct AmbiguousTopDir(Vec<std::fs::DirEntry>);
        impl Display for AmbiguousTopDir {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                write!(f, "More than one top dir:")?;
                for entry in &self.0 {
                    writeln!(f)?;
                    write!(f, "- {}", entry.file_name().display())?;
                }
                Ok(())
            }
        }
        bail!("{}", AmbiguousTopDir(entries))
    };

    let project_dir = dest_dir.join(
        top_dir
            .file_name()
            .expect("We confirmed a file name nested in the temp_dir."),
    );
    fs::rename(top_dir, &project_dir)?;
    ProjectDir::new(project_dir)
}

#[instrument(level = "debug", skip_all)]
fn unpack_tarball(tarball: impl Read, dest_dir: &Path) -> anyhow::Result<ProjectDir> {
    let mut tar = tar::Archive::new(tarball);
    let temp_dir = tempfile::tempdir_in(dest_dir)?;
    tar.unpack(temp_dir.path())?;
    move_top_dir(temp_dir.path(), dest_dir)
}

#[instrument(level = "debug", skip_all)]
fn unpack_zip(zip_path: &Path, dest_dir: &Path) -> anyhow::Result<ProjectDir> {
    let zip = ZipArchive::new(
        File::open(zip_path)
            .with_context(|| format!("Failed to open zip at {}", zip_path.display()))?,
    )?;
    let metadata = zip.metadata();
    let temp_dir = tempfile::tempdir_in(dest_dir)?;
    (0..zip.len())
        .into_par_iter()
        .try_for_each(|index| -> anyhow::Result<()> {
            let mut zip = unsafe {
                ZipArchive::unsafe_new_with_metadata(File::open(zip_path)?, metadata.clone())
            };
            let mut zip_file = zip.by_index(index)?;
            let dst = temp_dir.path().join(zip_file.name());
            if zip_file.is_dir() {
                fs::create_dir_all(dst)?;
            } else {
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                io::copy(&mut zip_file, &mut File::create(dst)?)?;
            }
            Ok(())
        })?;
    move_top_dir(temp_dir.path(), dest_dir).with_context(|| {
        format!(
            "Failed to move {} to {}",
            temp_dir.path().display(),
            dest_dir.display()
        )
    })
}

#[instrument(level = "debug", skip_all, fields(project = %project))]
fn resolve_project(
    project: UrlRequirement,
    interpreter: &Interpreter,
    repository: &Repository,
    wheel_options: &WheelOptions,
    dependency_configuration: &DependencyConfiguration,
    dest_dir: &Path,
) -> anyhow::Result<ResolvedProject> {
    let (_downloaded_guard, project, sub_dir, project_req) = {
        let mut sub_dir: Option<PathBuf> = None;
        if let Some(fragment) = project.url.fragment() {
            for (name, value) in form_urlencoded::parse(fragment.as_bytes()) {
                if name.as_ref() == "subdirectory" {
                    if let Some(sub_dir) = sub_dir {
                        warn!(
                            "Overriding earlier subdirectory fragment param {sub_dir} with \
                            {value} from {url}.",
                            sub_dir = sub_dir.display(),
                            url = project.url
                        )
                    }
                    sub_dir = Some(PathBuf::from(value.into_owned()));
                }
            }
        }
        match project.scheme {
            Scheme::File => (
                None,
                project.to_file_path(),
                sub_dir,
                Some(project.requirement),
            ),
            Scheme::Http | Scheme::Https => {
                let file_name = project.file_name()?;
                let chroot_dir = tempfile::tempdir_in(dest_dir)?;
                let dst = chroot_dir.path().join(file_name);
                download_project(project.url, &mut File::create(&dst)?)?;
                (Some(chroot_dir), dst, sub_dir, Some(project.requirement))
            }
            Scheme::Git(git_ref) => {
                let (_, url) = project
                    .url
                    .as_str()
                    .split_once('+')
                    .expect("We already parsed git+<url> to get here.");
                let mut url = Url::parse(url)?;
                url.set_fragment(None);
                if git_ref.is_some() {
                    let (path_prefix, _) = project
                        .url
                        .path()
                        .rsplit_once('@')
                        .expect("We already parsed <path_prefix>@<ref> to get here.");
                    url.set_path(path_prefix);
                }
                let clone_temp_dir = tempfile::tempdir_in(dest_dir)?;
                let clone_dir = clone_temp_dir.path();
                let project_dir = clone_dir.to_path_buf();
                git_clone_project(url, git_ref, clone_dir)?;
                (
                    Some(clone_temp_dir),
                    project_dir,
                    sub_dir,
                    Some(project.requirement),
                )
            }
        }
    };

    let include_system_site_packages = false; // TODO: XXX: plumb this
    let venv_builder = BuildSystemVenvBuilder {
        interpreter,
        repository,
        wheel_options,
        dependency_configuration,
        include_system_site_packages,
    };
    let build_err_context = || {
        if let Some(req) = project_req.as_ref() {
            format!("Failed to build wheel for Python project \"{req}\".",)
        } else {
            format!(
                "Failed to build wheel for Python project at `{}`.",
                project.display()
            )
        }
    };
    let wheel_path = if project.is_dir() {
        let project_dir = if let Some(sub_dir) = sub_dir {
            ProjectDir::new(project.join(sub_dir))?
        } else {
            ProjectDir::new(&project)?
        };
        build_wheel(project_dir, dest_dir, &venv_builder, &mut Scripts::Embedded)
            .with_context(build_err_context)?
    } else if is_tgz(&project) {
        let mut project_dir = unpack_tarball(
            flate2::read::GzDecoder::new(File::open(&project)?),
            dest_dir,
        )?;
        if let Some(sub_dir) = sub_dir {
            project_dir.push_sub_dir(sub_dir)?;
        }
        build_wheel(project_dir, dest_dir, &venv_builder, &mut Scripts::Embedded)
            .with_context(build_err_context)?
    } else if is_zip(&project) {
        let mut project_dir = unpack_zip(&project, dest_dir).with_context(|| {
            format!(
                "Failed to unpack {} to {}",
                project.display(),
                dest_dir.display()
            )
        })?;
        if let Some(sub_dir) = sub_dir {
            project_dir.push_sub_dir(sub_dir)?;
        }
        build_wheel(project_dir, dest_dir, &venv_builder, &mut Scripts::Embedded)
            .with_context(build_err_context)?
    } else if is_whl(&project) {
        project
    } else {
        if let Some(req) = project_req {
            bail!(
                "Can't identify {req} downloaded at {}.\n\
                It is not a whl and does not appear to hold Python project sources.",
                project.display()
            )
        } else {
            bail!(
                "Can't identify {}.\n\
                It is not a whl and does not appear to hold Python project sources.",
                project.display()
            )
        }
    };

    let wheel = cache_wheel(&wheel_path, wheel_options)?;
    let wheel_file = WheelFile::parse_file_name(&wheel.file_name)?;
    let zip = ZipArchive::new(File::open(&wheel.path)?.into_file())?;
    let zip_source = wheel_path.display();
    let metadata_dirs = wheel_file.metadata_dirs_from_zip(&zip, &zip_source, None)?;
    let mut whl = Whl { zip, zip_source };
    let whl_metadata = WheelMetadata::parse(wheel_file, metadata_dirs, &mut whl)?;
    let dependencies = {
        let mut requirements = Vec::with_capacity(whl_metadata.requires_dists.len());
        let extras = if let Some(project_req) = project_req {
            project_req.extras
        } else {
            vec![]
        };
        for mut requirement in whl_metadata.requires_dists {
            if requirement.evaluate_markers(interpreter.marker_env(), &extras) {
                requirement.marker = MarkerTree::TRUE;
                requirements.push(requirement)
            }
        }
        requirements
    };
    Ok(ResolvedProject {
        wheel,
        dependencies,
    })
}

fn check_valid_platforms(
    interpreter_selection: &InterpreterSelection,
    platforms: &[Platform],
) -> anyhow::Result<()> {
    let mut invalid_targets = Vec::with_capacity(platforms.len());
    for platform in platforms.iter().filter_map(|platform| match platform {
        Platform::Details(platform) => Some(platform),
        _ => None,
    }) {
        if !interpreter_selection
            .constraints
            .contains(platform.python_implementation()?)
        {
            invalid_targets.push(platform);
        }
    }
    if invalid_targets.is_empty() {
        return Ok(());
    }

    let mut message = format!(
        "The following {targets_do} not satisfy {ics}:",
        targets_do = if invalid_targets.len() == 1 {
            "target does"
        } else {
            "targets do"
        },
        ics = interpreter_selection.constraints
    );
    match invalid_targets.as_slice() {
        &[target] => {
            writeln!(&mut message, " {target}")?;
        }
        _ => {
            for target in invalid_targets {
                writeln!(&mut message)?;
                write!(&mut message, "- {target}")?;
            }
        }
    }
    bail!(message)
}

fn into_optional_index_map_of_cow_cow<'a>(
    items: Vec<KeyValue>,
) -> Option<IndexMap<Cow<'a, str>, Cow<'a, str>>> {
    if items.is_empty() {
        None
    } else {
        Some(items.into_iter().map(KeyValue::into_cow_tuple).collect())
    }
}

fn into_vec_of_cow<'a>(items: impl IntoIterator<Item = String>) -> Vec<Cow<'a, str>> {
    items.into_iter().map(Cow::Owned).collect()
}

fn adjust_requirements(
    pex_info: &mut RawPexInfo,
    wheels: &[FingerprintedWheel],
) -> anyhow::Result<()> {
    if pex_info.requirements.is_empty() {
        pex_info.requirements.extend(
            wheels
                .iter()
                .map(|wheel| {
                    wheel
                        .path
                        .file_name()
                        .and_then(OsStr::to_str)
                        .ok_or_else(|| {
                            anyhow!(
                                "The wheel at {path} does not have a valid file name.",
                                path = wheel.path.display()
                            )
                        })
                        .and_then(WheelFile::parse_file_name)
                        .map(|wheel_file| Cow::Owned(wheel_file.project_name.to_string()))
                })
                .collect::<anyhow::Result<IndexSet<_>>>()?,
        );
    }
    Ok(())
}

#[instrument(level = "debug", skip_all)]
fn resolve_wheel_files<'a>(
    repository: Option<&'a Repository<'a>>,
    platforms: &'a [Platform<'a>],
    requirements: &[Requirement<Url>],
    wheel_options: &WheelOptions,
    dependency_configuration: &DependencyConfiguration,
    ignore_errors: bool,
) -> anyhow::Result<(Option<&'a Interpreter>, Vec<FingerprintedWheel>)> {
    match repository {
        Some(repository @ Repository::Venvs(venvs)) => {
            let (preferred_python, wheels) = resolve_wheels_from_venvs(
                repository,
                venvs,
                platforms,
                wheel_options,
                requirements,
                dependency_configuration,
                ignore_errors,
            )?;
            Ok((preferred_python, wheels))
        }
        Some(Repository::Wheels(wheel_files)) => {
            let wheels = resolve_wheels_from_files(
                wheel_files,
                platforms,
                requirements,
                dependency_configuration,
                ignore_errors,
            )?;
            let fingerprinted_wheels = wheels
                .into_par_iter()
                .map(|wheel| cache_wheel(wheel, wheel_options))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let _preferred_platform = platforms.iter().next();
            Ok((None, fingerprinted_wheels))
        }
        None => {
            if requirements.is_empty() {
                let _preferred_platform = platforms.iter().next();
                Ok((None, vec![]))
            } else {
                bail!("Cannot resolve requirements without either `--wheels` or `--venv`.")
            }
        }
    }
}

#[derive(Clone, Hash, Eq, PartialEq)]
enum Platform<'a> {
    Details(PlatformDetails<'a>),
    Interpreter(Cow<'a, Interpreter>),
}

impl<'a> Display for Platform<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Platform::Details(platform) => write!(f, "{}", platform),
            Platform::Interpreter(interpreter) => {
                write!(f, "{}", interpreter.details.path.display())
            }
        }
    }
}

impl<'a> Platform<'a> {
    pub(crate) fn implementation(&self) -> anyhow::Result<PythonImplementation> {
        match self {
            Self::Interpreter(interpreter) => Ok(interpreter.details.python_implementation()),
            Self::Details(platform) => platform.python_implementation(),
        }
    }
}

impl<'a> TryFrom<PythonPlatform> for Platform<'a> {
    type Error = anyhow::Error;

    fn try_from(target: PythonPlatform) -> Result<Self, Self::Error> {
        match target {
            PythonPlatform::Spec(spec) => python_platform::parse(Cow::Owned(spec), None, None)
                .map_err(|err| {
                    anyhow!(
                        "Failed to parse --target <spec>: {err}\n\
                            {PYTHON_PLATFORM_LONG_HELP}"
                    )
                })
                .map(Platform::Details),
            PythonPlatform::Interpreter(path) => IdentifyInterpreter::read(&mut Scripts::Embedded)
                .and_then(|identification_script| Interpreter::load(&path, &identification_script))
                .map(|interpreter| Platform::Interpreter(Cow::Owned(interpreter))),
        }
    }
}

struct VenvRepository(PathBuf);

impl MetadataReader for VenvRepository {
    fn locate_dirs(&mut self, wheel_file: &WheelFile) -> anyhow::Result<MetadataDirs> {
        MetadataDirs::locate_in_dir(&self.0, &wheel_file.project_name, &wheel_file.version)
    }

    fn read(
        &mut self,
        metadata_dirs: &MetadataDirs,
        _wheel_file: &WheelFile,
        file_name: &str,
    ) -> anyhow::Result<String> {
        let mut metadata_file_path = self.0.join(metadata_dirs.dist_info_dir().as_path());
        metadata_file_path.push(file_name);
        Ok(fs::read_to_string(metadata_file_path)?)
    }
}

fn resolve_wheels_from_venvs<'a>(
    repository: &Repository,
    virtualenvs: &'a Virtualenvs<'a>,
    platforms: &'a [Platform<'a>],
    wheel_options: &WheelOptions,
    requirements: &[Requirement<Url>],
    dependency_configuration: &DependencyConfiguration,
    ignore_errors: bool,
) -> anyhow::Result<(Option<&'a Interpreter>, Vec<FingerprintedWheel>)> {
    let (venvs, mut repositories) = {
        let inventory = virtualenvs
            .borrow_venvs()
            .into_par_iter()
            .map(inventory_venv)
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut venvs = IndexMap::with_capacity(inventory.len());
        let mut repositories = Vec::with_capacity(inventory.len());
        for (venv, repository) in inventory {
            venvs.insert(venv.prefix().join(&venv.site_packages_relpath), venv);
            repositories.push(repository);
        }
        (venvs, repositories)
    };

    let mut wheels = IndexMap::new();
    let mut errors_by_platform = IndexMap::new();
    let platforms = if platforms.is_empty() {
        virtualenvs.borrow_platforms()
    } else {
        platforms
    };
    let mut preferred_python = None;
    for (installed_wheels, venv_repository) in &mut repositories {
        for platform in platforms {
            if wheels.contains_key(platform) {
                continue;
            }
            let wheel_files = installed_wheels
                .keys()
                .map(|file_name| WheelFile::parse_file_name(file_name))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let result = match platform {
                Platform::Details(platform) => resolve_wheels(
                    "venv",
                    platform,
                    requirements,
                    wheel_files,
                    venv_repository,
                    dependency_configuration,
                    None,
                    ignore_errors,
                ),
                Platform::Interpreter(interpreter) => resolve_wheels(
                    "venv",
                    interpreter.as_ref(),
                    requirements,
                    wheel_files,
                    venv_repository,
                    dependency_configuration,
                    None,
                    ignore_errors,
                ),
            };
            match result {
                Ok(resolved_wheels) => {
                    for installed_wheel in resolved_wheels
                        .keys()
                        .map(|file_name| installed_wheels.get(*file_name))
                    {
                        wheels
                            .entry(platform)
                            .or_insert_with(|| {
                                (
                                    venvs.get(&venv_repository.0).expect(
                                        "All venvs are indexed by their site-packages path.",
                                    ),
                                    IndexSet::new(),
                                )
                            })
                            .1
                            .insert(installed_wheel.expect(
                                "Resolved wheel_files were derived from installed_wheels.",
                            ));
                    }
                    if preferred_python.is_none() {
                        preferred_python =
                            venvs.get(&venv_repository.0).map(|venv| &venv.interpreter);
                    }
                    break;
                }
                Err(err) => errors_by_platform
                    .entry(platform)
                    .or_insert_with(Vec::new)
                    .push(err),
            }
        }
    }
    if !errors_by_platform.is_empty() {
        struct ErrorsByPlatform<'a>(IndexMap<&'a Platform<'a>, Vec<anyhow::Error>>);
        impl<'a> Display for ErrorsByPlatform<'a> {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                let count = self.0.len();
                write!(
                    f,
                    "Failed to resolve wheels for {count} {platforms}:",
                    platforms = if count == 1 { "platform" } else { "platforms" },
                )?;
                for (index, (platform, errors)) in self.0.iter().enumerate() {
                    writeln!(f)?;
                    write!(f, "{index:>3}. ", index = index + 1)?;
                    match *platform {
                        Platform::Details(platform) => {
                            write!(f, "Target {platform}")?;
                        }
                        Platform::Interpreter(interpreter) => {
                            if interpreter.is_venv() {
                                write!(f, "Venv @ {}", interpreter.details.prefix.display())?;
                            } else {
                                write!(f, "Interpreter @ {}", interpreter.details.path.display())?;
                            }
                        }
                    }
                    for error in errors.iter() {
                        writeln!(f)?;
                        write!(f, "    - {error}")?;
                    }
                }
                Ok(())
            }
        }
        bail!("{}", ErrorsByPlatform(errors_by_platform))
    }

    let mut wheel_paths = vec![];
    for (_, (venv, installed_wheels)) in wheels {
        let install_paths = InstallPaths::for_venv(venv)?;
        let include_system_site_packages = false; // TODO: XXX: plumb this
        let venv_builder = BuildSystemVenvBuilder {
            interpreter: &venv.interpreter,
            repository,
            wheel_options,
            dependency_configuration,
            include_system_site_packages,
        };
        wheel_paths.append(
            &mut installed_wheels
                .into_par_iter()
                .map(|installed_wheel| {
                    pack_wheel(
                        installed_wheel,
                        &install_paths,
                        wheel_options,
                        &venv_builder,
                    )
                })
                .collect::<anyhow::Result<Vec<_>>>()?,
        );
    }
    Ok((preferred_python, wheel_paths))
}

fn wheel_path(file_name: &str, wheel_options: &WheelOptions) -> anyhow::Result<PathBuf> {
    let options_dir_name = match (
        wheel_options.compression_method,
        wheel_options.compression_level,
    ) {
        (CompressionMethod::Stored, None) => Cow::Borrowed("stored-default"),
        (CompressionMethod::Deflated, None) => Cow::Borrowed("deflated-default"),
        (CompressionMethod::Zstd, None) => Cow::Borrowed("zstd-default"),
        (CompressionMethod::Stored, Some(level)) => Cow::Owned(format!("stored-{level}")),
        (CompressionMethod::Deflated, Some(level)) => Cow::Owned(format!("deflated-{level}")),
        (CompressionMethod::Zstd, Some(level)) => Cow::Owned(format!("zstd-{level}")),
        _ => bail!(
            "Unsupported compression method {method:?}",
            method = wheel_options.compression_method
        ),
    };
    let mut whl_path = CacheDir::Wheels.path()?.join(options_dir_name.as_ref());
    whl_path.push(file_name);
    Ok(whl_path)
}

#[derive(Eq)]
struct FingerprintedWheel {
    path: PathBuf,
    file_name: String,
    fingerprint: Fingerprint,
}

impl PartialEq for FingerprintedWheel {
    fn eq(&self, other: &Self) -> bool {
        self.file_name == other.file_name && self.fingerprint == other.fingerprint
    }
}

impl Hash for FingerprintedWheel {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write(self.file_name.as_bytes());
        state.write(self.fingerprint.as_bytes())
    }
}

type WheelDigestAlgorithm = Sha256;
static ALGORITHM_NAME: &str = "sha256";

fn cache_wheel(wheel: &Path, wheel_options: &WheelOptions) -> anyhow::Result<FingerprintedWheel> {
    let time_cache = debug_span!("cache_wheel", wheel=%wheel.display());
    let _time_cache = time_cache.enter();
    let wheel_file = WheelFile::parse_file_name(
        wheel.file_name().and_then(OsStr::to_str).ok_or_else(|| {
            anyhow!(
                "The wheel at {path} does not have a valid file name.",
                path = wheel.display()
            )
        })?,
    )?;
    let wheel_path = wheel_path(wheel_file.file_name, wheel_options)?;
    let fingerprint_path = wheel_path.with_added_extension(ALGORITHM_NAME);
    if let Some(fingerprint) = atomic_file(&wheel_path, |whl_file| {
        let whl_zip = ZipArchive::new(File::open(wheel)?)?;
        let mut dest = DigestingWriter::new(WheelDigestAlgorithm::new(), whl_file);
        recompress_zipped_whl_to_file(whl_zip, &mut dest, wheel_options)?;
        let (mut whl_file, fingerprint, _) = dest.into_parts();
        whl_file.rewind()?;
        let mut fingerprint_file = tempfile::Builder::new()
            .prefix(wheel_file.file_name)
            .suffix(ALGORITHM_NAME)
            .tempfile_in(
                fingerprint_path
                    .parent()
                    .expect("The wheel and fingerprint are stored in a cache directory."),
            )?;
        fingerprint.store(&mut fingerprint_file)?;
        fingerprint_file.persist(&fingerprint_path)?;
        Ok(fingerprint)
    })? {
        Ok(FingerprintedWheel {
            path: wheel_path,
            file_name: wheel_file.file_name.to_string(),
            fingerprint,
        })
    } else {
        let mut fingerprint_file = File::open(fingerprint_path)?;
        let fingerprint =
            Fingerprint::load(&mut fingerprint_file, WheelDigestAlgorithm::output_size())?;
        Ok(FingerprintedWheel {
            path: wheel_path,
            file_name: wheel_file.file_name.to_string(),
            fingerprint,
        })
    }
}

fn pack_wheel(
    wheel: &InstalledWheel,
    install_paths: &InstallPaths,
    wheel_options: &WheelOptions,
    venv_builder: &impl VenvBuilder,
) -> anyhow::Result<FingerprintedWheel> {
    let wheel_file = wheel.file_name()?;
    let time_pack = debug_span!("pack_wheel", wheel_file = wheel_file);
    let _time_pack = time_pack.enter();
    if let Some(project_dir) = wheel.editable() {
        let project_dir = ProjectDir::new(project_dir)?;
        let dest_dir = tempfile::tempdir()?;
        let built_editable = build_wheel(
            project_dir,
            dest_dir.path(),
            venv_builder,
            &mut Scripts::Embedded,
        )?;
        return cache_wheel(&built_editable, wheel_options);
    }

    let wheel_path = wheel_path(&wheel_file, wheel_options)?;
    let fingerprint_path = wheel_path.with_added_extension(ALGORITHM_NAME);
    if let Some(fingerprint) = atomic_file(&wheel_path, |whl_file| {
        let mut digester = DigestingWriter::new(WheelDigestAlgorithm::new(), whl_file);
        wheel.pack(install_paths, wheel_options, &mut digester)?;
        let fingerprint = digester.into_fingerprint();
        let mut fingerprint_file = tempfile::Builder::new()
            .prefix(&wheel_file)
            .suffix(ALGORITHM_NAME)
            .tempfile_in(
                fingerprint_path
                    .parent()
                    .expect("The wheel and fingerprint are stored in a cache directory."),
            )?;
        fingerprint.store(&mut fingerprint_file)?;
        fingerprint_file.persist(&fingerprint_path)?;
        Ok(fingerprint)
    })? {
        Ok(FingerprintedWheel {
            path: wheel_path,
            file_name: wheel_file,
            fingerprint,
        })
    } else {
        let mut fingerprint_file = File::open(fingerprint_path)?;
        let fingerprint =
            Fingerprint::load(&mut fingerprint_file, WheelDigestAlgorithm::output_size())?;
        Ok(FingerprintedWheel {
            path: wheel_path,
            file_name: wheel_file,
            fingerprint,
        })
    }
}

type VenvWheelRepository = (IndexMap<String, InstalledWheel>, VenvRepository);

fn inventory_venv<'a>(
    venv: &'a Virtualenv<'a>,
) -> anyhow::Result<(&'a Virtualenv<'a>, VenvWheelRepository)> {
    let installed_wheels = collect_installed_wheels(venv)?
        .into_iter()
        .map(|installed_wheel| {
            installed_wheel
                .file_name()
                .map(|file_name| (file_name, installed_wheel))
        })
        .collect::<anyhow::Result<IndexMap<_, _>>>()?;
    let repository = VenvRepository(
        venv.interpreter
            .details
            .prefix
            .join(venv.site_packages_relpath.as_ref()),
    );
    Ok((venv, (installed_wheels, repository)))
}

fn resolve_wheels_from_files<'a>(
    wheel_files: &'a [impl AsRef<Path>],
    platforms: &'a [Platform<'a>],
    requirements: &[Requirement<Url>],
    dependency_configuration: &DependencyConfiguration,
    ignore_errors: bool,
) -> anyhow::Result<Vec<&'a Path>> {
    if platforms.is_empty() {
        if !requirements.is_empty() {
            struct Requirements<'a>(&'a [Requirement<Url>]);
            impl<'a> Display for Requirements<'a> {
                fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                    write!(f, "You must specify at least one `--target` to resolve ")?;
                    match &self.0 {
                        &[requirement] => write!(f, "requirement: {}", requirement)?,
                        _ => {
                            writeln!(f, "requirements:")?;
                            for (index, requirement) in self.0.iter().enumerate() {
                                if index + 1 == self.0.len() {
                                    write!(f, "  {requirement}")?;
                                } else {
                                    writeln!(f, "  {requirement}")?;
                                }
                            }
                        }
                    }
                    Ok(())
                }
            }
            bail!("{}", Requirements(requirements))
        }
        return Ok(wheel_files.iter().map(AsRef::as_ref).collect());
    }

    let file_names = file_names(wheel_files.iter().map(AsRef::as_ref))?;
    let mut wheel_paths_by_file_name: IndexMap<&str, &Path> =
        IndexMap::with_capacity(wheel_files.len());
    for (file_name, path) in file_names.iter().zip(wheel_files) {
        wheel_paths_by_file_name.insert(file_name, path.as_ref());
    }
    let mut wheel_repository = Wheels::new(wheel_paths_by_file_name);
    let mut resolved_file_names: IndexSet<&str> = IndexSet::with_capacity(file_names.len());
    for platform in platforms {
        let resolved_wheels = match platform {
            Platform::Details(platform) => {
                let wheel_files = file_names
                    .iter()
                    .map(|file_name| WheelFile::parse_file_name(file_name))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                resolve_wheels(
                    "specified set of wheels",
                    platform,
                    requirements,
                    wheel_files,
                    &mut wheel_repository,
                    dependency_configuration,
                    None,
                    ignore_errors,
                )?
            }
            Platform::Interpreter(interpreter) => {
                let wheel_files = file_names
                    .iter()
                    .map(|file_name| WheelFile::parse_file_name(file_name))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                resolve_wheels(
                    "specified set of wheels",
                    interpreter.as_ref(),
                    requirements,
                    wheel_files,
                    &mut wheel_repository,
                    dependency_configuration,
                    None,
                    ignore_errors,
                )?
            }
        };
        resolved_file_names.extend(resolved_wheels.keys());
    }
    let wheel_paths = wheel_repository.select(resolved_file_names.into_iter())?;
    Ok(wheel_paths)
}

#[allow(clippy::too_many_arguments)]
#[instrument(level = "debug", skip_all)]
fn build_pex(
    preferred_python: Option<&Interpreter>,
    search_path: Option<SearchPath>,
    preferred_platform: Option<&Platform>,
    wheels: Vec<FingerprintedWheel>,
    pex_info: &mut RawPexInfo,
    packed: bool,
    shebang: Shebang,
    output: Option<PathBuf>,
    extra_args: Vec<String>,
    entry_point: Option<PexEntryPoint>,
    sources: Sources,
) -> anyhow::Result<()> {
    match output {
        Some(path) => {
            let subject = Cow::Owned(format!("PEX at {path}", path = path.display()));
            create_pex(
                subject,
                false,
                preferred_platform,
                wheels,
                pex_info,
                packed,
                shebang,
                &path,
                entry_point,
                sources,
            )
        }
        None => {
            let subject = Cow::Borrowed("ephemeral PEX");
            let (python_args, args) = if pex_info.has_entry_point() {
                (vec![], extra_args)
            } else {
                (extra_args, vec![])
            };
            if packed {
                let chroot = tempfile::tempdir()?;
                let path = chroot.path();
                create_pex(
                    subject,
                    true,
                    preferred_platform,
                    wheels,
                    pex_info,
                    packed,
                    shebang,
                    path,
                    entry_point,
                    sources,
                )?;
                execute_pex(
                    preferred_python,
                    search_path,
                    python_args,
                    path,
                    args,
                    pex_info.configured_cache_root(),
                )
            } else {
                let pex = NamedTempFile::new()?;
                let path = pex.path();
                create_pex(
                    subject,
                    true,
                    preferred_platform,
                    wheels,
                    pex_info,
                    packed,
                    shebang,
                    path,
                    entry_point,
                    sources,
                )?;
                execute_pex(
                    preferred_python,
                    search_path,
                    python_args,
                    path,
                    args,
                    pex_info.configured_cache_root(),
                )
            }
        }
    }
}

fn file_names<'a>(paths: impl ExactSizeIterator<Item = &'a Path>) -> anyhow::Result<Vec<&'a str>> {
    let mut file_names = Vec::with_capacity(paths.len());
    for path in paths {
        let file_name = path
            .file_name()
            .and_then(|file_name| file_name.to_str())
            .ok_or_else(|| {
                anyhow!(
                    "Invalid path {path}: file name is not UTF-8.",
                    path = path.display()
                )
            })?;
        file_names.push(file_name);
    }
    Ok(file_names)
}

struct Wheels<'a> {
    wheel_files: IndexMap<&'a str, &'a Path>,
    wheel_zips: IndexMap<String, ZipArchive<File>>,
}

impl<'a> Wheels<'a> {
    fn new(wheel_files: IndexMap<&'a str, &'a Path>) -> Self {
        let wheel_zips = IndexMap::with_capacity(wheel_files.len());
        Self {
            wheel_files,
            wheel_zips,
        }
    }

    fn select(
        mut self,
        file_names: impl ExactSizeIterator<Item = &'a str>,
    ) -> anyhow::Result<Vec<&'a Path>> {
        let mut paths = Vec::with_capacity(file_names.len());
        for file_name in file_names {
            paths.push(
                self.wheel_files
                    .shift_remove(file_name)
                    .ok_or_else(|| anyhow!("The collected wheels do not include {file_name}."))?,
            )
        }
        Ok(paths)
    }
}

impl<'a> MetadataReader for Wheels<'a> {
    fn locate_dirs(&mut self, wheel_file: &WheelFile) -> anyhow::Result<MetadataDirs> {
        if let Some(path) = self.wheel_files.get(wheel_file.file_name) {
            let wheel_zip = ZipArchive::new(File::open(path)?)?;
            let metadata_dirs = MetadataDirs::locate_in_zip(
                &wheel_zip,
                path.display(),
                None,
                &wheel_file.project_name,
                &wheel_file.version,
            )?;
            self.wheel_zips
                .insert(wheel_file.file_name.to_string(), wheel_zip);
            Ok(metadata_dirs)
        } else {
            bail!(
                "The collected wheels do not include {file_name}.",
                file_name = wheel_file.file_name
            )
        }
    }

    fn read(
        &mut self,
        metadata_dirs: &MetadataDirs,
        wheel_file: &WheelFile,
        file_name: &str,
    ) -> anyhow::Result<String> {
        let zip = self
            .wheel_zips
            .get_mut(wheel_file.file_name)
            .ok_or_else(|| {
                anyhow!(
                    "The collected wheels do not include {file_name}.",
                    file_name = wheel_file.file_name
                )
            })?;
        let dist_info_dir = metadata_dirs.dist_info_dir();
        Ok(io::read_to_string(
            zip.by_name_ex(&format!("{dist_info_dir}/{file_name}"))?,
        )?)
    }
}

#[allow(clippy::too_many_arguments)]
fn create_pex(
    subject: Cow<'_, str>,
    ephemeral: bool,
    preferred_python: Option<&Platform>,
    wheels: Vec<FingerprintedWheel>,
    pex_info: &mut RawPexInfo,
    packed: bool,
    shebang: Shebang,
    path: &Path,
    entry_point: Option<PexEntryPoint>,
    sources: Sources,
) -> anyhow::Result<()> {
    let wheel_files = file_names(wheels.iter().map(|wheel| wheel.path.as_path()))?
        .into_iter()
        .map(WheelFile::parse_file_name)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let required_targets = RequiredTargets::for_wheel_files(subject, wheel_files.iter())?;
    let mut targets = required_targets.unique_targets();
    if targets.is_empty() {
        targets |= if ephemeral {
            let current_target = SimplifiedTarget::current()?;
            enum_set!(current_target)
        } else {
            let all_targets = SimplifiedTarget::all();
            if *AVAILABLE_TARGETS != all_targets {
                warn!(
                    "The {subject} has no platform specific wheels but this pexrc binary only has support \
                    for the following platforms:\n\
                    {available_targets}\n\
                    \n\
                    The {subject} will not run on the following platforms:\n\
                    {missing_targets}\n\
                    \n\
                    If the {subject} needs to run on the missing platforms, use a pexrc binary built with \
                    support for all platforms.\n\
                    One place to find these is in the official releases here:\n\
                    https://github.com/pex-tool/pex.rc/releases/tag/v{VERSION}",
                    subject = required_targets.subject,
                    available_targets = *AVAILABLE_TARGETS,
                    missing_targets = all_targets - *AVAILABLE_TARGETS
                );
            }
            *AVAILABLE_TARGETS
        }
    }

    let clibs = targets
        .iter()
        .map(|target| {
            CLIB_BY_TARGET
                .get(&target)
                .expect("The allowed --target values are all keys in CLIB_BY_TARGET.")
        })
        .collect::<Vec<_>>();
    let proxies = targets
        .iter()
        .map(|target| {
            PROXY_BY_TARGET
                .get(&target)
                .expect("The allowed --target values are all keys in PROXY_BY_TARGET.")
        })
        .chain(
            targets
                .iter()
                .filter_map(|target| PROXYW_BY_TARGET.get(&target)),
        )
        .collect::<Vec<_>>();
    if packed {
        create_packed_pex(
            preferred_python,
            wheels,
            pex_info,
            clibs,
            proxies,
            shebang,
            path,
            entry_point,
            sources,
        )
    } else {
        create_zipapp(
            preferred_python,
            wheels,
            pex_info,
            clibs,
            proxies,
            shebang,
            path,
            entry_point,
            sources,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn create_packed_pex(
    preferred_python: Option<&Platform>,
    wheels: Vec<FingerprintedWheel>,
    pex_info: &mut RawPexInfo,
    clibs: Vec<&Binary>,
    proxies: Vec<&Binary>,
    shebang: Shebang,
    path: &Path,
    entry_point: Option<PexEntryPoint>,
    sources: Sources,
) -> anyhow::Result<()> {
    let mut dest_dir = if let Some(parent_dir) = path.parent() {
        tempfile::tempdir_in(parent_dir)
    } else {
        tempfile::tempdir()
    }?;

    adjust_requirements(pex_info, &wheels)?;
    let mut code_hash = Key::new();
    sources.copy(dest_dir.path(), &mut code_hash)?;
    if let Some(entry_point) = entry_point
        && let Some(exe) = entry_point.resolve(pex_info, &wheels)?
    {
        exe.write(dest_dir.path(), &mut code_hash)?;
    }
    pex_info.code_hash = Cow::Owned(code_hash.fingerprint().hex_digest());

    let deps_dir = dest_dir.path().join(DEPS_DIR);
    fs::create_dir(&deps_dir)?;
    for wheel in wheels {
        platform::link_or_copy(wheel.path, deps_dir.join(&wheel.file_name))?;
        pex_info.distributions.insert(
            Cow::Owned(wheel.file_name),
            Cow::Owned(wheel.fingerprint.hex_digest()),
        );
    }
    pex_info.deps_are_wheel_files = true;

    Scripts::Embedded.write(dest_dir.path())?;

    let pex_dir = dest_dir.path().join("__pex__");
    let clibs_dir = pex_dir.join(".clibs");
    fs::create_dir_all(&clibs_dir)?;
    for clib in clibs {
        clib.embed_in_dir(&clibs_dir, false)?;
    }
    let proxies_dir = pex_dir.join(".proxies");
    fs::create_dir(&proxies_dir)?;
    for proxy in proxies {
        proxy.embed_in_dir(&proxies_dir, true)?;
    }

    pex_info.finalize_pex_hash()?;
    let mut pex_info_fp = File::create_new(dest_dir.path().join(PEX_INFO_FILE))?;
    pex_info.write(&mut pex_info_fp)?;

    let mut shebang_buffer = sh_boot_buffer();
    write_shebang(preferred_python, pex_info, shebang, &mut shebang_buffer)?;
    let shebang = String::from_utf8(shebang_buffer)?;
    write_boot(pex_info, dest_dir.path(), shebang.as_ref())?;

    if path.is_dir() {
        fs::remove_dir_all(path)?;
    } else if path.is_file() {
        fs::remove_file(path)?;
    }
    fs::rename(&dest_dir, path)?;
    dest_dir.disable_cleanup(true);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn create_zipapp(
    preferred_python: Option<&Platform>,
    wheels: Vec<FingerprintedWheel>,
    pex_info: &mut RawPexInfo,
    clibs: Vec<&Binary>,
    proxies: Vec<&Binary>,
    shebang: Shebang,
    path: &Path,
    entry_point: Option<PexEntryPoint>,
    sources: Sources,
) -> anyhow::Result<()> {
    let mut dst_zip_fp = if let Some(parent_dir) = path.parent() {
        NamedTempFile::new_in(parent_dir)?
    } else {
        NamedTempFile::new()?
    };
    write_shebang(preferred_python, pex_info, shebang, &mut dst_zip_fp)?;

    let mut dst_zip = ZipWriter::new(&dst_zip_fp);

    let directory_options = SimpleFileOptions::DEFAULT;
    let deflated_file_options =
        SimpleFileOptions::DEFAULT.compression_method(CompressionMethod::Deflated);
    let stored_file_options =
        SimpleFileOptions::DEFAULT.compression_method(CompressionMethod::Stored);

    adjust_requirements(pex_info, &wheels)?;
    let mut code_hash = Key::new();
    sources.inject(&mut dst_zip, deflated_file_options, &mut code_hash)?;
    if let Some(entry_point) = entry_point
        && let Some(exe) = entry_point.resolve(pex_info, &wheels)?
    {
        exe.inject(&mut dst_zip, deflated_file_options, &mut code_hash)?;
    }
    pex_info.code_hash = Cow::Owned(code_hash.fingerprint().hex_digest());

    if !wheels.is_empty() {
        dst_zip.add_directory(DEPS_ZIP_DIR, directory_options)?;
        for wheel in wheels {
            dst_zip.start_file(
                format!("{DEPS_DIR}/{}", wheel.file_name),
                stored_file_options,
            )?;
            let mut src = File::open(wheel.path)?;
            io::copy(&mut src, &mut dst_zip)?;
            pex_info.distributions.insert(
                Cow::Owned(wheel.file_name),
                Cow::Owned(wheel.fingerprint.hex_digest()),
            );
        }
    }
    pex_info.deps_are_wheel_files = true;

    dst_zip.add_directory("__pex__", directory_options)?;
    Scripts::Embedded.inject(&mut dst_zip, deflated_file_options)?;

    dst_zip.add_directory("__pex__/.proxies", directory_options)?;
    for proxy in proxies {
        proxy.embed_in_zip(&mut dst_zip, "__pex__/.proxies", deflated_file_options)?;
    }

    dst_zip.add_directory("__pex__/.clibs", directory_options)?;
    for clib in clibs {
        clib.embed_in_zip(&mut dst_zip, "__pex__/.clibs", deflated_file_options)?;
    }

    inject_boot(pex_info, &mut dst_zip, deflated_file_options)?;

    pex_info.finalize_pex_hash()?;
    dst_zip.start_file("PEX-INFO", deflated_file_options)?;
    pex_info.write(&mut dst_zip)?;

    dst_zip.finish()?;
    mark_executable(dst_zip_fp.as_file_mut())?;

    if path.is_dir() {
        fs::remove_dir_all(path)?;
    }
    dst_zip_fp.persist(path)?;

    Ok(())
}

fn write_shebang(
    preferred_platform: Option<&Platform>,
    pex_info: &mut RawPexInfo,
    shebang: Shebang,
    sink: &mut impl Write,
) -> anyhow::Result<()> {
    match shebang {
        Shebang::Custom(shebang) => {
            writeln!(
                sink,
                "#!{shebang}",
                shebang = shebang.trim_prefix("#!").trim_suffix('\n')
            )?;
            Ok(())
        }
        Shebang::EnvCompatible => {
            if let Some(preferred_platform) = preferred_platform {
                match preferred_platform.implementation()? {
                    PythonImplementation::CPython(python) => writeln!(
                        sink,
                        "#!/usr/bin/env python{major}.{minor}",
                        major = python.major,
                        minor = python.minor
                    )?,
                    PythonImplementation::PyPy(pypy) => writeln!(
                        sink,
                        "#!/usr/bin/env pypy{major}.{minor}",
                        major = pypy.major,
                        minor = pypy.minor
                    )?,
                };
            } else {
                sink.write_all(b"#!/usr/bin/env python\n")?;
            }
            Ok(())
        }
        Shebang::ShBoot => {
            let preferred_python = if let Some(preferred) = preferred_platform {
                Some(preferred.implementation()?)
            } else {
                None
            };
            write_sh_boot_shebang("<subject>", pex_info, preferred_python, sink)
        }
    }
}

fn execute_pex(
    preferred_python: Option<&Interpreter>,
    search_path: Option<SearchPath>,
    python_args: Vec<String>,
    pex: &Path,
    args: Vec<String>,
    custom_pex_root: Option<Cow<Path>>,
) -> anyhow::Result<()> {
    let preferred_python = preferred_python.map(|interpreter| interpreter.realpath.as_path());
    if let Some(custom_root) = custom_pex_root.as_deref()
        && let Ok(root) = CacheDir::root()
        && custom_root != root.as_ref()
    {
        warn!(
            "The `--runtime-pex-root {custom_root}` will not be used for this ephemeral PEX run.",
            custom_root = custom_root.display(),
        );
        warn!("The cache for builds is at {root}.", root = root.display())
    }
    let exit_code = pexrs::boot(
        preferred_python,
        python_args,
        pex,
        args,
        Some([(
            "__PEX_EPHEMERAL__",
            env::join_paths([
                pex.as_os_str(),
                &env::args_os().next().expect("There is always an argv0"),
            ])?,
        )]),
        search_path,
        false,
        false,
        false,
    )?;
    process::exit(exit_code)
}
