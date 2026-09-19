// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::{Display, Formatter};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::time::SystemTime;

use base64::Engine;
use base64::display::Base64Display;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use digest::Digest;
use fs_err::File;
use logging_timer::time;
use sha2::Sha256;

pub fn default_digest() -> impl Digest {
    Sha256::new()
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Fingerprint(Vec<u8>);

impl Fingerprint {
    pub fn new<D: Digest>(digest: D) -> Self {
        Self(Vec::from(digest.finalize().as_slice()))
    }

    #[time("trace", "Fingerprint.{}")]
    pub fn base64_digest(&self) -> String {
        URL_SAFE_NO_PAD.encode(&self.0)
    }

    #[time("trace", "Fingerprint.{}")]
    pub fn hex_digest(&self) -> String {
        hex::encode(&self.0)
    }
}

impl Display for Fingerprint {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Base64Display::new(&self.0, &URL_SAFE_NO_PAD).fmt(f)
    }
}

impl<R: Read> TryFrom<BufReader<R>> for Fingerprint {
    type Error = anyhow::Error;

    fn try_from(value: BufReader<R>) -> anyhow::Result<Self> {
        let mut digest = default_digest();
        digest_reader(value, &mut digest)?;
        Ok(Self::new(digest))
    }
}

pub struct DigestingReader<D: Digest, R: Read> {
    digest: D,
    reader: R,
    size: u64,
}

impl<D: Digest, R: Read> DigestingReader<D, R> {
    pub fn new(digest: D, reader: R) -> Self {
        Self {
            digest,
            reader,
            size: 0,
        }
    }

    pub fn into_fingerprint(self) -> Fingerprint {
        Fingerprint::new(self.digest)
    }

    pub fn into_fingerprint_and_size(self) -> (Fingerprint, u64) {
        (Fingerprint::new(self.digest), self.size)
    }
}

impl<D: Digest, R: Read> Read for DigestingReader<D, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let amount = self.reader.read(buf)?;
        self.digest.update(&buf[0..amount]);
        self.size += amount as u64;
        Ok(amount)
    }
}

#[derive(Default)]
pub struct HashOptions {
    path: bool,
    mtime: bool,
    size: bool,
    contents: bool,
}

impl HashOptions {
    pub const fn new() -> Self {
        Self {
            path: false,
            mtime: false,
            size: false,
            contents: false,
        }
    }

    pub const fn path(mut self, path: bool) -> Self {
        self.path = path;
        self
    }

    pub const fn mtime(mut self, mtime: bool) -> Self {
        self.mtime = mtime;
        self
    }

    pub const fn size(mut self, size: bool) -> Self {
        self.size = size;
        self
    }

    pub const fn contents(mut self, contents: bool) -> Self {
        self.contents = contents;
        self
    }
}

#[time("debug", "fingerprint.{}")]
pub fn hash_file(path: &Path, options: &HashOptions) -> anyhow::Result<Fingerprint> {
    let mut digest = default_digest();
    digest_file(path, options, &mut digest)?;
    Ok(Fingerprint::new(digest))
}

pub(crate) fn digest_file<D>(
    path: &Path,
    options: &HashOptions,
    digest: &mut D,
) -> anyhow::Result<()>
where
    D: Digest,
{
    if options.path {
        digest.update(b"path:");
        digest.update(path.as_os_str().as_encoded_bytes());
    }
    if options.mtime || options.size {
        let metadata = path.metadata()?;
        if options.mtime {
            digest.update(b"mtime:");
            digest.update(
                metadata
                    .modified()?
                    .duration_since(SystemTime::UNIX_EPOCH)?
                    .as_nanos()
                    .to_ne_bytes(),
            )
        }
        if options.size {
            digest.update(b"size:");
            digest.update(metadata.len().to_ne_bytes())
        }
    }
    if options.contents {
        digest.update(b"contents:");
        digest_path(path, digest)?;
    }
    Ok(())
}

pub fn fingerprint_file<D>(path: &Path, mut digest: D) -> anyhow::Result<(usize, Fingerprint)>
where
    D: Digest,
{
    let size = digest_path(path, &mut digest)?;
    Ok((size, Fingerprint::new(digest)))
}

fn digest_path<D>(path: &Path, digest: &mut D) -> anyhow::Result<usize>
where
    D: Digest,
{
    digest_reader(BufReader::new(File::open(path)?), digest)
}

fn digest_reader<D>(mut reader: BufReader<impl Read>, digest: &mut D) -> anyhow::Result<usize>
where
    D: Digest,
{
    let mut size: usize = 0;
    loop {
        let amount_read = {
            let buf = reader.fill_buf()?;
            if buf.is_empty() {
                return Ok(size);
            }
            digest.update(buf);
            buf.len()
        };
        size += amount_read;
        reader.consume(amount_read);
    }
}
