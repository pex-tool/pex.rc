// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::cmp;

use cache::Fingerprint;
use digest::Digest;
use include_dir::File;
use owo_colors::OwoColorize;
use platform::path_for_terminal_output;
use sha2::Sha256;

use crate::embeds::{CLIBS_DIR, PROXIES_DIR};

fn iter_embeds<'a>() -> impl Iterator<Item = &'a File<'a>> {
    CLIBS_DIR.files().chain(PROXIES_DIR.files())
}

pub fn display() -> anyhow::Result<()> {
    let mut paths = Vec::new();
    let mut max_width = 0;
    for embed in iter_embeds() {
        let path = path_for_terminal_output(embed.path()).to_string();
        max_width = cmp::max(max_width, path.len());
        paths.push(path);
    }
    let count = paths.len();
    anstream::println!(
        "There {are} {count} {embeds}:",
        are = if count == 1 { "is" } else { "are" },
        count = count.yellow(),
        embeds = if count == 1 { "embed" } else { "embeds" }
    );
    for (idx, (embed, path)) in iter_embeds().zip(paths).enumerate() {
        let mut digest = Sha256::new();
        digest.update(embed.contents());
        let fingerprint = Fingerprint::new(digest);
        anstream::println!(
            "{idx:>3}. {path} {pad}{size:<8} bytes {alg}:{fingerprint}",
            idx = (idx + 1).yellow(),
            path = path.blue(),
            pad = " ".repeat(max_width - path.len()),
            size = embed.contents().len().yellow(),
            alg = "sha256-base64".green(),
            fingerprint = fingerprint.base64_digest().green(),
        )
    }
    Ok(())
}
