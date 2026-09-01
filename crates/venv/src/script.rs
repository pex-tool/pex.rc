// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::Path;
use std::sync::LazyLock;

use either::Either;
use pelite::image::IMAGE_SUBSYSTEM_WINDOWS_GUI;
use pelite::{PeFile, Wrap};
use python_platform::PythonVersion;
use regex::Regex;
use zip::ZipArchive;
use zip_ext::ZipArchiveExt;

// See: https://peps.python.org/pep-0263/
static PEP_263_CODING_LINE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[ \t\f]*#.*?coding[:=][ \t]*([-_.a-zA-Z0-9]+)")
        .expect("This is a known good regex.")
});
static PIP_AND_UV_BIN_SH_RE_DIRECTOR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^'''exec' .*(python|pypy).*").expect("This is a known good regex.")
});

#[cfg_attr(test, derive(Debug, Eq, PartialEq))]
pub(crate) struct Shebang {
    pub(crate) end: usize,
    pub(crate) extra_content: Option<Range<usize>>,
}

#[cfg_attr(test, derive(Debug, Eq, PartialEq))]
pub(crate) struct PythonScript {
    pub(crate) is_windowed: bool,
    pub(crate) shebang: Option<Shebang>,
}

impl PythonScript {
    pub(crate) fn detect(
        contents: &mut (impl Read + Seek),
        size: u64,
        python_version: PythonVersion,
    ) -> anyhow::Result<Option<Self>> {
        match Self::detect_posix(contents, python_version)? {
            Either::Left(python_script) => Ok(Some(python_script)),
            Either::Right(contents) => Self::detect_windows(contents, size),
        }
    }

    fn detect_posix(
        contents: &mut (impl Read + Seek),
        python_version: PythonVersion,
    ) -> anyhow::Result<Either<Self, &mut (impl Read + Seek)>> {
        let mut buf_read = BufReader::new(contents);
        let mut magic_buf: [u8; 2] = [0; 2];
        buf_read.read_exact(&mut magic_buf)?;
        if &magic_buf != b"#!" {
            return Ok(Either::Right(buf_read.into_inner()));
        }
        let mut shebang = Vec::new();
        buf_read.read_until(b'\n', &mut shebang)?;
        shebang.make_ascii_lowercase();
        let interpreter =
            if let Some((interpreter, _)) = shebang.split_once(|x| x.is_ascii_whitespace()) {
                interpreter
            } else {
                shebang.as_slice()
            };
        let shebang_end = 2 /* #! */ + interpreter.len();

        let mut extra_shebang_content: Option<Range<usize>> = None;
        let mut is_windowed = false;
        let mut is_python = interpreter == b"python";
        if !is_python {
            is_windowed = interpreter == b"pythonw";
            is_python = is_windowed;
        }
        if !is_python && interpreter == b"/bin/sh" {
            let mut line_buffer = String::new();
            let mut start = shebang_end;
            let mut amount_read = buf_read.read_line(&mut line_buffer)?;
            if PEP_263_CODING_LINE_RE.is_match(&line_buffer) {
                start += amount_read;
                line_buffer.clear();
                amount_read = buf_read.read_line(&mut line_buffer)?;
            }
            if line_buffer != "'''': pshprs\n"
                && !PIP_AND_UV_BIN_SH_RE_DIRECTOR_RE.is_match(&line_buffer)
            {
                return Ok(Either::Right(buf_read.into_inner()));
            }
            is_python = true;
            let mut end = start + amount_read;
            loop {
                line_buffer.clear();
                end += buf_read.read_line(&mut line_buffer)?;
                if line_buffer == "'''\n" {
                    break;
                }
            }
            extra_shebang_content = Some(start..end);
        }
        if !is_python {
            is_python = interpreter.ends_with(b"/python") || interpreter.ends_with(b"pypy");
        }
        if !is_python {
            is_python = interpreter
                .ends_with(format!("/python{major}", major = python_version.major).as_bytes())
                || interpreter
                    .ends_with(format!("/pypy{major}", major = python_version.major).as_bytes());
        }
        if !is_python {
            is_python = interpreter.ends_with(
                format!(
                    "/python{major}.{minor}",
                    major = python_version.major,
                    minor = python_version.minor
                )
                .as_bytes(),
            ) || interpreter.ends_with(
                format!(
                    "/pypy{major}.{minor}",
                    major = python_version.major,
                    minor = python_version.minor
                )
                .as_bytes(),
            );
        }

        if !is_python {
            Ok(Either::Right(buf_read.into_inner()))
        } else {
            Ok(Either::Left(Self {
                is_windowed,
                shebang: Some(Shebang {
                    end: shebang_end,
                    extra_content: extra_shebang_content,
                }),
            }))
        }
    }

    fn detect_windows(
        contents: &mut (impl Read + Seek),
        size: u64,
    ) -> anyhow::Result<Option<Self>> {
        if let Ok(mut zip) = ZipArchive::new(contents)
            && zip.by_name_ex("__main__.py").is_ok()
        {
            let zip_offset = zip.offset();
            let file = zip.into_inner();
            let pe_contents_len = size - zip_offset;
            let mut contents = Vec::with_capacity(pe_contents_len as usize);
            let mut pe_contents = file.take(pe_contents_len);
            pe_contents.read_to_end(&mut contents)?;
            if let Ok(pe_file) = PeFile::from_bytes(&contents) {
                let _src = pe_contents.into_inner();
                let is_windowed = IMAGE_SUBSYSTEM_WINDOWS_GUI
                    == match pe_file.optional_header() {
                        Wrap::T32(header) => header.Subsystem,
                        Wrap::T64(header) => header.Subsystem,
                    };
                return Ok(Some(Self {
                    is_windowed,
                    shebang: None,
                }));
            }
        }
        Ok(None)
    }

    pub(crate) fn re_write(
        &self,
        contents: &mut (impl Read + Seek),
        sink: &mut impl Write,
    ) -> anyhow::Result<()> {
        sink.write_all(if self.is_windowed {
            b"#!pythonw"
        } else {
            b"#!python"
        })?;
        if let Some(shebang) = self.shebang.as_ref() {
            let mut source = BufReader::new(contents);
            source.seek(SeekFrom::Start(shebang.end as u64))?;
            if let Some(skip) = shebang.extra_content.as_ref() {
                let coding_line_count = skip.start - shebang.end;
                if coding_line_count > 0 {
                    let mut coding_line_content = source.take(coding_line_count as u64);
                    io::copy(&mut coding_line_content, sink)?;
                    source = coding_line_content.into_inner()
                }
                source.seek(SeekFrom::Start(skip.end as u64))?;
            }
            io::copy(&mut source, sink)?;
        } else {
            let mut zip = ZipArchive::new(contents)?;
            let mut main_py = zip.by_name_ex("__main__.py")?;
            io::copy(&mut main_py, sink)?;
        }
        Ok(())
    }

    pub(crate) fn reified_contents(
        &self,
        shebang_interpreter: &Path,
        contents: &mut (impl Read + Seek),
    ) -> anyhow::Result<Vec<u8>> {
        let mut reified_contents = Vec::new();
        reified_contents.extend_from_slice(b"#!");
        reified_contents.extend_from_slice(shebang_interpreter.as_os_str().as_encoded_bytes());
        reified_contents.push(b'\n');
        if let Some(shebang) = self.shebang.as_ref() {
            let mut source = BufReader::new(contents);
            source.seek(SeekFrom::Start(shebang.end as u64))?;
            if let Some(skip) = shebang.extra_content.as_ref() {
                let coding_line_count = skip.start - shebang.end;
                if coding_line_count > 0 {
                    let mut coding_line_content = source.take(coding_line_count as u64);
                    io::copy(&mut coding_line_content, &mut reified_contents)?;
                    source = coding_line_content.into_inner()
                }
                source.seek(SeekFrom::Start(skip.end as u64))?;
            }
            io::copy(&mut source, &mut reified_contents)?;
        } else {
            let mut zip = ZipArchive::new(contents)?;
            let mut main_py = zip.by_name_ex("__main__.py")?;
            io::copy(&mut main_py, &mut reified_contents)?;
        }
        Ok(reified_contents)
    }
}
