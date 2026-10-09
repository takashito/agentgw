//! Files handed between agentgw processes, through the gateway.
//!
//! **The content never rides the link.** It goes up with `POST /files` and down with
//! `GET /files/<id>` on the gateway's HTTP port — a connection of its own, so a big file can't hold
//! up the Slack events on the link. The link only carries a short `FileReady` ("come and get it").
//! **The receiver decides where a file goes, by its kind** — a sender never names a path.

use crate::state_dir::StateDir;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// An agentgw build for one kind of machine (`meta`: `triple`, `build`).
pub const BUILD: &str = "agentgw-build";
/// How long the gateway keeps a file nobody is using.
pub const KEEP_MS: u64 = 3600 * 1000;

/// The kinds a receiver knows what to do with. Anything else is refused.
pub fn known(kind: &str) -> bool {
    kind == BUILD
}

/// What a sender says about a file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Head {
    pub kind: String,
    pub from: String,
    /// Who it is for. Empty = the gateway.
    pub to: String,
    pub meta: serde_json::Value,
    pub sha256: String,
}

/// One file the gateway holds, as `files/<id>.json` records it.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub meta: serde_json::Value,
    pub size: u64,
    pub sha256: String,
    #[serde(rename = "createdMs")]
    pub created_ms: u64,
}

/// The gateway's `files/` folder.
pub struct Store {
    dir: PathBuf,
}

/// Ids are ours: hex only, so one can never name a path outside `files/`.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Written 600: the folder sits in the state directory, beside the tokens.
fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
}

impl Store {
    pub fn at(dir: &StateDir) -> Self {
        Store { dir: dir.join("files") }
    }

    /// Takes the body in as it streams, hashing as it goes. **Nothing is kept unless the checksum
    /// matches**; a half-written `.part` is removed on any failure.
    pub async fn put<S, E>(&self, head: Head, mut body: S) -> Result<Entry, String>
    where
        S: futures_util::Stream<Item = Result<axum::body::Bytes, E>> + Unpin,
        E: std::fmt::Display,
    {
        if !known(&head.kind) {
            return Err(format!("unknown kind {:?}", head.kind));
        }
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| format!("cannot make {}: {e}", self.dir.display()))?;
        let id = crate::state_dir::mint_secret()[..16].to_string();
        let part = self.dir.join(format!("{id}.part"));
        let record = self.dir.join(format!("{id}.json"));
        let result = async {
            use tokio::io::AsyncWriteExt;
            let file = open_private(&part).map_err(|e| format!("cannot write {}: {e}", part.display()))?;
            let mut file = tokio::fs::File::from_std(file);
            let (mut hasher, mut size) = (Sha256::new(), 0u64);
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(|e| format!("the upload broke off: {e}"))?;
                hasher.update(&chunk);
                size += chunk.len() as u64;
                file.write_all(&chunk).await.map_err(|e| format!("cannot write: {e}"))?;
            }
            file.flush().await.map_err(|e| format!("cannot write: {e}"))?;
            let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
            if got != head.sha256.to_ascii_lowercase() {
                return Err(format!("checksum mismatch (expected {}, got {got})", head.sha256));
            }
            let entry = Entry {
                id: id.clone(),
                kind: head.kind.clone(),
                from: head.from.clone(),
                to: head.to.clone(),
                meta: head.meta.clone(),
                size,
                sha256: got,
                created_ms: crate::clock::now_ms(),
            };
            let json = serde_json::to_vec(&entry).map_err(|e| e.to_string())?;
            {
                use std::io::Write;
                open_private(&record)
                    .and_then(|mut f| f.write_all(&json))
                    .map_err(|e| format!("cannot write: {e}"))?;
            }
            std::fs::rename(&part, self.dir.join(&id)).map_err(|e| format!("cannot keep the file: {e}"))?;
            Ok(entry)
        }
        .await;
        if result.is_err() {
            let _ = std::fs::remove_file(&part);
            let _ = std::fs::remove_file(&record);
        }
        result
    }

    /// A held file and where it is. `None` for an id that isn't ours or isn't here.
    pub fn get(&self, id: &str) -> Option<(Entry, PathBuf)> {
        if !valid_id(id) {
            return None;
        }
        let path = self.dir.join(id);
        let record = std::fs::read(self.dir.join(format!("{id}.json"))).ok()?;
        let entry: Entry = serde_json::from_slice(&record).ok()?;
        path.is_file().then_some((entry, path))
    }

    pub fn remove(&self, id: &str) {
        if valid_id(id) {
            let _ = std::fs::remove_file(self.dir.join(id));
            let _ = std::fs::remove_file(self.dir.join(format!("{id}.json")));
        }
    }

    /// Drops what nobody needs: half uploads, records without a file, and files older than
    /// [`KEEP_MS`] that `keep` doesn't name (a rollout in progress names the builds it hands out).
    pub fn sweep(&self, now_ms: u64, keep: &[String]) {
        let Ok(dir) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for f in dir.flatten() {
            let name = f.file_name().to_string_lossy().to_string();
            if name.ends_with(".part") {
                let _ = std::fs::remove_file(f.path());
                continue;
            }
            let id = name.strip_suffix(".json").unwrap_or(&name).to_string();
            if keep.contains(&id) {
                continue;
            }
            let stale = self
                .get(&id)
                .is_none_or(|(e, _)| now_ms.saturating_sub(e.created_ms) > KEEP_MS);
            if stale {
                if valid_id(&id) {
                    self.remove(&id);
                } else {
                    let _ = std::fs::remove_file(f.path());
                }
            }
        }
    }
}

/// Where this process reaches the gateway's HTTP, and the key it shows. On the gateway: its own
/// link port on loopback. On a machine: the base of the URL it dials (whatever carries the link
/// carries this too — `update` already relies on it).
pub fn base_of(env: &HashMap<String, String>, access: &crate::bridge::state::Access) -> Option<(String, String)> {
    let token = env.get("AGENTGW_LINK_TOKEN").filter(|t| !t.is_empty())?.clone();
    let role = env.get("AGENTGW_BRIDGE_ROLE").map(|r| r.trim().to_lowercase());
    if role.as_deref() == Some("machine") {
        let url = access
            .gateway
            .as_ref()
            .map(|g| g.link_url.clone())
            .filter(|u| !u.is_empty())?;
        return Some((crate::setup::add_machine::probe_base(&url), token));
    }
    let port = crate::bridge::machine::link_port(|k| env.get(k).cloned());
    Some((format!("http://127.0.0.1:{port}"), token))
}

async fn curl(args: &[&str]) -> Result<Vec<u8>, String> {
    let out = tokio::process::Command::new("curl")
        .args(args)
        .output()
        .await
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if said.is_empty() { err } else { said });
    }
    Ok(out.stdout)
}

/// Sends a file up to the gateway. Returns its id. The checksum is taken here; `head.sha256` is ignored.
pub async fn upload(base: &str, token: &str, path: &Path, head: &Head) -> Result<String, String> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let sum = crate::setup::update::sha256_hex(&bytes);
    drop(bytes);
    let file = format!("@{}", path.display());
    let url = format!("{base}/files");
    let out = curl(&[
        "-sS",
        "--fail-with-body",
        "-X",
        "POST",
        "-H",
        "content-type: application/octet-stream",
        "--data-binary",
        &file,
        "-H",
        &format!("x-api-token: {token}"),
        "-H",
        &format!("x-agentgw-kind: {}", head.kind),
        "-H",
        &format!("x-agentgw-from: {}", head.from),
        "-H",
        &format!("x-agentgw-to: {}", head.to),
        "-H",
        &format!("x-agentgw-meta: {}", head.meta),
        "-H",
        &format!("x-agentgw-sha256: {sum}"),
        &url,
    ])
    .await?;
    let said = String::from_utf8_lossy(&out).to_string();
    serde_json::from_slice::<serde_json::Value>(&out)
        .ok()
        .and_then(|v| v["id"].as_str().map(str::to_string))
        .ok_or(said)
}

/// Fetches a file the gateway holds into `out`.
pub async fn download(base: &str, token: &str, id: &str, out: &Path) -> Result<(), String> {
    let out_s = out.to_string_lossy().to_string();
    let url = format!("{base}/files/{id}");
    curl(&["-sS", "-f", "-o", &out_s, "-H", &format!("x-api-token: {token}"), &url])
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> (Store, StateDir) {
        let dir = StateDir::at(
            std::env::temp_dir().join(format!("agentgw-files-{name}-{}", std::process::id())),
        );
        let _ = std::fs::remove_dir_all(dir.path());
        std::fs::create_dir_all(dir.path()).unwrap();
        (Store::at(&dir), dir)
    }

    fn head(sum: &str) -> Head {
        Head {
            kind: BUILD.into(),
            from: "mac".into(),
            to: String::new(),
            meta: serde_json::json!({}),
            sha256: sum.into(),
        }
    }

    fn body(
        parts: &[&'static [u8]],
    ) -> impl futures_util::Stream<Item = Result<axum::body::Bytes, std::io::Error>> + Unpin {
        futures_util::stream::iter(
            parts
                .iter()
                .map(|p| Ok(axum::body::Bytes::from_static(p)))
                .collect::<Vec<_>>(),
        )
    }

    #[tokio::test]
    async fn what_goes_in_comes_out() {
        let (s, dir) = store("roundtrip");
        let sum = crate::setup::update::sha256_hex(b"hello world");
        let e = s.put(head(&sum), body(&[b"hello ", b"world"])).await.unwrap();
        assert_eq!(e.size, 11);
        let (got, path) = s.get(&e.id).unwrap();
        assert_eq!(got.from, "mac");
        assert_eq!(std::fs::read(path).unwrap(), b"hello world");
        let _ = std::fs::remove_dir_all(dir.path());
    }

    #[tokio::test]
    async fn a_wrong_checksum_leaves_nothing_behind() {
        let (s, dir) = store("badsum");
        let err = s.put(head(&"0".repeat(64)), body(&[b"x"])).await.unwrap_err();
        assert!(err.contains("checksum"), "{err}");
        assert_eq!(std::fs::read_dir(dir.join("files")).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(dir.path());
    }

    #[tokio::test]
    async fn an_unknown_kind_is_refused() {
        let (s, dir) = store("kind");
        let mut h = head(&crate::setup::update::sha256_hex(b"x"));
        h.kind = "skill".into();
        assert!(s.put(h, body(&[b"x"])).await.is_err());
        let _ = std::fs::remove_dir_all(dir.path());
    }

    #[test]
    fn an_id_cannot_leave_the_files_folder() {
        let (s, dir) = store("escape");
        std::fs::write(dir.join("access.json"), "{}").unwrap();
        for id in ["../access.json", "..", "a/b", ""] {
            assert!(s.get(id).is_none(), "{id}");
        }
        let _ = std::fs::remove_dir_all(dir.path());
    }

    #[tokio::test]
    async fn the_start_up_sweep_keeps_what_a_rollout_points_at() {
        let (s, dir) = store("sweep");
        let sum = crate::setup::update::sha256_hex(b"x");
        let keep = s.put(head(&sum), body(&[b"x"])).await.unwrap();
        let old = s.put(head(&sum), body(&[b"x"])).await.unwrap();
        std::fs::write(dir.join("files").join("stray.part"), b"half").unwrap();
        std::fs::write(dir.join("files").join("0123abcd.json"), b"{}").unwrap();
        // Two hours on: both are old, only the one in use stays
        s.sweep(keep.created_ms + 2 * KEEP_MS, std::slice::from_ref(&keep.id));
        assert!(s.get(&keep.id).is_some());
        assert!(s.get(&old.id).is_none());
        assert!(!dir.join("files").join("stray.part").exists());
        assert!(!dir.join("files").join("0123abcd.json").exists(), "a record without its file");
        let _ = std::fs::remove_dir_all(dir.path());
    }

    #[tokio::test]
    async fn a_fresh_file_survives_the_sweep() {
        let (s, dir) = store("fresh");
        let sum = crate::setup::update::sha256_hex(b"x");
        let e = s.put(head(&sum), body(&[b"x"])).await.unwrap();
        s.sweep(e.created_ms + 1000, &[]);
        assert!(s.get(&e.id).is_some());
        let _ = std::fs::remove_dir_all(dir.path());
    }
}
