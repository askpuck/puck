//! Puck Cloud: sync del vault con uno spazio privato (Supabase oggi).
//!
//! Protocollo whole-project (single writer, un utente): il vault è la verità;
//! un solo manifest JSON descrive tutti i file (hash + versione).
//! - all'avvio: `prepare` → "Preparing workspace" → pull se il remoto è più nuovo.
//! - a inizio task: `check` → confronto rapido di versione; pull se diverso.
//! - a fine lavoro / chiusura app: `push` → carica i file cambiati, manifest v+1.
//! Rimozioni: il manifest è la verità — un file che non c'è più nel manifest
//! viene rimosso dal locale al pull (mirror), e al push i file spariti vengono
//! cancellati dal remoto.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex as StdMutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::coordinatore::env_puck;
use tauri::{Emitter, Manager};

const MANIFEST_KEY: &str = "manifest.json";
const STORE_DIR: &str = ".puck-cloud";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMeta {
    pub sha: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Manifest {
    pub version: u64,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub files: BTreeMap<String, FileMeta>,
}

/// Backend di storage: 3 verbi. Il protocollo è generico su questo.
pub trait Store {
    fn get(&self, key: &str) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, String>> + Send;
    fn put(&self, key: &str, bytes: &[u8]) -> impl std::future::Future<Output = Result<(), String>> + Send;
    fn delete(&self, key: &str) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

pub struct Cloud {
    pub url: String,
    pub anon: String,
}

impl Cloud {
    pub fn from_env() -> Option<Cloud> {
        let url = env_puck("PUCK_SUPABASE_URL")?;
        let anon = env_puck("PUCK_SUPABASE_ANON_KEY")?;
        let url = url.trim().trim_end_matches('/').to_string();
        if url.is_empty() || anon.trim().is_empty() {
            return None;
        }
        Some(Cloud {
            url,
            anon: anon.trim().to_string(),
        })
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        let mut m = reqwest::header::HeaderMap::new();
        if let Ok(v) = reqwest::header::HeaderValue::from_str(&self.anon) {
            m.insert("apikey", v);
        }
        if let Some(sess) = session() {
            let bearer = format!("Bearer {}", sess.access_token);
            if let Ok(v) = reqwest::header::HeaderValue::from_str(&bearer) {
                m.insert(reqwest::header::AUTHORIZATION, v);
            }
        } else if let Ok(v) = reqwest::header::HeaderValue::from_str(&self.anon) {
            m.insert(reqwest::header::AUTHORIZATION, v);
        }
        m
    }

    /// Oggetti per-utente: `{uid}/<key>` quando loggato (RLS auth.uid),
    /// altrimenti key nuda (dev/single-tenant).
    fn key_for(&self, key: &str) -> String {
        let k = key.trim_start_matches('/');
        match session().map(|s| s.uid) {
            Some(uid) if !uid.is_empty() => format!("{uid}/{k}"),
            _ => k.to_string(),
        }
    }

    fn url_for(&self, key: &str) -> String {
        format!(
            "{}/storage/v1/object/puck/{}",
            self.url,
            self.key_for(key)
        )
    }
}

impl Store for Cloud {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let resp = reqwest::Client::new()
            .get(self.url_for(key))
            .headers(self.headers())
            .send()
            .await
            .map_err(|e| format!("cloud get {key}: {e}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!("cloud get {key}: HTTP {}", resp.status().as_u16()));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("cloud read {key}: {e}"))?;
        Ok(Some(bytes.to_vec()))
    }

    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), String> {
        let mut headers = self.headers();
        // POST crea; con x-upsert sovrascrive se esiste (update policy).
        if let Ok(v) = reqwest::header::HeaderValue::from_str("true") {
            headers.insert("x-upsert", v);
        }
        let resp = reqwest::Client::new()
            .post(self.url_for(key))
            .headers(headers)
            .body(bytes.to_vec())
            .send()
            .await
            .map_err(|e| format!("cloud put {key}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("cloud put {key}: HTTP {}", resp.status().as_u16()));
        }
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), String> {
        let resp = reqwest::Client::new()
            .delete(self.url_for(key))
            .headers(self.headers())
            .send()
            .await
            .map_err(|e| format!("cloud delete {key}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("cloud delete {key}: HTTP {}", resp.status().as_u16()));
        }
        Ok(())
    }
}

// ---- local state ----

fn cloud_dir(vault: &Path) -> Result<PathBuf, String> {
    let d = vault.join(STORE_DIR);
    fs::create_dir_all(&d).map_err(|e| format!("Cloud: {e}"))?;
    Ok(d)
}

fn local_manifest_path(vault: &Path) -> Result<PathBuf, String> {
    Ok(cloud_dir(vault)?.join("local.json"))
}

pub fn load_local_manifest(vault: &Path) -> Result<Option<Manifest>, String> {
    let p = local_manifest_path(vault)?;
    match fs::read_to_string(&p) {
        Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s)
            .map(Some)
            .map_err(|e| format!("Cloud local manifest: {e}")),
        _ => Ok(None),
    }
}

fn save_local_manifest(vault: &Path, m: &Manifest) -> Result<(), String> {
    let p = local_manifest_path(vault)?;
    fs::write(
        &p,
        serde_json::to_vec_pretty(m).map_err(|e| format!("Cloud manifest: {e}"))?,
    )
    .map_err(|e| format!("Cloud manifest: {e}"))
}

// ---- snapshot dei file del vault ----

fn sha256(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

fn keep_dir(name: &str) -> bool {
    name == ".git"
        || name == "node_modules"
        || name == "target"
        || name == ".puck-review"
        || name == STORE_DIR
        || (name.starts_with('.') && name != ".puck")
}

fn keep_file(rel: &str, name: &str) -> bool {
    if name == ".DS_Store" || name == ".env" || name == STORE_DIR {
        return false;
    }
    if rel.contains("/.git/")
        || rel.starts_with(".git/")
        || rel.contains("/node_modules/")
        || rel.contains("/target/")
        || rel.contains("/.puck-review/")
        || rel.starts_with(".puck-review/")
    {
        return false;
    }
    // .puck: solo memory.md e schema.md
    if rel.contains(".puck/") {
        let inner = rel.rsplit_once(".puck/").map(|(_, r)| r).unwrap_or("");
        return inner == "memory.md" || inner == "schema.md";
    }
    !name.starts_with('.')
}

/// File del vault con hash: la fonte di verità del push.
pub fn snapshot(vault: &Path) -> Result<BTreeMap<String, FileMeta>, String> {
    let mut out = BTreeMap::new();
    fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<String, FileMeta>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            let rel = p
                .strip_prefix(base)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            if p.is_dir() {
                if keep_dir(&name) {
                    continue;
                }
                walk(&p, base, out);
                continue;
            }
            if !keep_file(&rel, &name) {
                continue;
            }
            if let Ok(bytes) = fs::read(&p) {
                out.insert(
                    rel,
                    FileMeta {
                        sha: sha256(&bytes),
                        bytes: bytes.len() as u64,
                    },
                );
            }
        }
    }
    walk(vault, vault, &mut out);
    Ok(out)
}

fn rel_exists(vault: &Path, rel: &str) -> Option<PathBuf> {
    let p = vault.join(rel);
    p.is_file().then_some(p)
}

// ---- protocollo (generico sullo store) ----

fn diff_for_push(local: &BTreeMap<String, FileMeta>, last: &Manifest) -> Vec<String> {
    local
        .iter()
        .filter(|(rel, meta)| match last.files.get(*rel) {
            Some(m) if m.sha == meta.sha => false,
            _ => true,
        })
        .map(|(rel, _)| rel.clone())
        .collect()
}

pub async fn prepare<S: Store>(store: &S, vault: &Path) -> Result<Manifest, String> {
    let local = load_local_manifest(vault)?;
    match store.get(MANIFEST_KEY).await? {
        Some(bytes) => {
            let remote: Manifest =
                serde_json::from_slice(&bytes).map_err(|e| format!("Cloud manifest remoto: {e}"))?;
            let local_version = local.as_ref().map(|m| m.version).unwrap_or(0);
            if remote.version > local_version {
                mirror(store, vault, &remote).await?;
            }
            Ok(remote)
        }
        None => {
            // primo avvio connesso: segna il workspace come preparato
            // (manifest vuoto locale), così la UI esce da "Preparing…".
            match local {
                Some(m) => Ok(m),
                None => {
                    let m = Manifest::default();
                    let _ = save_local_manifest(vault, &m);
                    Ok(m)
                }
            }
        }
    }
}

pub async fn check<S: Store>(store: &S, vault: &Path) -> Result<Manifest, String> {
    prepare(store, vault).await
}

pub async fn push<S: Store>(store: &S, vault: &Path) -> Result<Manifest, String> {
    if !vault.is_dir() {
        return Err("Vault is missing.".into());
    }
    let current = snapshot(vault)?;
    let last = load_local_manifest(vault)?.unwrap_or_default();
    let changed = diff_for_push(&current, &last);
    for rel in &changed {
        if let Some(p) = rel_exists(vault, rel) {
            let bytes = fs::read(&p).map_err(|e| format!("Cloud read {rel}: {e}"))?;
            store.put(&format!("files/{rel}"), &bytes).await?;
        }
    }
    // rimozioni remote: file che esistevano nell'ultimo sync e non ci sono più
    for (rel, _) in &last.files {
        if !current.contains_key(rel) {
            let _ = store.delete(&format!("files/{rel}")).await;
        }
    }
    let manifest = Manifest {
        version: last.version + 1,
        updated: chrono::Utc::now().to_rfc3339(),
        files: current,
    };
    let bytes = serde_json::to_vec(&manifest).map_err(|e| format!("Cloud manifest: {e}"))?;
    store.put(MANIFEST_KEY, &bytes).await?;
    save_local_manifest(vault, &manifest)?;
    Ok(manifest)
}

async fn mirror<S: Store>(store: &S, vault: &Path, remote: &Manifest) -> Result<(), String> {
    let current = snapshot(vault)?;
    for (rel, meta) in &remote.files {
        let local_ok = current.get(rel).is_some_and(|m| m.sha == meta.sha);
        if local_ok {
            continue;
        }
        if let Some(bytes) = store.get(&format!("files/{rel}")).await? {
            let p = vault.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("Cloud pull {rel}: {e}"))?;
            }
            fs::write(&p, bytes).map_err(|e| format!("Cloud pull {rel}: {e}"))?;
        }
    }
    for rel in current.keys() {
        if !remote.files.contains_key(rel) {
            let _ = fs::remove_file(vault.join(rel));
        }
    }
    save_local_manifest(vault, remote)
}

// ---- auth: Puck Cloud login (Supabase GoTrue, magic link PKCE) ----

static SESSION: OnceLock<StdMutex<Option<Session>>> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub email: String,
    #[serde(default)]
    pub uid: String,
}

fn b64url(data: &[u8]) -> String {
    const AL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        out.push(AL[(b[0] >> 2) as usize] as char);
        out.push(AL[(((b[0] & 0x3) << 4) | (b[1] >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(AL[(((b[1] & 0xf) << 2) | (b[2] >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(AL[(b[2] & 0x3f) as usize] as char);
        }
    }
    out
}

fn pkce_verifier() -> Result<String, String> {
    let mut b = [0u8; 32];
    getrandom::getrandom(&mut b).map_err(|e| format!("random: {e}"))?;
    Ok(b64url(&b))
}

fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(verifier.as_bytes());
    b64url(&h.finalize())
}

fn redirect_url() -> String {
    env_puck("PUCK_SUPABASE_REDIRECT").unwrap_or_else(|| {
        if cfg!(debug_assertions) {
            "http://localhost:1420/auth".into()
        } else {
            // Supabase non accetta schemi custom nei redirect: passa dal ponte
            // https://askpuck.app/auth che inoltra a puck://auth.
            "https://askpuck.app/auth".into()
        }
    })
}

#[cfg(test)]
static TEST_FILE: StdMutex<Option<PathBuf>> = StdMutex::new(None);
#[cfg(test)]
static TEST_LOCK: StdMutex<()> = StdMutex::new(());

fn session_file() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(p) = TEST_FILE.lock().unwrap().clone() {
        return Some(p);
    }
    if let Some(p) = env_puck("PUCK_CLOUD_SESSION_FILE") {
        let p = p.trim();
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    vault_path().map(|v| v.join(STORE_DIR).join("session.json"))
}

#[cfg(target_os = "macos")]
fn keychain_store(json: &str) -> Result<(), String> {
    let st = std::process::Command::new("security")
        .args([
            "add-generic-password",
            "-a",
            "puck-cloud",
            "-s",
            "app.puck.desktop",
            "-w",
            json,
            "-U",
        ])
        .status()
        .map_err(|e| format!("keychain: {e}"))?;
    if st.success() {
        Ok(())
    } else {
        Err("keychain: add failed".into())
    }
}

#[cfg(target_os = "macos")]
fn keychain_load() -> Option<String> {
    let out = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-a",
            "puck-cloud",
            "-s",
            "app.puck.desktop",
            "-w",
        ])
        .output()
        .ok()?;
    if out.status.success() {
        String::from_utf8(out.stdout).ok().map(|s| s.trim().to_string())
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn keychain_clear() {
    let _ = std::process::Command::new("security")
        .args(["delete-generic-password", "-a", "puck-cloud", "-s", "app.puck.desktop"])
        .status();
}

fn test_file_override() -> bool {
    #[cfg(test)]
    {
        return TEST_FILE.lock().unwrap().is_some();
    }
    #[cfg(not(test))]
    {
        false
    }
}

fn load_session() -> Option<Session> {
    let raw = if env_puck("PUCK_CLOUD_SESSION_FILE").is_some() || test_file_override() {
        let p = session_file()?;
        fs::read_to_string(p).ok().map(|s| s.trim().to_string())
    } else {
        keychain_load().or_else(|| {
            let p = session_file()?;
            fs::read_to_string(p).ok().map(|s| s.trim().to_string())
        })
    }?;
    serde_json::from_str(&raw).ok()
}

fn save_session(sess: &Session) -> Result<(), String> {
    let raw = serde_json::to_string(sess).map_err(|e| format!("session: {e}"))?;
    let mut stored = false;
    if env_puck("PUCK_CLOUD_SESSION_FILE").is_none() && !test_file_override() {
        #[cfg(target_os = "macos")]
        {
            stored = keychain_store(&raw).is_ok();
        }
    }
    if let Some(p) = session_file() {
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(&p, raw.as_bytes()).map_err(|e| format!("session file: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o600));
        }
        stored = true;
    }
    if stored {
        Ok(())
    } else {
        Err("session: no storage available".into())
    }
}

fn clear_session() {
    if env_puck("PUCK_CLOUD_SESSION_FILE").is_none() && !test_file_override() {
        #[cfg(target_os = "macos")]
        keychain_clear();
    }
    if let Some(p) = session_file() {
        let _ = fs::remove_file(p);
    }
    if let Some(g) = SESSION.get() {
        if let Ok(mut s) = g.lock() {
            *s = None;
        }
    }
}

/// Sessione corrente (cache in memoria; letto una volta da Keychain/file).
pub fn session() -> Option<Session> {
    let g = SESSION.get_or_init(|| StdMutex::new(None));
    let mut s = g.lock().ok()?;
    if s.is_none() {
        *s = load_session();
    }
    s.clone()
}

/// Refresha la sessione se scaduta (da chiamare dai punti async, non dalla UI).
pub async fn refresh_if_needed() {
    let Some(sess) = session() else {
        return;
    };
    if sess.expires_at > 0 && sess.expires_at < chrono::Utc::now().timestamp() {
        if let Ok(fresh) = refresh_token(&sess.refresh_token).await {
            set_session(Some(fresh));
        }
    }
}

async fn refresh_token(refresh: &str) -> Result<Session, String> {
    let cloud = Cloud::from_env().ok_or("Puck Cloud non configurato.")?;
    let resp = reqwest::Client::new()
        .post(format!("{}/auth/v1/token", cloud.url))
        .query(&[("grant_type", "refresh_token")])
        .header("apikey", &cloud.anon)
        .header("content-type", "application/json")
        .json(&json!({"refresh_token": refresh}))
        .send()
        .await
        .map_err(|e| format!("cloud refresh: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("cloud refresh: HTTP {}", resp.status().as_u16()));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("cloud refresh json: {e}"))?;
    let sess = Session {
        access_token: v["access_token"].as_str().unwrap_or("").to_string(),
        refresh_token: v["refresh_token"].as_str().unwrap_or("").to_string(),
        expires_at: v["expires_at"].as_i64().unwrap_or(0),
        email: v["user"]["email"]
            .as_str()
            .map(ToOwned::to_owned)
            .unwrap_or_default(),
        uid: v["user"]["id"].as_str().unwrap_or("").to_string(),
    };
    if sess.access_token.is_empty() {
        return Err("Refresh senza access_token.".into());
    }
    let _ = save_session(&sess);
    crate::context::run_log("cloud_auth", json!({"phase": "refresh", "ok": true}));
    Ok(sess)
}

fn set_session(sess: Option<Session>) {
    if let Some(g) = SESSION.get() {
        if let Ok(mut s) = g.lock() {
            *s = sess;
        }
    }
}

#[tauri::command]
pub async fn cloud_connect(email: String) -> Result<Value, String> {
    let cloud = Cloud::from_env().ok_or(
        "Puck Cloud non configurato: PUCK_SUPABASE_URL / PUCK_SUPABASE_ANON_KEY nel .env (vedi cloud-setup.md).",
    )?;
    let email = email.trim().to_string();
    if !email.contains('@') || email.len() < 5 {
        return Err("Email non valida.".into());
    }
    let verifier = pkce_verifier()?;
    let challenge = pkce_challenge(&verifier);
    save_pending_verifier(&verifier)?;
    let body = json!({
        "email": email,
        "redirect_to": redirect_url(),
        "code_challenge": challenge,
        "code_challenge_method": "s256",
    });
    let resp = reqwest::Client::new()
        .post(format!("{}/auth/v1/magiclink", cloud.url))
        .header("apikey", &cloud.anon)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("cloud connect: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("cloud connect: HTTP {}", resp.status().as_u16()));
    }
    crate::context::run_log("cloud_auth", json!({"phase": "magiclink", "ok": true}));
    Ok(json!({"ok": true, "redirect": redirect_url(), "email": email}))
}

fn pending_path() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(p) = TEST_FILE.lock().unwrap().clone() {
        return p.parent().map(|d| d.join("pending-verifier"));
    }
    if let Some(p) = env_puck("PUCK_CLOUD_SESSION_FILE") {
        let p = p.trim();
        if !p.is_empty() {
            let p = PathBuf::from(p);
            return p.parent().map(|d| d.join("pending-verifier"));
        }
    }
    vault_path().map(|v| v.join(STORE_DIR).join("pending-verifier"))
}

fn save_pending_verifier(v: &str) -> Result<(), String> {
    let p = pending_path().ok_or("pending: no storage")?;
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("pending: {e}"))?;
    }
    fs::write(&p, v.as_bytes()).map_err(|e| format!("pending: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn take_pending_verifier() -> Option<String> {
    let p = pending_path()?;
    let v = fs::read_to_string(&p).ok()?;
    let _ = fs::remove_file(&p);
    Some(v.trim().to_string())
}

async fn exchange_code(code: &str) -> Result<Session, String> {
    let cloud = Cloud::from_env().ok_or("Puck Cloud non configurato.")?;
    let verifier = take_pending_verifier()
        .ok_or("Verifica non trovata: il link è scaduto, ripeti il login dal pulsante.")?;
    let resp = reqwest::Client::new()
        .post(format!("{}/auth/v1/token", cloud.url))
        .query(&[("grant_type", "pkce")])
        .header("apikey", &cloud.anon)
        .header("content-type", "application/json")
        .json(&json!({"auth_code": code.trim(), "code_verifier": verifier}))
        .send()
        .await
        .map_err(|e| format!("cloud callback: {e}"))?;
    let status = resp.status();
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("cloud callback json: {e}"))?;
    if !status.is_success() {
        let msg = v
            .get("error_description")
            .or_else(|| v.get("msg"))
            .and_then(Value::as_str)
            .unwrap_or("errore sconosciuto");
        return Err(format!("cloud callback: HTTP {} — {}", status.as_u16(), msg));
    }
    let sess = Session {
        access_token: v["access_token"].as_str().unwrap_or("").to_string(),
        refresh_token: v["refresh_token"].as_str().unwrap_or("").to_string(),
        expires_at: v["expires_at"].as_i64().unwrap_or(0),
        email: v["user"]["email"].as_str().unwrap_or("").to_string(),
        uid: v["user"]["id"].as_str().unwrap_or("").to_string(),
    };
    if sess.access_token.is_empty() {
        return Err("Risposta senza access_token.".into());
    }
    Ok(sess)
}

#[tauri::command]
pub async fn cloud_auth_callback(app: tauri::AppHandle, code: String) -> Result<Value, String> {
    if code.trim().is_empty() {
        return Err("Risposta di autenticazione incompleta.".into());
    }
    let sess = exchange_code(code.trim()).await?;
    save_session(&sess)?;
    set_session(Some(sess.clone()));
    crate::context::run_log("cloud_auth", json!({"phase": "callback", "ok": true}));
    // dopo il login: pull del proprio spazio.
    let _ = prepare_app(&app).await;
    Ok(json!({"ok": true, "email": sess.email}))
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Deep link: "puck://auth?code=…" (o con error=…). Restituisce l'email.
pub async fn auth_from_url(raw: &str) -> Result<String, String> {
    let mut code = None;
    let mut err = None;
    if let Some(query) = raw.split('?').nth(1) {
        for kv in query.split('&') {
            let mut it = kv.splitn(2, '=');
            let (k, v) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
            match k {
                "code" => code = Some(url_decode(v)),
                "error_description" | "error" => err = Some(url_decode(v)),
                _ => {}
            }
        }
    }
    if let Some(e) = err {
        return Err(format!("Login fallito: {e}"));
    }
    let code = code.ok_or("Deep link senza code.")?;
    let sess = exchange_code(&code).await?;
    save_session(&sess)?;
    set_session(Some(sess.clone()));
    crate::context::run_log(
        "cloud_auth",
        json!({"phase": "deeplink", "ok": true, "email": sess.email}),
    );
    Ok(sess.email)
}

pub async fn auth_from_app(app: &tauri::AppHandle, raw: &str) {
    let r = auth_from_url(&raw).await;
    match &r {
        Ok(email) => {
            // dopo il login: pull del proprio spazio.
            let _ = prepare_app(app).await;
            emit_pulse(app, "cloud_auth", &format!("Puck Cloud collegato a {email}."));
            let _ = app.emit(
                "puck-crew",
                json!({"kind": "cloud_auth", "role": "puck", "text": email}),
            );
        }
        Err(e) => {
            crate::context::run_log("cloud_auth", json!({"phase": "deeplink", "ok": false, "err": crate::context::clip_log(e, 180)}));
            let _ = app.emit(
                "puck-crew",
                json!({"kind": "cloud_auth", "role": "puck", "text": format!("login: {e}")}),
            );
        }
    }
}

#[tauri::command]
pub fn cloud_signout() -> Result<(), String> {
    clear_session();
    crate::context::run_log("cloud_auth", json!({"phase": "signout", "ok": true}));
    Ok(())
}

/// Retry dell'allineamento dalla UI (es. "Preparing…" bloccato).
#[tauri::command]
pub async fn cloud_refresh(app: tauri::AppHandle) -> Result<Value, String> {
    let _ = prepare_app(&app).await;
    Ok(cloud_status())
}

// ---- cavi applicativi ----

pub fn vault_path() -> Option<PathBuf> {
    crate::context::vault_root().ok()
}

/// Carica .env (dev: ../.env del repo; poi la cartella config dell'app:
/// su macOS `~/Library/Application Support/app.puck.desktop/.env`).
pub fn load_user_env(app: &tauri::AppHandle) {
    let from_crate = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.env");
    let _ = dotenvy::from_path(&from_crate);
    let _ = dotenvy::dotenv();
    if let Ok(dir) = app.path().app_config_dir() {
        let _ = dotenvy::from_path(dir.join(".env"));
    }
}

fn emit_pulse(app: &tauri::AppHandle, kind: &str, text: &str) {
    let _ = app.emit(
        "puck-crew",
        json!({
            "kind": kind,
            "role": "puck",
            "text": text,
        }),
    );
}

pub async fn prepare_app(app: &tauri::AppHandle) -> Result<(), String> {
    refresh_if_needed().await;
    let Some(cloud) = Cloud::from_env() else {
        emit_pulse(app, "cloud", "Preparing workspace… (cloud off)");
        crate::context::run_log("cloud", json!({"phase": "skip", "reason": "not_configured"}));
        return Ok(());
    };
    let Some(vault) = vault_path() else {
        return Err("Vault missing.".into());
    };
    emit_pulse(app, "cloud", "Preparing workspace…");
    let r = prepare(&cloud, &vault).await;
    crate::context::run_log(
        "cloud",
        json!({
            "phase": "prepare",
            "ok": r.is_ok(),
            "err": r.as_ref().err().map(|e| crate::context::clip_log(e, 160)),
        }),
    );
    if r.is_ok() {
        emit_pulse(app, "cloud", "Workspace ready");
    }
    r.map(|_| ())
}

pub async fn check_app(app: &tauri::AppHandle) -> Result<(), String> {
    refresh_if_needed().await;
    let Some(cloud) = Cloud::from_env() else {
        crate::context::run_log("cloud", json!({"phase": "check", "ok": true, "mode": "local"}));
        let _ = app;
        return Ok(());
    };
    let Some(vault) = vault_path() else {
        return Ok(());
    };
    let r = check(&cloud, &vault).await;
    crate::context::run_log(
        "cloud",
        json!({
            "phase": "check",
            "ok": r.is_ok(),
            "err": r.as_ref().err().map(|e| crate::context::clip_log(e, 160)),
        }),
    );
    let _ = app;
    r.map(|_| ())
}

pub async fn push_app(app: &tauri::AppHandle) -> Result<(), String> {
    refresh_if_needed().await;
    let Some(cloud) = Cloud::from_env() else {
        crate::context::run_log("cloud", json!({"phase": "push", "ok": true, "mode": "local"}));
        let _ = app;
        return Ok(());
    };
    let Some(vault) = vault_path() else {
        return Ok(());
    };
    emit_pulse(app, "cloud", "Pushing on cloud, don't close the app…");
    let r = push(&cloud, &vault).await;
    crate::context::run_log(
        "cloud",
        json!({
            "phase": "push",
            "ok": r.is_ok(),
            "err": r.as_ref().err().map(|e| crate::context::clip_log(e, 160)),
        }),
    );
    if r.is_ok() {
        emit_pulse(app, "cloud", "Pushed.");
    }
    r.map(|_| ())
}

pub fn push_now() -> Result<(), String> {
    let Some(cloud) = Cloud::from_env() else {
        return Ok(());
    };
    let Some(vault) = vault_path() else {
        return Ok(());
    };
    let r = tauri::async_runtime::block_on(push(&cloud, &vault));
    crate::context::run_log(
        "cloud",
        json!({
            "phase": "close",
            "ok": r.is_ok(),
            "err": r.as_ref().err().map(|e| crate::context::clip_log(e, 160)),
        }),
    );
    r.map(|_| ())
}

#[tauri::command]
pub fn cloud_status() -> Value {
    let configured = Cloud::from_env().is_some();
    let sess = session();
    let vault = vault_path().map(|p| p.display().to_string());
    let last = vault
        .as_deref()
        .and_then(|v| load_local_manifest(Path::new(v)).ok().flatten());
    json!({
        "configured": configured,
        "connected": configured && last.is_some(),
        "signed_in": sess.is_some(),
        "email": sess.as_ref().map(|s| s.email.clone()).unwrap_or_default(),
        "version": last.as_ref().map(|m| m.version).unwrap_or(0),
        "last": last.as_ref().map(|m| m.updated.clone()).unwrap_or_default(),
        "vault": vault,
    })
}

// ---- test (stesso identico protocollo, store in-memory) ----

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Default, Clone)]
    struct MockStore {
        map: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    }

    impl Store for MockStore {
        async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
            Ok(self.map.lock().unwrap().get(key).cloned())
        }
        async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), String> {
            self.map
                .lock()
                .unwrap()
                .insert(key.to_string(), bytes.to_vec());
            Ok(())
        }
        async fn delete(&self, key: &str) -> Result<(), String> {
            self.map.lock().unwrap().remove(key);
            Ok(())
        }
    }

    fn tmp_vault() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "puck-cloud-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn push_first_version_then_incremental() {
        let store = MockStore::default();
        let vault = tmp_vault();
        fs::create_dir_all(vault.join("prova/.puck")).unwrap();
        fs::write(vault.join("user.md"), "U1").unwrap();
        fs::write(vault.join("prova/index.html"), "<html>1</html>").unwrap();
        fs::write(vault.join("prova/.puck/memory.md"), "## What this is\n- [m-1] X\n").unwrap();

        let m1 = push(&store, &vault).await.unwrap();
        assert_eq!(m1.version, 1);
        assert_eq!(m1.files.len(), 3);
        assert!(store.map.lock().unwrap().contains_key(MANIFEST_KEY));
        assert!(store.map.lock().unwrap().contains_key("files/prova/.puck/memory.md"));

        fs::write(vault.join("prova/index.html"), "<html>2</html>").unwrap();
        let m2 = push(&store, &vault).await.unwrap();
        assert_eq!(m2.version, 2);
        assert_ne!(
            m1.files.get("prova/index.html").unwrap().sha,
            m2.files.get("prova/index.html").unwrap().sha
        );
        let _ = fs::remove_dir_all(&vault);
    }

    #[tokio::test]
    async fn prepare_pulls_remote_into_empty_local() {
        let store = MockStore::default();
        let remote = tmp_vault();
        fs::create_dir_all(remote.join("p/.puck")).unwrap();
        fs::write(remote.join("user.md"), "U").unwrap();
        fs::write(remote.join("p/index.html"), "<p>1</p>").unwrap();
        fs::write(remote.join("p/.puck/memory.md"), "mem").unwrap();
        push(&store, &remote).await.unwrap();
        let _ = fs::remove_dir_all(&remote);

        let local = tmp_vault();
        let m = prepare(&store, &local).await.unwrap();
        assert_eq!(m.version, 1);
        assert_eq!(
            fs::read_to_string(local.join("p/index.html")).unwrap(),
            "<p>1</p>"
        );
        assert_eq!(
            fs::read_to_string(local.join("p/.puck/memory.md")).unwrap(),
            "mem"
        );
        let _ = fs::remove_dir_all(&local);
    }

    #[tokio::test]
    async fn check_mirrors_remote_deletions() {
        let store = MockStore::default();
        let remote = tmp_vault();
        fs::write(remote.join("a.md"), "A").unwrap();
        fs::write(remote.join("b.md"), "B").unwrap();
        push(&store, &remote).await.unwrap();
        let _ = fs::remove_dir_all(&remote);

        let local = tmp_vault();
        prepare(&store, &local).await.unwrap();
        assert!(local.join("a.md").is_file());
        assert!(local.join("b.md").is_file());

        // altro device: prepara (pull di v1), rimuove b.md e pusha → v2
        let remote2 = tmp_vault();
        prepare(&store, &remote2).await.unwrap();
        assert!(remote2.join("b.md").is_file());
        fs::remove_file(remote2.join("b.md")).unwrap();
        let m2 = push(&store, &remote2).await.unwrap();
        assert_eq!(m2.version, 2);
        assert!(!store.map.lock().unwrap().contains_key("files/b.md"));
        let _ = fs::remove_dir_all(&remote2);

        // il primo device fa check → mirror: b.md via
        check(&store, &local).await.unwrap();
        assert!(local.join("a.md").is_file());
        assert!(!local.join("b.md").exists());
        let _ = fs::remove_dir_all(&local);
    }

    #[tokio::test]
    async fn crash_local_ahead_pushes_on_next_push() {
        let store = MockStore::default();
        let vault = tmp_vault();
        fs::write(vault.join("x.md"), "X1").unwrap();
        push(&store, &vault).await.unwrap();
        // crash: modifico locale senza push
        fs::write(vault.join("x.md"), "X2").unwrap();
        // check non tocca nulla (remoto non più nuovo)
        check(&store, &vault).await.unwrap();
        assert_eq!(fs::read_to_string(vault.join("x.md")).unwrap(), "X2");
        let m = push(&store, &vault).await.unwrap();
        assert_eq!(m.version, 2);
        let remote: Manifest =
            serde_json::from_slice(&store.map.lock().unwrap()[MANIFEST_KEY]).unwrap();
        assert_eq!(remote.files["x.md"].sha, sha256(b"X2"));
        let _ = fs::remove_dir_all(&vault);
    }

    #[tokio::test]
    async fn snapshot_keeps_only_synced_files() {
        let vault = tmp_vault();
        fs::create_dir_all(vault.join("p/.puck")).unwrap();
        fs::create_dir_all(vault.join("p/.puck-review")).unwrap();
        fs::write(vault.join("user.md"), "u").unwrap();
        fs::write(vault.join("p/index.html"), "x").unwrap();
        fs::write(vault.join("p/.puck/memory.md"), "m").unwrap();
        fs::write(vault.join("p/.puck/schema.md"), "s").unwrap();
        fs::write(vault.join("p/.puck/alt.md"), "skip").unwrap();
        fs::write(vault.join("p/.puck-review/shot.png"), "skip").unwrap();
        fs::write(vault.join(".env"), "SECRET").unwrap();
        fs::create_dir_all(vault.join(".puck-cloud")).unwrap();
        fs::write(vault.join(".puck-cloud/local.json"), "{\"version\":1}").unwrap();
        let snap = snapshot(&vault).unwrap();
        assert!(snap.contains_key("user.md"));
        assert!(snap.contains_key("p/index.html"));
        assert!(snap.contains_key("p/.puck/memory.md"));
        assert!(snap.contains_key("p/.puck/schema.md"));
        assert!(!snap.contains_key("p/.puck/alt.md"));
        assert!(!snap.contains_key("p/.puck-review/shot.png"));
        assert!(!snap.contains_key(".env"));
        assert!(!snap.contains_key(".puck-cloud/local.json"));
        let _ = fs::remove_dir_all(&vault);
    }

    #[test]
    fn b64url_encodes_without_padding() {
        assert_eq!(b64url(&[0x00, 0x01, 0x02, 0xff]), "AAEC_w");
        assert_eq!(b64url(&[0xff, 0xff, 0xff, 0xee]), "____7g");
    }

    #[test]
    fn pkce_verifier_and_challenge_have_expected_shape() {
        let v1 = pkce_verifier().unwrap();
        let v2 = pkce_verifier().unwrap();
        assert_eq!(v1.len(), 43);
        assert_ne!(v1, v2);
        let c1 = pkce_challenge(&v1);
        assert_eq!(c1.len(), 43);
        assert_eq!(c1, pkce_challenge(&v1));
        assert_ne!(c1, pkce_challenge(&v2));
    }

    #[test]
    fn session_file_roundtrip() {
        let _g = TEST_LOCK.lock().unwrap();
        let dir = tmp_vault();
        let file = dir.join("session.json");
        *TEST_FILE.lock().unwrap() = Some(file.clone());
        clear_session();
        assert!(session().is_none());
        let sess = Session {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 123,
            email: "m@x.it".into(),
            uid: String::new(),
        };
        save_session(&sess).unwrap();
        let loaded = session().unwrap();
        assert_eq!(loaded.email, "m@x.it");
        assert_eq!(loaded.access_token, "at");
        clear_session();
        assert!(session().is_none());
        *TEST_FILE.lock().unwrap() = None;
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_for_prefixes_with_uid_when_logged_in() {
        let _g = TEST_LOCK.lock().unwrap();
        let dir = tmp_vault();
        let file = dir.join("session.json");
        *TEST_FILE.lock().unwrap() = Some(file.clone());
        clear_session();
        save_session(&Session {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 0,
            email: "m@x.it".into(),
            uid: "u-42".into(),
        })
        .unwrap();
        let cloud = Cloud {
            url: "https://x.supabase.co".into(),
            anon: "anon".into(),
        };
        assert_eq!(cloud.key_for("manifest.json"), "u-42/manifest.json");
        assert_eq!(cloud.key_for("files/a.md"), "u-42/files/a.md");
        clear_session();
        assert_eq!(cloud.key_for("manifest.json"), "manifest.json");
        *TEST_FILE.lock().unwrap() = None;
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_verifier_roundtrip_and_url_decode() {
        let _g = TEST_LOCK.lock().unwrap();
        let dir = tmp_vault();
        let file = dir.join("session.json");
        *TEST_FILE.lock().unwrap() = Some(file.clone());
        assert!(take_pending_verifier().is_none());
        save_pending_verifier("verif-123").unwrap();
        assert_eq!(take_pending_verifier().as_deref(), Some("verif-123"));
        assert!(take_pending_verifier().is_none());
        *TEST_FILE.lock().unwrap() = None;
        assert_eq!(url_decode("a%3Db%20c"), "a=b c");
        assert_eq!(url_decode("code-x"), "code-x");
        let _ = fs::remove_dir_all(&dir);
    }
}
