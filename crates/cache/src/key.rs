// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use digest::Digest;
use sha2::Sha256;

use crate::fingerprint::digest_file;
use crate::{Fingerprint, HashOptions};

pub struct Key<D: Digest = Sha256> {
    digest: D,
}

struct DigestingReader<'a, D: Digest, R: Read> {
    digest: &'a mut D,
    reader: R,
    size: u64,
}

impl<'a, D: Digest, R: Read> DigestingReader<'a, D, R> {
    fn new(digest: &'a mut D, reader: R) -> Self {
        Self {
            digest,
            reader,
            size: 0,
        }
    }
}

impl<'a, D: Digest, R: Read> Read for DigestingReader<'a, D, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let amount = self.reader.read(buf)?;
        self.digest.update(&buf[0..amount]);
        self.size += amount as u64;
        Ok(amount)
    }
}

impl<D: Digest> Key<D> {
    pub fn new() -> Self {
        Self { digest: D::new() }
    }

    pub fn file(
        &mut self,
        path: impl AsRef<Path>,
        options: &HashOptions,
        prefix: Option<&Path>,
    ) -> anyhow::Result<&mut Self> {
        self.digest.update(b"file");
        digest_file(path.as_ref(), options, &mut self.digest, prefix)?;
        Ok(self)
    }

    pub fn file_stream(
        &mut self,
        path: impl AsRef<Path>,
        input: &mut impl Read,
        output: &mut impl Write,
    ) -> anyhow::Result<&mut Self> {
        self.digest.update(b"file");
        self.digest.update(b"path:");
        self.digest
            .update(path.as_ref().as_os_str().as_encoded_bytes());
        self.digest.update(b"contents:");
        let mut input = DigestingReader::new(&mut self.digest, input);
        io::copy(&mut input, output)?;
        Ok(self)
    }

    pub fn file_contents(&mut self, name: &str, contents: &[u8]) -> anyhow::Result<&mut Self> {
        self.digest.update(b"file");
        self.digest.update(b"path:");
        self.digest.update(name.as_bytes());
        self.digest.update(b"contents:");
        self.digest.update(contents);
        Ok(self)
    }

    pub fn property(&mut self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> &mut Self {
        self.digest.update(b"property");
        self.digest.update(key.as_ref());
        self.digest.update(value.as_ref());
        self
    }

    pub fn list<V: AsRef<[u8]>>(
        &mut self,
        key: impl AsRef<[u8]>,
        values: impl ExactSizeIterator<Item = V>,
    ) -> &mut Self {
        self.digest.update(b"list");
        self.digest.update(key.as_ref());
        self.digest.update("len");
        self.digest.update(values.len().to_ne_bytes());
        for value in values {
            self.digest.update(value.as_ref());
        }
        self
    }

    pub fn object(
        &mut self,
        key: impl AsRef<[u8]>,
        object: impl Iterator<Item = (impl AsRef<[u8]>, impl AsRef<[u8]>)>,
    ) -> &mut Self {
        self.digest.update(b"object");
        self.digest.update(key.as_ref());
        for (name, value) in object {
            self.digest.update(name.as_ref());
            self.digest.update(value.as_ref());
        }
        self
    }

    pub fn data(&mut self, value: impl AsRef<[u8]>) -> &mut Self {
        self.digest.update(value);
        self
    }

    pub fn fingerprint(self) -> Fingerprint {
        Fingerprint::new(self.digest)
    }
}

impl Default for Key {
    fn default() -> Key<Sha256> {
        Key::<Sha256>::new()
    }
}

impl<D: Digest> From<Key<D>> for PathBuf {
    fn from(value: Key<D>) -> Self {
        PathBuf::from(value.fingerprint().base64_digest())
    }
}
