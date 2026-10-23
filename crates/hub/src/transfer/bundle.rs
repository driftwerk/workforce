use std::collections::BTreeMap;
use std::io::{Read, Seek, Write};

use eyre::{eyre, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::model::{Manifest, MANIFEST_FILE};

fn options() -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(CompressionMethod::Deflated)
}

fn checksum(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Writes a transfer bundle as a zip, accumulating a per-entry content hash
/// that is sealed into `manifest.json` on `finish`.
pub struct BundleWriter<W: Write + Seek> {
    zip: ZipWriter<W>,
    checksums: BTreeMap<String, String>,
}

impl<W: Write + Seek> BundleWriter<W> {
    pub fn new(writer: W) -> Self {
        Self {
            zip: ZipWriter::new(writer),
            checksums: BTreeMap::new(),
        }
    }

    pub fn add_bytes(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        self.zip.start_file(name, options())?;
        self.zip.write_all(bytes)?;
        self.checksums.insert(name.to_string(), checksum(bytes));
        Ok(())
    }

    pub fn add_json<T: Serialize>(&mut self, name: &str, value: &T) -> Result<()> {
        self.add_bytes(name, &serde_json::to_vec_pretty(value)?)
    }

    pub fn add_ndjson<T: Serialize>(&mut self, name: &str, items: &[T]) -> Result<()> {
        let mut buf = Vec::new();
        for item in items {
            serde_json::to_writer(&mut buf, item)?;
            buf.push(b'\n');
        }
        self.add_bytes(name, &buf)
    }

    pub fn finish(mut self, mut manifest: Manifest) -> Result<W> {
        manifest.checksums = std::mem::take(&mut self.checksums);
        let bytes = serde_json::to_vec_pretty(&manifest)?;
        self.zip.start_file(MANIFEST_FILE, options())?;
        self.zip.write_all(&bytes)?;
        Ok(self.zip.finish()?)
    }
}

/// Reads a transfer bundle, with checksum verification against the manifest.
pub struct BundleReader<R: Read + Seek> {
    zip: ZipArchive<R>,
}

impl<R: Read + Seek> BundleReader<R> {
    pub fn new(reader: R) -> Result<Self> {
        Ok(Self {
            zip: ZipArchive::new(reader)?,
        })
    }

    pub fn try_read_bytes(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        match self.zip.by_name(name) {
            Ok(mut file) => {
                let mut buf = Vec::new();
                file.read_to_end(&mut buf)?;
                Ok(Some(buf))
            }
            Err(zip::result::ZipError::FileNotFound) => Ok(None),
            Err(e) => Err(eyre!("failed to read bundle entry {name}: {e}")),
        }
    }

    pub fn read_bytes(&mut self, name: &str) -> Result<Vec<u8>> {
        self.try_read_bytes(name)?
            .ok_or_else(|| eyre!("bundle is missing entry {name}"))
    }

    pub fn read_json<T: DeserializeOwned>(&mut self, name: &str) -> Result<T> {
        Ok(serde_json::from_slice(&self.read_bytes(name)?)?)
    }

    pub fn read_ndjson<T: DeserializeOwned>(&mut self, name: &str) -> Result<Vec<T>> {
        let Some(bytes) = self.try_read_bytes(name)? else {
            return Ok(Vec::new());
        };
        bytes
            .split(|&b| b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                serde_json::from_slice(line).map_err(|e| eyre!("malformed line in {name}: {e}"))
            })
            .collect()
    }

    pub fn entries_with_prefix(&self, prefix: &str) -> Vec<String> {
        self.zip
            .file_names()
            .filter(|name| name.starts_with(prefix))
            .map(str::to_string)
            .collect()
    }

    pub fn manifest(&mut self) -> Result<Manifest> {
        self.read_json(MANIFEST_FILE)
    }

    pub fn verify_checksums(&mut self) -> Result<()> {
        let manifest = self.manifest()?;
        if manifest.bundle_version > super::model::BUNDLE_VERSION {
            return Err(eyre!(
                "bundle version {} is newer than supported version {}",
                manifest.bundle_version,
                super::model::BUNDLE_VERSION
            ));
        }
        for (name, expected) in &manifest.checksums {
            let actual = checksum(&self.read_bytes(name)?);
            if &actual != expected {
                return Err(eyre!("checksum mismatch for bundle entry {name}"));
            }
        }
        Ok(())
    }
}
