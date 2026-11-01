//! The check that a person approved a release with a hardware key, by touch
//! and PIN.
//!
//! An approval is a short message that names the repository, the commit to
//! release, its version, the commit of the `publish` branch that the release
//! is to run from, and the time it was signed. [`SignedApproval::text`] holds
//! the message and its SSH signature, and `touchgate approve` starts
//! `release.yml` with that text as its `approval` input. Nothing about the
//! approval lives in git, so approving adds no commit and starts no CI.
//!
//! `ssh-keygen -Y verify` checks the signature against the one key allowed.
//! It does not check how the key made the signature. The allowed-signers
//! format has no option that demands a touch or a PIN, and a FIDO2 key asked
//! for a silent assertion signs without either. The authenticator signs a
//! flags byte along with the message. This module reads that byte and
//! requires user presence and user verification.
//!
//! The signature is made in the namespace `touchgate-release`, not `git`. A
//! signature covers its namespace, so a commit signature made with the same
//! key can never pass as an approval, nor an approval as a commit signature.
//!
//! Nothing here runs code from the commit it checks. Until the check passes,
//! the tree is untrusted, and anything that builds it or runs `cargo` inside
//! it could fake a pass. This module reads the changelog at the commit as a
//! raw object with `git cat-file`. Replacement objects are off, because a
//! `refs/replace/` ref would make git answer for one commit with another. The
//! user's and the system's git configuration are off too.
//!
//! The approval names the commit of the `publish` branch the release is to
//! run from. A change to that branch after the approval stops the release
//! until someone approves again. This catches a change made in good faith that
//! the approver did not know of. A change made to get around the check can
//! remove the check as well. Only the ruleset on `publish` stops such a change.
//!
//! An approval expires: its time, which the signature covers, must be recent.
//! `touchgate approve` starts the release right after signing, and the
//! approval is checked first, so the limit can be short. Within it, anyone who
//! can start the workflow can start it again with the same approval. That
//! releases the same commit again, and publishing skips what the registry has
//! already.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::changelog::{self, Changelog};
use crate::version::{Date, Version};

/// The authenticator flag for user presence: the key was touched.
const USER_PRESENT: u8 = 0x01;
/// The authenticator flag for user verification: the PIN was entered.
const USER_VERIFIED: u8 = 0x04;

/// The namespace an approval is signed in.
const NAMESPACE: &str = "touchgate-release";

/// The first line of an approval's message.
const MESSAGE_HEADER: &str = "touchgate release approval";

/// The line before the signature in [`SignedApproval::text`].
const SIGNATURE_BEGIN: &str = "-----BEGIN SSH SIGNATURE-----";

/// The changelog that a release's version is read from, at the root of the
/// repository.
const CHANGELOG: &str = "CHANGELOG.md";

/// The principal the one allowed key is listed under. Only this module reads
/// the allowed-signers file, so the name is arbitrary.
const PRINCIPAL: &str = "touchgate";

/// How far an approval's time may lie ahead of the clock, for a signing
/// machine whose clock runs a little fast.
const CLOCK_SKEW_SECONDS: i64 = 300;

/// What a person approves: the release of `commit` in `repository` as
/// `version`, by `release.yml` from `workflow_commit` on `publish`.
#[derive(Debug, PartialEq)]
pub struct Approval {
    /// The repository as `owner/name`.
    pub repository: String,
    /// The full hash of the commit to release, on the default branch.
    pub commit: String,
    pub version: Version,
    /// The commit of the `publish` branch the release is to run from.
    pub workflow_commit: String,
    /// When the approval was signed, in seconds since the Unix epoch.
    pub time: i64,
}

impl Approval {
    /// Returns the message that the key signs, one `name: value` line per
    /// field after [`MESSAGE_HEADER`].
    fn message(&self) -> String {
        format!(
            "{MESSAGE_HEADER}\nrepository: {}\ncommit: {}\nversion: {}\nworkflow-commit: {}\ntime: {}\n",
            self.repository, self.commit, self.version, self.workflow_commit, self.time
        )
    }

    /// Reads a message as [`Approval::message`] writes it, and nothing else.
    fn parse_message(message: &str) -> Result<Self, String> {
        let malformed = |why: &str| format!("the approval is malformed: {why}");
        let body = message
            .strip_suffix('\n')
            .ok_or_else(|| malformed("it does not end in a newline"))?;
        let mut lines = body.split('\n');
        if lines.next() != Some(MESSAGE_HEADER) {
            return Err(malformed(&format!(
                "it does not start with `{MESSAGE_HEADER}`"
            )));
        }
        let mut field = |name: &str| {
            lines
                .next()
                .and_then(|line| line.strip_prefix(name))
                .and_then(|line| line.strip_prefix(": "))
                .ok_or_else(|| malformed(&format!("no `{name}` line where it belongs")))
        };
        let repository = field("repository")?.to_owned();
        let commit = field("commit")?.to_owned();
        let version = Version::parse(field("version")?)?;
        let workflow_commit = field("workflow-commit")?.to_owned();
        let time = field("time")?;
        let time = time
            .parse::<i64>()
            .ok()
            .filter(|_| !time.starts_with('+'))
            .ok_or_else(|| malformed(&format!("`{time}` is not a time in seconds")))?;
        if lines.next().is_some() {
            return Err(malformed("it has lines after `time`"));
        }
        if !is_repository(&repository) {
            return Err(malformed(&format!("`{repository}` is not owner/name")));
        }
        for hash in [&commit, &workflow_commit] {
            if !is_hash(hash) {
                return Err(malformed(&format!("`{hash}` is not a full commit hash")));
            }
        }
        Ok(Self {
            repository,
            commit,
            version,
            workflow_commit,
            time,
        })
    }

    /// Signs the approval with the key at `key`, which asks for its PIN and a
    /// touch.
    ///
    /// This function runs without an SSH agent. ssh-keygen would ask an agent
    /// that holds the key instead. An agent such as GNOME Keyring's cannot
    /// ask a security key for its PIN, so it refuses.
    pub fn sign(self, key: &Path) -> Result<SignedApproval, String> {
        let message = self.message();
        let dir = private_temp_dir()?;
        let payload = dir.join("approval");
        let signed = std::fs::write(&payload, &message)
            .map_err(|error| error.to_string())
            .and_then(|()| {
                let status = Command::new("ssh-keygen")
                    .env_remove("SSH_AUTH_SOCK")
                    // -q drops "Signing file" and "Write signature to", and
                    // keeps the PIN prompt and "Confirm user presence".
                    .args(["-q", "-Y", "sign", "-n", NAMESPACE, "-f"])
                    .arg(key)
                    .arg(&payload)
                    .status()
                    .map_err(|error| format!("ssh-keygen: {error}"))?;
                if !status.success() {
                    return Err("ssh-keygen could not sign the approval".to_owned());
                }
                std::fs::read_to_string(dir.join("approval.sig")).map_err(|error| error.to_string())
            });
        let _ = std::fs::remove_dir_all(&dir);
        Ok(SignedApproval {
            approval: self,
            message,
            armored: signed?.trim_end().to_owned(),
        })
    }
}

/// An approval with the signature that `touchgate approve` made. Parsing one
/// checks only its form; [`verify`] checks the signature.
#[derive(Debug)]
pub struct SignedApproval {
    pub approval: Approval,
    /// The exact bytes that were signed.
    message: String,
    /// The signature, from `-----BEGIN SSH SIGNATURE-----` to its end line.
    armored: String,
}

impl SignedApproval {
    /// Reads the text that [`SignedApproval::text`] writes.
    pub fn parse(text: &str) -> Result<Self, String> {
        let split = text
            .find(&format!("\n{SIGNATURE_BEGIN}\n"))
            .ok_or("the approval has no SSH signature")?;
        let (message, armored) = text.split_at(split + 1);
        Ok(Self {
            approval: Approval::parse_message(message)?,
            message: message.to_owned(),
            armored: armored.trim_end().to_owned(),
        })
    }

    /// Returns the message followed by its signature: what `release.yml`
    /// takes as its `approval` input, and what the GitHub release keeps.
    pub fn text(&self) -> String {
        format!("{}{}\n", self.message, self.armored)
    }

    /// Checks that `key` signed the message with a touch and the PIN, in the
    /// namespace of approvals.
    fn check_signature(&self, key: &str) -> Result<(), String> {
        let (key_type, key_blob) = parse_public_key(key)?;
        let signature = Signature::parse(&decode_armor(&self.armored)?)?;
        if signature.public_key != key_blob {
            return Err("the approval is signed by another key than the one allowed".to_owned());
        }
        if signature.namespace != NAMESPACE.as_bytes() {
            return Err(format!(
                "the approval is signed in the namespace `{}`, not `{NAMESPACE}`. Approve with \
                 `touchgate approve`",
                String::from_utf8_lossy(&signature.namespace)
            ));
        }
        let Some(flags) = signature.flags else {
            return Err(
                "the approval is signed with a key that is not a FIDO2 key, so it carries no \
                 touch or PIN"
                    .to_owned(),
            );
        };
        if flags & USER_PRESENT == 0 {
            return Err("the approval was signed without a touch of the key".to_owned());
        }
        if flags & USER_VERIFIED == 0 {
            return Err("the approval was signed without the key's PIN".to_owned());
        }
        ssh_keygen_verify(key_type, key, self.message.as_bytes(), &self.armored)
            .map_err(|error| format!("the signature of the approval is invalid: {error}"))
    }
}

/// What a release run expects of its approval.
pub struct Expected<'a> {
    /// The public key that approves releases, as `ssh-keygen` writes it:
    /// `sk-ssh-ed25519@openssh.com AAAA... comment`.
    pub key: &'a str,
    /// The repository of the run, as `owner/name`.
    pub repository: &'a str,
    /// The ref the release must be on, such as `origin/main`.
    pub branch: &'a str,
    /// The commit of the `publish` branch this run's release workflow comes
    /// from.
    pub workflow_commit: &'a str,
    /// How long an approval lasts.
    pub max_age_seconds: i64,
}

/// Checks that `signed` approves a release that this run, as `expected`
/// describes it, may publish, with the repository at `repo`.
pub fn verify(repo: &Path, signed: &SignedApproval, expected: &Expected) -> Result<(), String> {
    signed.check_signature(expected.key)?;

    // Only now are the fields known to be the signer's.
    let approval = &signed.approval;
    if approval.repository != expected.repository {
        return Err(format!(
            "the approval is for {}, not for {}",
            approval.repository, expected.repository
        ));
    }
    if approval.workflow_commit != expected.workflow_commit {
        return Err(format!(
            "the approval expects the release to run from publish at {}, but this run comes \
             from {}. The publish branch changed after the approval: check the change, then \
             approve again",
            approval.workflow_commit, expected.workflow_commit
        ));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_secs() as i64;
    let age = now - approval.time;
    if age > expected.max_age_seconds {
        return Err(format!(
            "the approval is {} old, older than the {} an approval lasts. Approve again",
            describe(age),
            describe(expected.max_age_seconds)
        ));
    }
    if age < -CLOCK_SKEW_SECONDS {
        return Err(format!(
            "the approval is dated {} seconds in the future",
            -age
        ));
    }
    let (version, date) = approvable_release(repo, &approval.commit, expected.branch)?;
    if version != approval.version {
        return Err(format!(
            "the approval is for {}, but {} releases {version}",
            approval.version, approval.commit
        ));
    }
    check_release_date(&version, date, approval.time)
}

/// Checks that the changelog dates the release of `version` `date`, the day
/// of the approval at `time` or the day before, in UTC. The day before
/// admits a release prepared late in the evening and approved after midnight.
pub fn check_release_date(version: &Version, date: Date, time: i64) -> Result<(), String> {
    let day = Date::of_unix_time(time);
    if date == day || date == Date::of_unix_time(time - 86_400) {
        return Ok(());
    }
    let relation = if date > day { "after" } else { "before" };
    Err(format!(
        "{CHANGELOG} dates {version} {date}, {relation} the approval on {day}. Run \
         `touchgate prepare {version}` to date it anew, push that, and approve again"
    ))
}

/// Returns the version that `commit` releases and its date, read from the
/// changelog at the root of the repository at `repo`. Fails if `commit` is
/// not on `branch`, or if its changelog has entries under `## Unreleased`,
/// which the release would ship without naming them. Runs nothing from
/// `commit`.
pub fn approvable_release(
    repo: &Path,
    commit: &str,
    branch: &str,
) -> Result<(Version, Date), String> {
    if !is_hash(commit) {
        return Err(format!("`{commit}` is not a full commit hash"));
    }
    let ancestor = git(repo)
        .args(["merge-base", "--is-ancestor", commit, branch])
        .status()
        .map_err(|error| format!("git: {error}"))?;
    if !ancestor.success() {
        return Err(format!("{commit} is not on {branch}"));
    }
    let output = git(repo)
        .args(["cat-file", "blob", &format!("{commit}:{CHANGELOG}")])
        .output()
        .map_err(|error| format!("git: {error}"))?;
    if !output.status.success() {
        return Err(format!("{commit} has no {CHANGELOG} at the root"));
    }
    let text = crate::utf8(output.stdout, CHANGELOG)?;
    if !changelog::unreleased_entries(&text).is_empty() {
        return Err(format!(
            "{commit} has entries under `## Unreleased` in {CHANGELOG}, which this release would \
             ship unnamed. Prepare the release, or approve the commit that prepared it"
        ));
    }
    let changelog = Changelog::parse(&text).map_err(|errors| {
        format!(
            "{CHANGELOG} at {commit} is malformed: {}",
            errors.join("; ")
        )
    })?;
    changelog
        .newest_release()
        .map(|(version, date)| (version.clone(), date))
        .ok_or_else(|| format!("{CHANGELOG} at {commit} has no released section"))
}

/// Reads how long an approval lasts, such as `15m`, `1h` or `7d`, in seconds.
/// From a minute to 30 days.
pub fn parse_age(text: &str) -> Result<i64, String> {
    let invalid = || format!("`{text}` is not a duration from 1m to 30d, such as 30m, 1h or 7d");
    let (number, unit) = text.split_at(text.len().saturating_sub(1));
    let unit = match unit {
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return Err(invalid()),
    };
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let seconds = number
        .parse::<i64>()
        .ok()
        .and_then(|number| number.checked_mul(unit))
        .ok_or_else(invalid)?;
    if (60..=30 * 86_400).contains(&seconds) {
        Ok(seconds)
    } else {
        Err(invalid())
    }
}

/// Formats a number of seconds in minutes under two hours, in hours under two
/// days, and in days beyond.
fn describe(seconds: i64) -> String {
    match seconds {
        ..7_200 => format!("{} minutes", seconds / 60),
        7_200..172_800 => format!("{} hours", seconds / 3_600),
        _ => format!("{} days", seconds / 86_400),
    }
}

/// Returns whether `text` is a full SHA-1 or SHA-256 commit hash.
pub fn is_hash(text: &str) -> bool {
    (text.len() == 40 || text.len() == 64)
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Returns whether `text` reads as a GitHub repository, `owner/name`.
fn is_repository(text: &str) -> bool {
    let allowed = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    matches!(text.split_once('/'), Some((owner, name)) if allowed(owner) && allowed(name))
}

/// Returns a git command, with the settings that could make git answer for
/// another object turned off.
fn git(repo: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
    ] {
        command.env_remove(variable);
    }
    command
}

/// Runs `ssh-keygen -Y verify` with `key` as the one allowed signer.
fn ssh_keygen_verify(
    key_type: &str,
    key: &str,
    payload: &[u8],
    armored: &str,
) -> Result<(), String> {
    let dir = private_temp_dir()?;
    let allowed = dir.join("allowed_signers");
    let signature = dir.join("signature");
    let key_base64 = key.split_whitespace().nth(1).unwrap_or_default();
    let written = std::fs::write(
        &allowed,
        format!("{PRINCIPAL} namespaces=\"{NAMESPACE}\" {key_type} {key_base64}\n"),
    )
    .and_then(|()| std::fs::write(&signature, format!("{armored}\n")));
    let result = written.map_err(|error| error.to_string()).and_then(|()| {
        let mut child = Command::new("ssh-keygen")
            .args(["-Y", "verify", "-n", NAMESPACE, "-I", PRINCIPAL, "-f"])
            .arg(&allowed)
            .arg("-s")
            .arg(&signature)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("ssh-keygen: {error}"))?;
        child
            .stdin
            .take()
            .expect("piped")
            .write_all(payload)
            .map_err(|error| error.to_string())?;
        let output = child
            .wait_with_output()
            .map_err(|error| error.to_string())?;
        if output.status.success() {
            Ok(())
        } else {
            Err(crate::utf8(output.stderr, "ssh-keygen")?.trim().to_owned())
        }
    });
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// Creates a directory that no one else made. Creating it fails if the name is
/// taken, so nothing placed there beforehand is read as the allowed signers.
pub fn private_temp_dir() -> Result<PathBuf, String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("touchgate-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    Ok(dir)
}

/// Returns the bytes inside `-----BEGIN SSH SIGNATURE-----`.
fn decode_armor(armored: &str) -> Result<Vec<u8>, String> {
    let body: String = armored
        .lines()
        .map(str::trim)
        .skip_while(|line| *line != "-----BEGIN SSH SIGNATURE-----")
        .skip(1)
        .take_while(|line| *line != "-----END SSH SIGNATURE-----")
        .collect();
    if body.is_empty() {
        return Err("the approval is not signed with an SSH key".to_owned());
    }
    base64(&body).ok_or_else(|| "the SSH signature is not valid base64".to_owned())
}

/// Returns the type and the decoded blob of a public key line.
fn parse_public_key(key: &str) -> Result<(&str, Vec<u8>), String> {
    let mut fields = key.split_whitespace();
    let (Some(key_type), Some(blob)) = (fields.next(), fields.next()) else {
        return Err(
            "the key is not a public key line such as `sk-ssh-ed25519@openssh.com AAAA...`"
                .to_owned(),
        );
    };
    if !matches!(
        key_type,
        "sk-ssh-ed25519@openssh.com" | "sk-ecdsa-sha2-nistp256@openssh.com"
    ) {
        return Err(format!(
            "the allowed key is a `{key_type}` key, not a FIDO2 key, so it cannot prove a touch or a PIN"
        ));
    }
    let blob = base64(blob).ok_or("the allowed key is not valid base64")?;
    Ok((key_type, blob))
}

/// What touchgate reads of an SSHSIG signature, the format of
/// `PROTOCOL.sshsig` in OpenSSH.
struct Signature {
    public_key: Vec<u8>,
    namespace: Vec<u8>,
    /// The authenticator flags, which only a FIDO2 key's signature carries.
    flags: Option<u8>,
}

impl Signature {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let invalid = || "the SSH signature is malformed".to_owned();
        let mut reader = Reader(bytes);
        if reader.take(6).ok_or_else(invalid)? != b"SSHSIG"
            || reader.u32().ok_or_else(invalid)? != 1
        {
            return Err(invalid());
        }
        let public_key = reader.string().ok_or_else(invalid)?.to_vec();
        let namespace = reader.string().ok_or_else(invalid)?.to_vec();
        let _reserved = reader.string().ok_or_else(invalid)?;
        let _hash = reader.string().ok_or_else(invalid)?;
        let mut signature = Reader(reader.string().ok_or_else(invalid)?);
        if !reader.0.is_empty() {
            return Err(invalid());
        }
        // A FIDO2 signature is the type, the signature, then the flags byte
        // and a counter. Others end after the signature.
        let signature_type = signature.string().ok_or_else(invalid)?;
        let _raw = signature.string().ok_or_else(invalid)?;
        let flags = if signature_type.starts_with(b"sk-") {
            let flags = signature.take(1).ok_or_else(invalid)?[0];
            let _counter = signature.u32().ok_or_else(invalid)?;
            Some(flags)
        } else {
            None
        };
        if !signature.0.is_empty() {
            return Err(invalid());
        }
        Ok(Self {
            public_key,
            namespace,
            flags,
        })
    }
}

/// Reads the SSH wire format: big-endian `uint32`s and length-prefixed
/// strings.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.0.len() < len {
            return None;
        }
        let (taken, rest) = self.0.split_at(len);
        self.0 = rest;
        Some(taken)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn string(&mut self) -> Option<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }
}

/// Decodes standard base64 with padding.
fn base64(text: &str) -> Option<Vec<u8>> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.chunks(4).enumerate() {
        let last = index == bytes.len() / 4 - 1;
        let padding = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if padding > 2 || (padding > 0 && !last) {
            return None;
        }
        let mut word = 0u32;
        for &c in &chunk[..4 - padding] {
            word = word << 6 | u32::from(value(c)?);
        }
        word <<= 6 * padding as u32;
        let decoded = word.to_be_bytes();
        out.extend_from_slice(&decoded[1..4 - padding]);
    }
    Some(out)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn string(bytes: &[u8]) -> Vec<u8> {
        let mut out = (bytes.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(bytes);
        out
    }

    fn sshsig(signature: &[u8]) -> Vec<u8> {
        let mut out = b"SSHSIG".to_vec();
        out.extend_from_slice(&1u32.to_be_bytes());
        for field in [&b"key"[..], b"git", b"", b"sha512", signature] {
            out.extend(string(field));
        }
        out
    }

    #[test]
    fn reads_the_flags_of_a_fido2_signature() {
        let mut fido = string(b"sk-ssh-ed25519@openssh.com");
        fido.extend(string(&[0; 64]));
        fido.push(USER_PRESENT | USER_VERIFIED);
        fido.extend_from_slice(&7u32.to_be_bytes());
        let parsed = Signature::parse(&sshsig(&fido)).unwrap();
        assert_eq!(
            (parsed.public_key.as_slice(), parsed.namespace.as_slice()),
            (&b"key"[..], &b"git"[..])
        );
        assert_eq!(parsed.flags, Some(0x05));

        let mut plain = string(b"ssh-ed25519");
        plain.extend(string(&[0; 64]));
        assert_eq!(Signature::parse(&sshsig(&plain)).unwrap().flags, None);

        fido.push(0);
        assert!(Signature::parse(&sshsig(&fido)).is_err());
    }

    #[test]
    fn ages() {
        assert_eq!(parse_age("1h"), Ok(3_600));
        assert_eq!(parse_age("30m"), Ok(1_800));
        assert_eq!(parse_age("7d"), Ok(604_800));
        for invalid in [
            "",
            "h",
            "0m",
            "31d",
            "1",
            "1w",
            "-1h",
            "+1h",
            "99999999999999999d",
        ] {
            assert!(parse_age(invalid).is_err(), "{invalid}");
        }
    }

    fn approval() -> Approval {
        Approval {
            repository: "borink-org/touchgate".to_owned(),
            commit: "a".repeat(40),
            version: Version::parse("0.4.0").unwrap(),
            workflow_commit: "b".repeat(40),
            time: 1_793_000_000,
        }
    }

    const SIGNATURE: &str = "-----BEGIN SSH SIGNATURE-----\nU1NI\n-----END SSH SIGNATURE-----";

    #[test]
    fn wants_the_release_dated_the_day_of_the_approval() {
        let version = Version::parse("0.4.0").unwrap();
        let date = Date::parse("2026-11-01").unwrap();
        // 2026-11-01 at 00:00 and at 23:59 UTC, and 2026-11-02 at 00:30.
        let (start, end, after_midnight) = (1_793_491_200, 1_793_577_599, 1_793_579_400);
        for time in [start, end, after_midnight] {
            assert_eq!(check_release_date(&version, date, time), Ok(()), "{time}");
        }
        let late = check_release_date(&version, date, start + 2 * 86_400).unwrap_err();
        assert!(
            late.contains("2026-11-01, before the approval on 2026-11-03"),
            "{late}"
        );
        let early = check_release_date(&version, date, start - 1).unwrap_err();
        assert!(
            early.contains("after the approval on 2026-10-31"),
            "{early}"
        );
    }

    #[test]
    fn reads_the_text_it_writes() {
        let message = approval().message();
        assert_eq!(
            message,
            format!(
                "touchgate release approval\nrepository: borink-org/touchgate\ncommit: {}\n\
                 version: 0.4.0\nworkflow-commit: {}\ntime: 1793000000\n",
                "a".repeat(40),
                "b".repeat(40)
            )
        );
        let text = format!("{message}{SIGNATURE}\n");
        let signed = SignedApproval::parse(&text).unwrap();
        assert_eq!(signed.approval, approval());
        assert_eq!(signed.message, message);
        assert_eq!(signed.armored, SIGNATURE);
        assert_eq!(signed.text(), text);
        assert_eq!(base64("U1NIU0lH").unwrap(), b"SSHSIG");
        assert_eq!(base64("YQ==").unwrap(), b"a");
    }

    #[test]
    fn reads_nothing_but_the_message_it_writes() {
        let message = approval().message();
        for bad in [
            message.replace("touchgate release approval", "Approve release 0.4.0."),
            message.replace("repository: ", "repository:"),
            message.replace("borink-org/touchgate", "borink-org"),
            message.replace(&"a".repeat(40), "aaaa"),
            message.replace("version: 0.4.0", "version: 4"),
            message.replace("1793000000", "+1793000000"),
            message.replace("time: 1793000000\n", "time: 1793000000\nextra: 1\n"),
            message.replace("time: 1793000000\n", "time: 1793000000"),
            // The fields in another order.
            message.replace(
                &format!("commit: {}\nversion: 0.4.0\n", "a".repeat(40)),
                &format!("version: 0.4.0\ncommit: {}\n", "a".repeat(40)),
            ),
        ] {
            assert!(Approval::parse_message(&bad).is_err(), "{bad}");
        }
        assert!(SignedApproval::parse(&message).is_err());
    }
}
