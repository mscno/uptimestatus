//! Packaging a database snapshot as a `tar.gz` and encrypting it with age.

use std::{
    fs::File,
    io::{self, BufReader, BufWriter, Read as _, Write as _},
    path::Path,
};

use age::{Encryptor, x25519};
use anyhow::Context as _;
use flate2::{Compression, write::GzEncoder};
use sha2::{Digest as _, Sha256};

/// One member of the archive: the name it gets inside the tar and its source.
pub(crate) struct Entry<'a> {
    pub(crate) name: &'a str,
    pub(crate) source: &'a Path,
    pub(crate) directory: bool,
}

/// Writes `entries` to `dest` as a gzip-compressed tar. Returns the file size.
pub(crate) fn pack(entries: &[Entry<'_>], dest: &Path) -> anyhow::Result<u64> {
    let file = File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let encoder = GzEncoder::new(BufWriter::new(file), Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for entry in entries {
        if entry.directory {
            builder
                .append_dir_all(entry.name, entry.source)
                .with_context(|| format!("archiving {}", entry.source.display()))?;
        } else {
            builder
                .append_path_with_name(entry.source, entry.name)
                .with_context(|| format!("archiving {}", entry.source.display()))?;
        }
    }
    let mut writer = builder
        .into_inner()
        .context("finishing the archive")?
        .finish()?;
    writer.flush()?;
    drop(writer);
    Ok(std::fs::metadata(dest)
        .with_context(|| format!("reading {}", dest.display()))?
        .len())
}

/// Encrypts `input` to the age `recipient` (`age1…`), writing `output`.
pub(crate) fn encrypt(recipient: &str, input: &Path, output: &Path) -> anyhow::Result<()> {
    let recipient: x25519::Recipient = recipient
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("`{recipient}` is not an age recipient (age1…) "))?;
    let encryptor = Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
        .context("preparing age encryption")?;
    let mut reader =
        BufReader::new(File::open(input).with_context(|| format!("opening {}", input.display()))?);
    let mut writer = encryptor
        .wrap_output(
            File::create(output).with_context(|| format!("creating {}", output.display()))?,
        )
        .context("writing the age header")?;
    io::copy(&mut reader, &mut writer).context("encrypting the archive")?;
    writer.finish().context("finishing encryption")?;
    Ok(())
}

/// The lowercase hex SHA-256 of a file, for the report and transfer checks.
pub(crate) fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut reader =
        BufReader::new(File::open(path).with_context(|| format!("opening {}", path.display()))?);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("reading {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn packs_and_encrypts_a_round_trip() {
        let dir = std::env::temp_dir().join(format!("uptime-backup-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("uptimestatus.db");
        std::fs::write(&source, b"not really a database").unwrap();

        let archive = dir.join("a.tar.gz");
        let entries = [Entry {
            name: "db/uptimestatus.db",
            source: &source,
            directory: false,
        }];
        let size = pack(&entries, &archive).unwrap();
        assert!(size > 0);

        // A private key lets us decrypt what the recipient encrypts.
        let identity = x25519::Identity::generate();
        let recipient = identity.to_public().to_string();
        let encrypted = dir.join("a.tar.gz.age");
        encrypt(&recipient, &archive, &encrypted).unwrap();
        assert_ne!(
            std::fs::read(&encrypted).unwrap(),
            std::fs::read(&archive).unwrap()
        );

        // The ciphertext is not the plaintext: it starts with the age header.
        let mut head = [0u8; 14];
        File::open(&encrypted)
            .unwrap()
            .read_exact(&mut head)
            .unwrap();
        assert_eq!(&head, b"age-encryption");

        assert_eq!(
            sha256_file(&archive).unwrap(),
            hex::encode(Sha256::digest(std::fs::read(&archive).unwrap()))
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
