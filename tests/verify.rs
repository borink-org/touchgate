//! `touchgate verify` against approvals signed as a FIDO2 key signs them,
//! checked by the real git and `ssh-keygen`.
//!
//! A software key stands in for the security key: it signs the authenticator
//! data of `sk-ssh-ed25519@openssh.com` with whatever flags a test asks for,
//! which is what a security key asked for a silent assertion would do too.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256, Sha512};

const SK: &str = "sk-ssh-ed25519@openssh.com";
const APPLICATION: &str = "ssh:signing";
const PRESENT_AND_VERIFIED: u8 = 0x05;
/// The commit of the publish branch the test releases run from.
const WORKFLOW: &str = "1111111111111111111111111111111111111111";
/// An approval as `touchgate approve` writes it.
const MESSAGE: &str =
    "Approve release 0.1.0.\n\nRelease-Workflow: 1111111111111111111111111111111111111111\n";

#[test]
fn accepts_an_approval_made_with_touch_and_pin() {
    let release = Release::new(PRESENT_AND_VERIFIED);
    let output = release.verify();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8(output.stdout.clone()).unwrap().trim(),
        release.approval
    );
}

#[test]
fn refuses_an_approval_made_without_the_pin() {
    let output = Release::new(0x01).verify();
    assert!(
        stderr(&output).contains("without the key's PIN"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn refuses_a_silent_approval_that_git_accepts() {
    let release = Release::new(0x00);
    let output = release.verify();
    assert!(
        stderr(&output).contains("without a touch"),
        "{}",
        stderr(&output)
    );
    // What touchgate is for: git alone takes the silent signature.
    let allowed = release.dir.join("allowed_signers");
    std::fs::write(&allowed, format!("someone {}\n", release.key)).unwrap();
    let git = release
        .git()
        .args(["-c", "gpg.format=ssh", "-c"])
        .arg(format!("gpg.ssh.allowedSignersFile={}", allowed.display()))
        .args(["verify-commit", &release.approval])
        .output()
        .unwrap();
    assert!(git.status.success(), "{}", stderr(&git));
}

#[test]
fn refuses_flags_changed_after_signing() {
    let release = Release::build(0x00, PRESENT_AND_VERIFIED, false, 0, MESSAGE);
    let output = release.verify();
    assert!(
        stderr(&output).contains("is invalid"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn refuses_a_merge_that_brings_in_unapproved_changes() {
    let output =
        Release::build(PRESENT_AND_VERIFIED, PRESENT_AND_VERIFIED, true, 0, MESSAGE).verify();
    assert!(
        stderr(&output).contains("another tree"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn refuses_another_key() {
    let release = Release::new(PRESENT_AND_VERIFIED);
    let other = public_key_line(&SigningKey::from_bytes(&[9; 32]));
    let output = release.verify_with(&other);
    assert!(
        stderr(&output).contains("another key"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn refuses_a_stale_approval() {
    let two_hours = 2 * 3_600;
    let release = Release::build(
        PRESENT_AND_VERIFIED,
        PRESENT_AND_VERIFIED,
        false,
        two_hours,
        MESSAGE,
    );
    let output = release.verify();
    let expected = "2 hours old, older than the 60 minutes an approval lasts";
    assert!(stderr(&output).contains(expected), "{}", stderr(&output));
    // A longer limit admits it.
    let longer = Command::new(env!("CARGO_BIN_EXE_touchgate"))
        .args(["verify", "--repo"])
        .arg(&release.dir)
        .args([
            "--commit",
            &release.merge,
            "--branch",
            "main",
            "--key",
            &release.key,
        ])
        .args(["--workflow-commit", WORKFLOW])
        .args(["--max-age", "3h"])
        .output()
        .unwrap();
    assert!(longer.status.success(), "{}", stderr(&longer));
}

#[test]
fn ignores_replacement_objects() {
    // A merge of the unsigned prepare commit, which a `refs/replace/` ref
    // tries to pass off as the approved merge.
    let release = Release::new(PRESENT_AND_VERIFIED);
    let tree = release.run(&["rev-parse", &format!("{}^{{tree}}", release.merge)]);
    let bogus = release.run(&[
        "commit-tree",
        &tree,
        "-p",
        &release.base,
        "-p",
        &release.prepared,
        "-m",
        "Merge.",
    ]);
    release.run(&["replace", &bogus, &release.merge]);
    release.run(&["update-ref", "refs/heads/main", &bogus]);
    let output = Command::new(env!("CARGO_BIN_EXE_touchgate"))
        .args(["verify", "--repo"])
        .arg(&release.dir)
        .args([
            "--commit",
            &bogus,
            "--branch",
            "main",
            "--key",
            &release.key,
        ])
        .args(["--workflow-commit", WORKFLOW])
        .output()
        .unwrap();
    assert!(
        stderr(&output).contains("not an approval"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn refuses_a_release_from_a_changed_publish_branch() {
    let release = Release::new(PRESENT_AND_VERIFIED);
    let output = Command::new(env!("CARGO_BIN_EXE_touchgate"))
        .args(["verify", "--repo"])
        .arg(&release.dir)
        .args([
            "--commit",
            &release.merge,
            "--branch",
            "main",
            "--key",
            &release.key,
        ])
        .args([
            "--workflow-commit",
            "2222222222222222222222222222222222222222",
        ])
        .output()
        .unwrap();
    assert!(
        stderr(&output).contains("publish branch changed"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn refuses_an_approval_that_names_no_publish_commit() {
    let release = Release::build(
        PRESENT_AND_VERIFIED,
        PRESENT_AND_VERIFIED,
        false,
        0,
        "Approve release 0.1.0.\n",
    );
    let output = release.verify();
    assert!(stderr(&output).contains("no single"), "{}", stderr(&output));
}

/// A repository whose `main` ends in the merge of a signed approval.
struct Release {
    dir: PathBuf,
    key: String,
    base: String,
    prepared: String,
    approval: String,
    merge: String,
}

impl Release {
    fn new(flags: u8) -> Self {
        Self::build(flags, flags, false, 0, MESSAGE)
    }

    /// Signs the approval with `flags`, and writes `claimed` as its flags.
    /// With `moved`, `main` gains a commit before the merge. The approval is
    /// dated `age` seconds ago.
    fn build(flags: u8, claimed: u8, moved: bool, age: u64, message: &str) -> Self {
        // Tests run in parallel, so each repository gets a directory of its own.
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "touchgate-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let mut release = Self {
            dir,
            key: public_key_line(&signing_key),
            base: String::new(),
            prepared: String::new(),
            approval: String::new(),
            merge: String::new(),
        };

        release.run(&["init", "-q", "-b", "main"]);
        std::fs::write(release.dir.join("f"), "1\n").unwrap();
        release.run(&["add", "f"]);
        release.run(&["commit", "-qm", "Base."]);
        let base = release.run(&["rev-parse", "HEAD"]);
        std::fs::write(release.dir.join("f"), "2\n").unwrap();
        release.run(&["commit", "-qam", "Prepare the 0.1.0 release."]);
        let prepared = release.run(&["rev-parse", "HEAD"]);
        let tree = release.run(&["rev-parse", "HEAD^{tree}"]);
        release.base = base.clone();
        release.prepared = prepared.clone();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let identity = format!("T <t@example.com> {} +0000", now - age);
        let unsigned = format!(
            "tree {tree}\nparent {prepared}\nauthor {identity}\ncommitter {identity}\n\n{message}"
        );
        let armored = sign(&signing_key, unsigned.as_bytes(), flags, claimed);
        let signed = unsigned.replacen(
            "\n\n",
            &format!("\ngpgsig {}\n\n", armored.replace('\n', "\n ")),
            1,
        );
        release.approval = release.run_with_input(
            &["hash-object", "-t", "commit", "-w", "--stdin"],
            signed.as_bytes(),
        );

        if moved {
            release.run(&["checkout", "-q", &base]);
            std::fs::write(release.dir.join("g"), "x\n").unwrap();
            release.run(&["add", "g"]);
            release.run(&["commit", "-qm", "Other."]);
            let approval = release.approval.clone();
            release.run(&["merge", "-q", "--no-ff", "-m", "Merge.", &approval]);
            release.merge = release.run(&["rev-parse", "HEAD"]);
        } else {
            let approval = release.approval.clone();
            release.merge = release.run(&[
                "commit-tree",
                &tree,
                "-p",
                &base,
                "-p",
                &approval,
                "-m",
                "Merge.",
            ]);
        }
        let merge = release.merge.clone();
        release.run(&["update-ref", "refs/heads/main", &merge]);
        release
    }

    fn verify(&self) -> std::process::Output {
        self.verify_with(&self.key)
    }

    fn verify_with(&self, key: &str) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_touchgate"))
            .args(["verify", "--repo"])
            .arg(&self.dir)
            .args(["--commit", &self.merge, "--branch", "main", "--key", key])
            .args(["--workflow-commit", WORKFLOW])
            .output()
            .unwrap()
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
        self.run_with_input(args, b"")
    }

    fn run_with_input(&self, args: &[&str], input: &[u8]) -> String {
        let mut child = self
            .git()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), input).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "git {args:?}: {}", stderr(&output));
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}

impl Drop for Release {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn stderr(output: &std::process::Output) -> String {
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

/// An SSHSIG signature of `message` as a FIDO2 key makes it: the key signs
/// the hash of the application, the flags, a counter and the hash of the
/// signed data. `claimed` is the flags byte written into the signature.
fn sign(key: &SigningKey, message: &[u8], flags: u8, claimed: u8) -> String {
    let signed = [
        &b"SSHSIG"[..],
        &string(b"git"),
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
        &string(b"git"),
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
