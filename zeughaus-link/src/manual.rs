//! The `/manual` exchange: a working-tree snapshot on its way to the CI,
//! which runs named jobs of the repository's pipeline from it.
//!
//! One exchange per run on [`crate::MANUAL_PATH`]: a [`RunUpload`] frame,
//! then the bundles as raw bytes (`sizes[i]` each, in `dirs` order), then the
//! uploader finishes its side; the CI answers with one [`RunReply`] frame.
//! Frames are those of [`crate::enlist`]: a 4-byte little-endian length and a
//! postcard body.

use serde::{Deserialize, Serialize};

use crate::enlist::UploadDir;

/// The largest bundle the CI accepts for one checkout.
pub const MAX_MANUAL_BUNDLE_BYTES: u64 = 512 << 20;

/// The most checkouts (the main one and its sibling path dependencies) one
/// manual run carries.
pub const MAX_MANUAL_DIRS: usize = 8;

/// The most jobs one manual run names.
pub const MAX_MANUAL_JOBS: usize = 16;

/// First frame of a `/manual` exchange; the bundles follow as raw bytes,
/// `sizes[i]` each.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunUpload {
    /// `git remote get-url origin` of the main checkout; the CI names the
    /// repository by it.
    pub origin: String,
    /// `<user>@<hostname>` of whoever uploads.
    pub actor: String,
    /// The subject of the main checkout's commit; the pipeline's subject.
    pub summary: String,
    /// The jobs of the snapshot's `.zeughaus-ci/` to run, by name.
    pub jobs: Vec<String>,
    /// The main checkout first, then its sibling checkouts.
    pub dirs: Vec<UploadDir>,
    /// One bundle size in bytes per dir.
    pub sizes: Vec<u64>,
}

/// The CI's answer to a [`RunUpload`] and its bundles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunReply {
    /// The run is a pipeline now: `number` in `repo`.
    Accepted {
        repo: String,
        number: u64,
    },
    Refused {
        message: String,
    },
}

/// `[A-Za-z0-9._-]{1,100}`, not `.` or `..`: a checkout directory name.
pub fn valid_dir_name(name: &str) -> bool {
    (1..=100).contains(&name.len())
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_names_are_one_plain_path_component() {
        for ok in [
            "zeughaus",
            "weida",
            "iced_nodegraph",
            "a.b-c",
            "..a",
            "x".repeat(100).as_str(),
        ] {
            assert!(valid_dir_name(ok), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "with space",
            "umlaut-\u{e4}",
            "x".repeat(101).as_str(),
        ] {
            assert!(!valid_dir_name(bad), "{bad}");
        }
    }

    #[cfg(feature = "tokio")]
    #[tokio::test]
    async fn an_upload_and_its_reply_travel_as_frames() {
        use crate::enlist::{read_frame, write_frame};

        let upload = RunUpload {
            origin: "git@github.com:griasdi/griasdi.git".to_owned(),
            actor: "hannes@tuco-pc".to_owned(),
            summary: "fix the build".to_owned(),
            jobs: vec![
                "build-windows-amd64".to_owned(),
                "check-windows-amd64".to_owned(),
            ],
            dirs: vec![
                UploadDir {
                    dir: "griasdi".to_owned(),
                    sha: "0".repeat(40),
                },
                UploadDir {
                    dir: "weida".to_owned(),
                    sha: "1".repeat(40),
                },
            ],
            sizes: vec![10, 20],
        };
        let reply = RunReply::Accepted {
            repo: "griasdi".to_owned(),
            number: 3,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &upload).await.unwrap();
        write_frame(&mut buf, &reply).await.unwrap();
        let mut r: &[u8] = &buf;
        assert_eq!(read_frame(&mut r).await.unwrap(), Some(upload));
        assert_eq!(read_frame(&mut r).await.unwrap(), Some(reply));
    }
}
