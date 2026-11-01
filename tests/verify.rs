//! `touchgate verify` against approvals signed as a FIDO2 key signs them,
//! checked by the real git and `ssh-keygen`.
//!
//! A software key takes the place of the security key. It signs the
//! authenticator data of `sk-ssh-ed25519@openssh.com` with the flags that a
//! test asks for. A security key asked for a silent assertion signs the same
//! way.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256, Sha512};

const SK: &str = "sk-ssh-ed25519@openssh.com";
const APPLICATION: &str = "ssh:signing";
const PRESENT_AND_VERIFIED: u8 = 0x05;
/// The namespace touchgate signs approvals in.
const NAMESPACE: &str = "touchgate-release";
/// The commit of the publish branch the test releases run from.
const WORKFLOW: &str = "1111111111111111111111111111111111111111";
const REPOSITORY: &str = "o/r";
/// A prepared changelog. The fixture puts the date of the approval for DATE.
const PREPARED: &str = "# Changelog\n\n## 0.1.0 - DATE\n\n- First.\n";

#[test]
fn accepts_an_approval_made_with_touch_and_pin() {
    let release = Release::build(Options::default());
    let outputs = release.dir.join("outputs");
    std::fs::write(&outputs, "").unwrap();
    let output = release
        .verify_command(&release.key)
        .env("GITHUB_OUTPUT", &outputs)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        release.commit
    );
    assert_eq!(
        std::fs::read_to_string(&outputs).unwrap(),
        format!("commit={}\nversion=0.1.0\n", release.commit)
    );
}

#[test]
fn refuses_an_approval_made_without_the_pin() {
    let output = Release::build(Options {
        flags: 0x01,
        claimed: 0x01,
        ..Options::default()
    })
    .verify();
    assert_refused(&output, "without the key's PIN");
}

#[test]
fn refuses_a_silent_approval_that_ssh_keygen_accepts() {
    let release = Release::build(Options {
        flags: 0x00,
        claimed: 0x00,
        ..Options::default()
    });
    assert_refused(&release.verify(), "without a touch");
    // What touchgate is for: ssh-keygen alone takes the silent signature.
    let text = std::fs::read_to_string(&release.approval).unwrap();
    let split = text.find("-----BEGIN").unwrap();
    let (message, signature) = text.split_at(split);
    let allowed = release.dir.join("allowed_signers");
    let signature_file = release.dir.join("signature");
    std::fs::write(&allowed, format!("someone {}\n", release.key)).unwrap();
    std::fs::write(&signature_file, signature).unwrap();
    let mut child = Command::new("ssh-keygen")
        .args(["-Y", "verify", "-n", NAMESPACE, "-I", "someone", "-f"])
        .arg(&allowed)
        .arg("-s")
        .arg(&signature_file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), message.as_bytes()).unwrap();
    let ssh_keygen = child.wait_with_output().unwrap();
    assert!(ssh_keygen.status.success(), "{}", stderr(&ssh_keygen));
}

#[test]
fn refuses_flags_changed_after_signing() {
    let output = Release::build(Options {
        flags: 0x00,
        ..Options::default()
    })
    .verify();
    assert_refused(&output, "is invalid");
}

#[test]
fn refuses_a_message_changed_after_signing() {
    let release = Release::build(Options::default());
    let text = std::fs::read_to_string(&release.approval).unwrap();
    std::fs::write(
        &release.approval,
        text.replace(&release.commit, &release.base),
    )
    .unwrap();
    assert_refused(&release.verify(), "is invalid");
}

#[test]
fn refuses_another_key() {
    let release = Release::build(Options::default());
    let other = public_key_line(&SigningKey::from_bytes(&[9; 32]));
    assert_refused(&release.verify_with(&other), "another key");
}

#[test]
fn refuses_a_commit_signature_made_with_the_same_key() {
    let output = Release::build(Options {
        namespace: "git",
        ..Options::default()
    })
    .verify();
    assert_refused(&output, "namespace `git`");
}

#[test]
fn refuses_a_stale_approval() {
    let release = Release::build(Options {
        age: 2 * 3_600,
        ..Options::default()
    });
    assert_refused(
        &release.verify(),
        "2 hours old, older than the 15 minutes an approval lasts",
    );
    // A longer limit admits it.
    let longer = release
        .verify_command(&release.key)
        .args(["--max-age", "3h"])
        .output()
        .unwrap();
    assert!(longer.status.success(), "{}", stderr(&longer));
}

#[test]
fn refuses_a_release_from_a_changed_publish_branch() {
    let release = Release::build(Options::default());
    let output = release
        .verify_command(&release.key)
        .args([
            "--workflow-commit",
            "2222222222222222222222222222222222222222",
        ])
        .output()
        .unwrap();
    assert_refused(&output, "publish branch changed");
}

#[test]
fn refuses_an_approval_for_another_repository() {
    let release = Release::build(Options::default());
    let output = release
        .verify_command(&release.key)
        .args(["--repository", "o/other"])
        .output()
        .unwrap();
    assert_refused(&output, "is for o/r, not for o/other");
}

#[test]
fn refuses_a_commit_that_is_not_on_the_default_branch() {
    let output = Release::build(Options {
        off_main: true,
        ..Options::default()
    })
    .verify();
    assert_refused(&output, "is not on main");
}

#[test]
fn refuses_another_version_than_the_changelog_releases() {
    let output = Release::build(Options {
        version: "0.2.0",
        ..Options::default()
    })
    .verify();
    assert_refused(&output, "the approval is for 0.2.0, but");
}

#[test]
fn refuses_a_release_dated_before_the_day_of_the_approval() {
    let output = Release::build(Options {
        changelog: "# Changelog\n\n## 0.1.0 - 2026-01-01\n\n- First.\n",
        ..Options::default()
    })
    .verify();
    assert_refused(&output, "dates 0.1.0 2026-01-01, before the approval on");
}

#[test]
fn refuses_a_commit_with_unreleased_entries_hidden_by_a_replacement() {
    let release = Release::build(Options {
        changelog: "# Changelog\n\n## Unreleased\n\n- Second.\n\n## 0.1.0 - DATE\n\n- First.\n",
        ..Options::default()
    });
    assert_refused(&release.verify(), "entries under `## Unreleased`");
    // A `refs/replace/` ref that answers for the commit with a clean one
    // changes nothing.
    std::fs::write(release.dir.join("CHANGELOG.md"), &release.prepared).unwrap();
    release.run(&["commit", "-qam", "Clean."]);
    let clean = release.run(&["rev-parse", "HEAD"]);
    release.run(&["replace", &release.commit, &clean]);
    assert_refused(&release.verify(), "entries under `## Unreleased`");
}

/// What a test changes about the release and its approval.
struct Options {
    /// The flags the key signs with.
    flags: u8,
    /// The flags written into the signature.
    claimed: u8,
    namespace: &'static str,
    /// How many seconds ago the approval was signed.
    age: u64,
    /// The version the approval names.
    version: &'static str,
    /// The changelog of the released commit, with the date of the approval
    /// for DATE.
    changelog: &'static str,
    /// Whether the released commit is on a branch of its own, not on `main`.
    off_main: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            flags: PRESENT_AND_VERIFIED,
            claimed: PRESENT_AND_VERIFIED,
            namespace: NAMESPACE,
            age: 0,
            version: "0.1.0",
            changelog: PREPARED,
            off_main: false,
        }
    }
}

/// A repository whose `main` ends in a prepared release, and a signed
/// approval of it in a file beside the repository.
struct Release {
    dir: PathBuf,
    key: String,
    base: String,
    commit: String,
    approval: PathBuf,
    /// [`PREPARED`], dated the day of the approval.
    prepared: String,
}

impl Release {
    fn build(options: Options) -> Self {
        // Tests run in parallel, so each repository gets a directory of its own.
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "touchgate-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("repo");
        std::fs::create_dir_all(&dir).unwrap();
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let mut release = Self {
            dir,
            key: public_key_line(&signing_key),
            base: String::new(),
            commit: String::new(),
            approval: root.join("approval.txt"),
            prepared: String::new(),
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let time = now - options.age;
        let date = utc_date(time);
        release.prepared = PREPARED.replace("DATE", &date);

        release.run(&["init", "-q", "-b", "main"]);
        std::fs::write(
            release.dir.join("CHANGELOG.md"),
            "# Changelog\n\n## Unreleased\n\n- First.\n",
        )
        .unwrap();
        release.run(&["add", "CHANGELOG.md"]);
        release.run(&["commit", "-qm", "Base."]);
        release.base = release.run(&["rev-parse", "HEAD"]);
        if options.off_main {
            release.run(&["switch", "-qc", "other"]);
        }
        std::fs::write(
            release.dir.join("CHANGELOG.md"),
            options.changelog.replace("DATE", &date),
        )
        .unwrap();
        release.run(&["commit", "-qam", "Prepare the 0.1.0 release."]);
        release.commit = release.run(&["rev-parse", "HEAD"]);
        if options.off_main {
            release.run(&["switch", "-q", "main"]);
        }

        let message = format!(
            "touchgate release approval\nrepository: {REPOSITORY}\ncommit: {}\nversion: {}\n\
             workflow-commit: {WORKFLOW}\ntime: {}\n",
            release.commit, options.version, time
        );
        let armored = sign(
            &signing_key,
            message.as_bytes(),
            options.namespace,
            options.flags,
            options.claimed,
        );
        std::fs::write(&release.approval, format!("{message}{armored}\n")).unwrap();
        release
    }

    fn verify(&self) -> Output {
        self.verify_with(&self.key)
    }

    fn verify_with(&self, key: &str) -> Output {
        self.verify_command(key).output().unwrap()
    }

    /// Returns `touchgate verify` with every value as a flag. A flag given
    /// again after these takes the place of the first.
    fn verify_command(&self, key: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_touchgate"));
        command
            .args(["verify", "--repo"])
            .arg(&self.dir)
            .arg("--approval")
            .arg(&self.approval)
            .args(["--branch", "main", "--repository", REPOSITORY, "--key", key])
            .args(["--workflow-commit", WORKFLOW]);
        command
    }

    fn git(&self) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.dir)
            .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
            .args(["-c", "commit.gpgsign=false"]);
        command
    }

    fn run(&self, args: &[&str]) -> String {
        let output = self.git().args(args).output().unwrap();
        assert!(output.status.success(), "git {args:?}: {}", stderr(&output));
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}

impl Drop for Release {
    fn drop(&mut self) {
        if let Some(root) = self.dir.parent() {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

/// Returns the UTC date of `seconds` since the Unix epoch, as YYYY-MM-DD,
/// with Howard Hinnant's `civil_from_days`.
fn utc_date(seconds: u64) -> String {
    let z = (seconds / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn assert_refused(output: &Output, expected: &str) {
    assert!(!output.status.success(), "verify passed");
    assert!(stderr(output).contains(expected), "{}", stderr(output));
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn string(bytes: &[u8]) -> Vec<u8> {
    let mut out = (bytes.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(bytes);
    out
}

fn key_blob(key: &SigningKey) -> Vec<u8> {
    [
        string(SK.as_bytes()),
        string(&key.verifying_key().to_bytes()),
        string(APPLICATION.as_bytes()),
    ]
    .concat()
}

fn public_key_line(key: &SigningKey) -> String {
    format!("{SK} {} test", base64(&key_blob(key)))
}

/// Returns an SSHSIG signature of `message`, made the way a FIDO2 key makes
/// it. The key signs the hash of the application, the flags, a counter and
/// the hash of the signed data. `claimed` is the flags byte written into the
/// signature.
fn sign(key: &SigningKey, message: &[u8], namespace: &str, flags: u8, claimed: u8) -> String {
    let signed = [
        &b"SSHSIG"[..],
        &string(namespace.as_bytes()),
        &string(b""),
        &string(b"sha512"),
        &string(&Sha512::digest(message)),
    ]
    .concat();
    let counter = 1u32.to_be_bytes();
    let authenticator_data = [
        &Sha256::digest(APPLICATION.as_bytes())[..],
        &[flags],
        &counter,
        &Sha256::digest(&signed),
    ]
    .concat();
    let signature = key.sign(&authenticator_data).to_bytes();
    let signature_blob = [
        &string(SK.as_bytes())[..],
        &string(&signature),
        &[claimed],
        &counter,
    ]
    .concat();
    let blob = [
        &b"SSHSIG"[..],
        &1u32.to_be_bytes(),
        &string(&key_blob(key)),
        &string(namespace.as_bytes()),
        &string(b""),
        &string(b"sha512"),
        &string(&signature_blob),
    ]
    .concat();
    let encoded = base64(&blob);
    let lines: Vec<&str> = encoded
        .as_bytes()
        .chunks(70)
        .map(|line| std::str::from_utf8(line).unwrap())
        .collect();
    format!(
        "-----BEGIN SSH SIGNATURE-----\n{}\n-----END SSH SIGNATURE-----",
        lines.join("\n")
    )
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let word = chunk
            .iter()
            .enumerate()
            .fold(0u32, |word, (i, &b)| word | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(word >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
