//! The speech model releases this build expects, and how `setup` installs them.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

/// A published speech model, pinned by the SHA-256 of each of its files.
pub struct Release {
    pub name: &'static str,
    pub licence: &'static str,
    pub url: &'static str,
    /// Size of the download, used when the server does not state it.
    pub download_bytes: u64,
    layout: Layout,
}

enum Layout {
    /// A `.tar.bz2` archive. These files from it, by name and SHA-256, form
    /// the model directory.
    #[cfg_attr(
        not(feature = "parakeet"),
        allow(dead_code, reason = "only the Parakeet release is an archive")
    )]
    Archive(&'static [(&'static str, &'static str)]),
    /// The download is the model file itself, with this SHA-256.
    File(&'static str),
}

/// The four files `kodiak-asr` loads, as extracted on 20 November 2026.
#[cfg(feature = "parakeet")]
pub const PARAKEET: Release = Release {
    name: "NVIDIA Parakeet TDT 0.6B v2 (INT8)",
    licence: "CC-BY-4.0",
    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8.tar.bz2",
    download_bytes: 482_440_461,
    layout: Layout::Archive(&[
        (
            "encoder.int8.onnx",
            "92a10bd9c23936ea0566c29f963bc57c2a5df4261b6a8921c39abea66557ca8b",
        ),
        (
            "decoder.int8.onnx",
            "4d097bae3bac5c96018d5293ba39f1b657296102b142affda40378f38b020f41",
        ),
        (
            "joiner.int8.onnx",
            "77a29f3e5ac7d37074e0af465391b80f314015d5b97f0f90c6c7767f1d56f428",
        ),
        (
            "tokens.txt",
            "ec182b70dd42113aff6c5372c75cac58c952443eb22322f57bbd7f53977d497d",
        ),
    ]),
};

pub const WHISPER_BASE_EN: Release = Release {
    name: "Whisper base.en",
    licence: "MIT",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin",
    download_bytes: 147_964_211,
    layout: Layout::File("a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002"),
};

/// What is at a model path, compared with the pinned release.
#[derive(Debug, PartialEq)]
pub enum State {
    Verified,
    Missing,
    /// Something is there but it is not this release; the text says what.
    Different(String),
}

impl Release {
    pub fn megabytes(&self) -> u64 {
        self.download_bytes / 1_000_000
    }

    pub fn check(&self, path: &Path) -> Result<State> {
        let files = match self.layout {
            Layout::Archive(files) => files
                .iter()
                .map(|(name, hash)| (path.join(name), *hash))
                .collect(),
            Layout::File(hash) => vec![(path.to_owned(), hash)],
        };
        if !files.iter().any(|(file, _)| file.exists()) {
            return Ok(State::Missing);
        }
        for (file, expected) in files {
            if !file.exists() {
                return Ok(State::Different(format!("{} is missing", file.display())));
            }
            let source = File::open(&file).with_context(|| format!("read {}", file.display()))?;
            if copy_hashed(source, io::sink())? != expected {
                return Ok(State::Different(format!(
                    "{} is not the pinned file",
                    file.display()
                )));
            }
        }
        Ok(State::Verified)
    }

    /// Downloads the release to `path`. `report` receives the bytes read so
    /// far and the total.
    pub fn fetch(&self, path: &Path, report: &mut dyn FnMut(u64, u64)) -> Result<()> {
        let response = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(30))
            .timeout_read(Duration::from_mins(1))
            .build()
            .get(self.url)
            .call()
            .with_context(|| format!("download {}", self.url))?;
        let total = response
            .header("Content-Length")
            .and_then(|length| length.parse().ok())
            .unwrap_or(self.download_bytes);
        let source = Counted {
            inner: response.into_reader(),
            done: 0,
            total,
            report,
        };
        self.install(source, path)
    }

    /// Reads the download from `source` and moves it to `path`. Nothing
    /// reaches `path` unless every file matched its pinned SHA-256.
    fn install(&self, source: impl Read, path: &Path) -> Result<()> {
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let staging = tempfile::tempdir_in(parent).context("create a download folder")?;
        match self.layout {
            Layout::File(expected) => {
                let staged = staging.path().join("model");
                let hash = copy_hashed(source, File::create(&staged)?)?;
                ensure!(hash == expected, "the download is not the pinned file");
                fs::rename(&staged, path)?;
            }
            Layout::Archive(files) => {
                let mut archive = tar::Archive::new(bzip2::read::BzDecoder::new(source));
                for entry in archive.entries().context("read the model archive")? {
                    let entry = entry.context("read the model archive")?;
                    let wanted = entry.header().entry_type().is_file().then(|| {
                        let entry_path = entry.path().ok()?;
                        files
                            .iter()
                            .find(|(name, _)| entry_path.file_name() == Some(OsStr::new(name)))
                            .copied()
                    });
                    let Some(Some((name, expected))) = wanted else {
                        continue;
                    };
                    let hash = copy_hashed(entry, File::create(staging.path().join(name))?)?;
                    ensure!(
                        hash == expected,
                        "{name} in the download is not the pinned file"
                    );
                }
                for (name, _) in files {
                    ensure!(
                        staging.path().join(name).exists(),
                        "the download has no {name}"
                    );
                }
                fs::create_dir_all(path)?;
                for (name, _) in files {
                    fs::rename(staging.path().join(name), path.join(name))?;
                }
            }
        }
        Ok(())
    }
}

/// Copies `source` to `sink` and returns the SHA-256 of what passed, in hex.
fn copy_hashed(mut source: impl Read, mut sink: impl Write) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 16];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            sink.flush()?;
            return Ok(hex::encode(hasher.finalize()));
        }
        hasher.update(&buffer[..read]);
        sink.write_all(&buffer[..read])?;
    }
}

/// Reports how much of a download has been read.
struct Counted<'a, R> {
    inner: R,
    done: u64,
    total: u64,
    report: &'a mut dyn FnMut(u64, u64),
}

impl<R: Read> Read for Counted<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.done += read as u64;
        (self.report)(self.done, self.total);
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufRead;
    use std::net::TcpListener;

    use super::*;

    fn pinned(contents: &[u8]) -> &'static str {
        Box::leak(hex::encode(Sha256::digest(contents)).into_boxed_str())
    }

    fn archive_release(files: &'static [(&'static str, &'static str)]) -> Release {
        Release {
            name: "test",
            licence: "test",
            url: "",
            download_bytes: 0,
            layout: Layout::Archive(files),
        }
    }

    /// A `.tar.bz2` holding `files` under one top-level directory.
    fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for (name, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, format!("model/{name}"), *contents)
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn an_archive_installs_only_its_pinned_files() {
        let files: &'static [_] = Box::leak(Box::new([
            ("encoder.onnx", pinned(b"encoder")),
            ("tokens.txt", pinned(b"tokens")),
        ]));
        let release = archive_release(files);
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("models/test");
        assert_eq!(release.check(&path).unwrap(), State::Missing);

        let download = archive(&[
            ("encoder.onnx", b"encoder"),
            ("sample.wav", b"not part of the model"),
            ("tokens.txt", b"tokens"),
        ]);
        release.install(download.as_slice(), &path).unwrap();

        assert_eq!(release.check(&path).unwrap(), State::Verified);
        assert!(!path.join("sample.wav").exists());
        // Only the model directory is left: the download folder is gone.
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn a_file_that_is_not_the_pinned_one_installs_nothing() {
        let files: &'static [_] = Box::leak(Box::new([
            ("encoder.onnx", pinned(b"encoder")),
            ("tokens.txt", pinned(b"tokens")),
        ]));
        let release = archive_release(files);
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("test");

        let tampered = archive(&[("encoder.onnx", b"encoder"), ("tokens.txt", b"other")]);
        let error = release.install(tampered.as_slice(), &path).unwrap_err();
        assert!(error.to_string().contains("tokens.txt"), "{error}");
        let incomplete = archive(&[("encoder.onnx", b"encoder")]);
        let error = release.install(incomplete.as_slice(), &path).unwrap_err();
        assert!(error.to_string().contains("no tokens.txt"), "{error}");

        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn an_altered_model_is_reported_as_different() {
        let files: &'static [_] = Box::leak(Box::new([
            ("encoder.onnx", pinned(b"encoder")),
            ("tokens.txt", pinned(b"tokens")),
        ]));
        let release = archive_release(files);
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("encoder.onnx"), b"encoder").unwrap();
        let State::Different(missing) = release.check(root.path()).unwrap() else {
            panic!("a model with a missing file is not verified");
        };
        assert!(missing.contains("tokens.txt"), "{missing}");
        fs::write(root.path().join("tokens.txt"), b"truncated").unwrap();
        assert!(matches!(
            release.check(root.path()).unwrap(),
            State::Different(_)
        ));
    }

    #[test]
    fn a_single_file_model_downloads_over_http() {
        const BODY: &[u8] = b"ggml model bytes";
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = io::BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while request.read_line(&mut line).unwrap() > 2 {
                line.clear();
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                BODY.len()
            )
            .unwrap();
            stream.write_all(BODY).unwrap();
        });
        let release = Release {
            name: "test",
            licence: "test",
            url: Box::leak(format!("http://{address}/model.bin").into_boxed_str()),
            download_bytes: 0,
            layout: Layout::File(pinned(BODY)),
        };
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("model.bin");
        let mut progress = (0, 0);
        release
            .fetch(&path, &mut |done, total| progress = (done, total))
            .unwrap();
        server.join().unwrap();

        assert_eq!(fs::read(&path).unwrap(), BODY);
        assert_eq!(progress, (BODY.len() as u64, BODY.len() as u64));
        assert_eq!(release.check(&path).unwrap(), State::Verified);
    }
}
