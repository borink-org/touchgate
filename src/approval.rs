//! The check that a person approved a release with a hardware key, by touch
//! and PIN.
//!
//! A release is a merge commit on the release branch. Its second parent is the
//! approval: a commit whose subject starts with `Approve release`, signed with
//! a FIDO2 SSH key (`sk-ssh-ed25519` or `sk-ecdsa-sha2-nistp256`), and with the
//! same tree as the merge. So the signature covers exactly what is released.
//!
//! `ssh-keygen -Y verify` checks the signature against the one key allowed.
//! It does not check how the signature was made: the allowed-signers format
//! has no option that demands a touch or a PIN, and a FIDO2 key asked for a
//! silent assertion signs without either. The authenticator signs a flags
//! byte along with the message, so this module reads that byte and requires
//! user presence and user verification.
//!
//! Nothing here runs code from the commits it checks, because until the
//! check passes the tree is untrusted, and anything that builds it, or runs
//! `cargo` inside it, could fake a pass. Commits are read as raw objects with
//! `git cat-file`, and parsed here, rather than through commands whose output
//! configuration can change. Replacement objects are off, since a
//! `refs/replace/` ref would make git answer for one commit with another, and
//! so are the user's and the system's git configuration.
//!
//! An approval expires: its committer time, which the signature covers, must
//! be recent. Otherwise an approval of a tree that was later abandoned, say
//! for a bug found after approving, could be merged and released by anyone
//! who can push.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// The authenticator flag for user presence: the key was touched.
const USER_PRESENT: u8 = 0x01;
/// The authenticator flag for user verification: the PIN was entered.
const USER_VERIFIED: u8 = 0x04;

/// The namespace git signs commits in.
const NAMESPACE: &str = "git";

/// The principal the one allowed key is listed under. Only this module reads
/// the allowed-signers file, so the name is arbitrary.
const PRINCIPAL: &str = "touchgate";

/// How far an approval's committer time may lie ahead of the clock, for a
/// signing machine whose clock runs a little fast.
const CLOCK_SKEW_SECONDS: i64 = 300;

/// Checks that `commit` in the repository at `repo` is a release that `key`
/// approved at most `max_age_seconds` ago, and returns the approval commit.
/// `key` is a public key as `ssh-keygen` writes it,
/// `sk-ssh-ed25519@openssh.com AAAA... comment`. `branch` is the ref the
/// release must be on, such as `origin/main`.
pub fn verify(
    repo: &Path,
    commit: &str,
    key: &str,
    branch: &str,
    max_age_seconds: i64,
) -> Result<String, String> {
    let (key_type, key_blob) = parse_public_key(key)?;
    let hash_len = commit.len();
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

    let merge = RawCommit::read(repo, commit)?;
    let [_, approval] = &merge.parents[..] else {
        return Err(format!(
            "{commit} is not a merge of two parents, so it has no approval as its second parent"
        ));
    };
    let approval = approval.clone();
    let approved = RawCommit::read(repo, &approval)?;
    if merge.tree != approved.tree {
        return Err(format!(
            "{commit} has another tree than the approval {approval}, so the merge brought in \
             changes that were not approved. Bring the release branch up to date and approve again"
        ));
    }
    if !approved.subject.starts_with("Approve release") {
        return Err(format!(
            "the second parent {approval} is `{}`, not an approval starting with `Approve release`",
            approved.subject
        ));
    }

    let header = if hash_len == 64 {
        "gpgsig-sha256"
    } else {
        "gpgsig"
    };
    let (payload, armored) = split_signature(&approved.raw, header)
        .ok_or_else(|| format!("the approval {approval} is not signed"))?;

    let signature = Signature::parse(&decode_armor(&armored)?)?;
    if signature.public_key != key_blob {
        return Err(format!(
            "the approval {approval} is signed by another key than the one allowed"
        ));
    }
    if signature.namespace != NAMESPACE.as_bytes() {
        return Err(format!(
            "the approval {approval} is not signed in the `git` namespace"
        ));
    }
    let Some(flags) = signature.flags else {
        return Err(format!(
            "the approval {approval} is signed with a key that is not a FIDO2 key, so it carries no touch or PIN"
        ));
    };
    if flags & USER_PRESENT == 0 {
        return Err(format!(
            "the approval {approval} was signed without a touch of the key"
        ));
    }
    if flags & USER_VERIFIED == 0 {
        return Err(format!(
            "the approval {approval} was signed without the key's PIN"
        ));
    }

    ssh_keygen_verify(key_type, key, &payload, &armored)
        .map_err(|error| format!("the signature of the approval {approval} is invalid: {error}"))?;

    // Only now is the committer time known to be the signer's.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_secs() as i64;
    let age = now - approved.committer_time;
    if age > max_age_seconds {
        return Err(format!(
            "the approval {approval} is {} old, older than the {} an approval lasts. Approve again",
            describe(age),
            describe(max_age_seconds)
        ));
    }
    if age < -CLOCK_SKEW_SECONDS {
        return Err(format!(
            "the approval {approval} is dated {} seconds in the future",
            -age
        ));
    }
    Ok(approval)
}

/// Reads how long an approval lasts, such as `30m`, `1h` or `7d`, in seconds.
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

/// A number of seconds in the unit that reads best: minutes under two hours,
/// hours under two days, days beyond.
fn describe(seconds: i64) -> String {
    match seconds {
        ..7_200 => format!("{} minutes", seconds / 60),
        7_200..172_800 => format!("{} hours", seconds / 3_600),
        _ => format!("{} days", seconds / 86_400),
    }
}

/// Whether `text` is a full SHA-1 or SHA-256 commit hash.
fn is_hash(text: &str) -> bool {
    (text.len() == 40 || text.len() == 64)
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// git, with what could make it answer for another object turned off.
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

/// What touchgate reads of a commit object.
struct RawCommit {
    /// The object as `git cat-file` prints it, which is what a signature
    /// covers once its own header is taken out.
    raw: Vec<u8>,
    tree: String,
    parents: Vec<String>,
    /// Seconds since the Unix epoch.
    committer_time: i64,
    /// The first line of the message.
    subject: String,
}

impl RawCommit {
    fn read(repo: &Path, hash: &str) -> Result<Self, String> {
        let output = git(repo)
            .args(["cat-file", "commit", hash])
            .output()
            .map_err(|error| format!("git: {error}"))?;
        if !output.status.success() {
            return Err(format!("git cannot read the commit {hash}"));
        }
        let raw = output.stdout;
        let malformed = || format!("the commit {hash} is malformed");
        let text =
            std::str::from_utf8(&raw).map_err(|_| format!("the commit {hash} is not UTF-8"))?;
        let (header, message) = text.split_once("\n\n").ok_or_else(malformed)?;
        let mut tree = None;
        let mut parents = Vec::new();
        let mut committer_time = None;
        for line in header.lines() {
            if let Some(value) = line.strip_prefix("tree ") {
                if tree.replace(value.to_owned()).is_some() || !is_hash(value) {
                    return Err(malformed());
                }
            } else if let Some(value) = line.strip_prefix("parent ") {
                if !is_hash(value) {
                    return Err(malformed());
                }
                parents.push(value.to_owned());
            } else if let Some(value) = line.strip_prefix("committer ") {
                // `Name <email> 1790000000 +0000`: the time is the second
                // field from the end.
                let time = value
                    .rsplit(' ')
                    .nth(1)
                    .and_then(|time| time.parse::<i64>().ok())
                    .ok_or_else(malformed)?;
                if committer_time.replace(time).is_some() {
                    return Err(malformed());
                }
            }
        }
        Ok(Self {
            tree: tree.ok_or_else(malformed)?,
            parents,
            committer_time: committer_time.ok_or_else(malformed)?,
            subject: message.lines().next().unwrap_or_default().to_owned(),
            raw,
        })
    }
}

/// Runs `ssh-keygen -Y verify` with `key` as the one allowed signer.
fn ssh_keygen_verify(
    key_type: &str,
    key: &str,
    payload: &[u8],
    armored: &str,
) -> Result<(), String> {
    let dir = fresh_dir()?;
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

/// A directory no one else made: creating it fails if the name is taken, so
/// nothing placed there beforehand is read as the allowed signers.
fn fresh_dir() -> Result<PathBuf, String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("touchgate-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    Ok(dir)
}

/// Splits a raw commit into what was signed and the armored signature. The
/// signed payload is the commit without the signature header and its
/// continuation lines, which start with a space.
fn split_signature(raw: &[u8], header: &str) -> Option<(Vec<u8>, String)> {
    let prefix = format!("{header} ");
    let mut payload = Vec::with_capacity(raw.len());
    let mut armored: Option<String> = None;
    let mut in_signature = false;
    let mut in_header = true;
    for line in raw.split_inclusive(|&b| b == b'\n') {
        if in_header {
            if line == b"\n" {
                in_header = false;
            } else if in_signature && line.starts_with(b" ") {
                let text = std::str::from_utf8(&line[1..]).ok()?;
                armored.as_mut()?.push_str(text);
                continue;
            } else if let Some(first) = line.strip_prefix(prefix.as_bytes()) {
                if armored.is_some() {
                    return None;
                }
                armored = Some(std::str::from_utf8(first).ok()?.to_owned());
                in_signature = true;
                continue;
            }
            in_signature = false;
        }
        payload.extend_from_slice(line);
    }
    Some((payload, armored?.trim_end().to_owned()))
}

/// The bytes inside `-----BEGIN SSH SIGNATURE-----`.
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

/// The type and the decoded blob of a public key line.
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

    #[test]
    fn splits_the_signature_from_the_payload() {
        let raw = b"tree t\nparent p\nauthor a\ncommitter c\ngpgsig -----BEGIN SSH SIGNATURE-----\n U1NI\n -----END SSH SIGNATURE-----\n\nApprove release 0.1.0.\n";
        let (payload, armored) = split_signature(raw, "gpgsig").unwrap();
        assert_eq!(
            payload,
            b"tree t\nparent p\nauthor a\ncommitter c\n\nApprove release 0.1.0.\n"
        );
        assert_eq!(
            armored,
            "-----BEGIN SSH SIGNATURE-----\nU1NI\n-----END SSH SIGNATURE-----"
        );
        assert_eq!(base64("U1NIU0lH").unwrap(), b"SSHSIG");
        assert_eq!(base64("YQ==").unwrap(), b"a");
    }
}
