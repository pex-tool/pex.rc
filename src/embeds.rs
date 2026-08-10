// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::io::{Read, Seek, Write};
use std::iter::Iterator;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::anyhow;
use enumset::EnumSet;
use include_dir::{Dir, include_dir};
use indexmap::IndexMap;
use target::SimplifiedTarget;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

#[derive(Eq, PartialEq, Hash)]
pub struct Binary<'a> {
    pub target: SimplifiedTarget,
    pub path: &'a Path,
    pub contents: &'a [u8],
}

impl<'a> Binary<'a> {
    pub fn embed_in_zip(
        &self,
        dst_zip: &mut ZipWriter<impl Write + Seek>,
        dst_dir: &str,
        file_options: SimpleFileOptions,
    ) -> anyhow::Result<()> {
        let dst_path = format!(
            "{dst_dir}/{embed}",
            embed = self
                .path
                .file_name()
                .expect("Embeds have file names.")
                .to_str()
                .expect("Embed file names are utf-8 strings.")
        );
        dst_zip.start_file(dst_path, file_options)?;
        let mut embed_reader = zstd::Decoder::new(self.contents)?;
        io::copy(&mut embed_reader, dst_zip)?;
        Ok(())
    }
}

const EMBEDS_DIR: Dir<'static> = include_dir!("$EMBEDS_DIR");

pub(crate) static CLIBS_DIR: LazyLock<&'static Dir> =
    LazyLock::new(|| EMBEDS_DIR.get_dir("clibs").expect("Embeds include clibs/."));

pub static CLIB_BY_TARGET: LazyLock<IndexMap<SimplifiedTarget, Binary<'static>>> =
    LazyLock::new(|| {
        CLIBS_DIR
            .files()
            .map(|file| {
                let path = file.path();
                let target = path
                    .file_prefix()
                    .expect("The C libraries all have a file name with an extension.")
                    .to_str()
                    .expect("The C library file names are utf-8 strings.");
                let target = SimplifiedTarget::try_from(target)
                    .expect("The C library file names are all derived from simplified targets.");
                (
                    target,
                    Binary {
                        target,
                        path,
                        contents: file.contents(),
                    },
                )
            })
            .collect()
    });

pub static AVAILABLE_TARGETS: LazyLock<EnumSet<SimplifiedTarget>> =
    LazyLock::new(|| CLIB_BY_TARGET.keys().collect());

pub(crate) static PROXIES_DIR: LazyLock<&'static Dir> = LazyLock::new(|| {
    EMBEDS_DIR
        .get_dir("proxies")
        .expect("Embeds include proxies/.")
});

fn identify_proxy_files(name: &str) -> IndexMap<SimplifiedTarget, Binary<'static>> {
    PROXIES_DIR
        .files()
        .filter_map(|file| {
            let path = file.path();
            let mut components = path
                .file_stem()
                .expect("The Python proxies all have a file name.")
                .to_str()
                .expect("The Python proxy file names are utf-8 strings.")
                .splitn(3, "-");
            assert_eq!(Some("python"), components.next());
            if components.next().expect(
                "The Python proxy file names are all of the form `python-proxyw?-<target>(.exe)?",
            ) != name
            {
                return None;
            }
            let target = components.next().expect(
                "The Python proxy file names are all of the form `python-proxyw?-<target>(.exe)?",
            );
            let target = SimplifiedTarget::try_from(target)
                .expect("The Python proxy file names are all derived from simplified targets.");
            Some((
                target,
                Binary {
                    target,
                    path,
                    contents: file.contents(),
                },
            ))
        })
        .collect()
}

pub static PROXY_BY_TARGET: LazyLock<IndexMap<SimplifiedTarget, Binary<'static>>> =
    LazyLock::new(|| identify_proxy_files("proxy"));

pub static PROXYW_BY_TARGET: LazyLock<IndexMap<SimplifiedTarget, Binary<'static>>> =
    LazyLock::new(|| identify_proxy_files("proxyw"));

pub fn read_proxy_content(target: SimplifiedTarget, is_gui: bool) -> anyhow::Result<impl Read> {
    let proxy = if is_gui
        && matches!(
            target,
            SimplifiedTarget::Arm64Windows | SimplifiedTarget::X64Windows
        ) {
        PROXYW_BY_TARGET
            .get(&target)
            .ok_or_else(|| anyhow!("There is no python-proxyw for {target}"))
    } else {
        PROXY_BY_TARGET
            .get(&target)
            .ok_or_else(|| anyhow!("There is no python-proxy for {target}"))
    }?;
    Ok(zstd::Decoder::new(proxy.contents)?)
}
