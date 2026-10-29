//! Kimi / Moonshot Provider。基于 kimi-switch-provider-common 的文件型引擎。

mod kimi_files;
mod kimi_usage;
mod net;
mod oauth;
mod paths;

pub mod device_flow;
pub mod web_membership;

pub use kimi_usage::cache_official_profile_from_blob;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use kimi_switch_core::error::{Error, Result};
use kimi_switch_core::{Account, AccountRegistry, CredentialStore, Quota};
use kimi_switch_engine::{BlobMetadata, FileBlobProvider, FileBlobRuntime, RefreshOutcome};

pub const PROVIDER_ID: &str = "kimi";

/// Kimi runtime adapter：只提供差异点，机制在引擎。
#[derive(Clone)]
pub struct KimiRuntime {
    registry: Arc<AccountRegistry>,
    /// 与引擎同一份凭证存储：401 协调恢复的轮换结果要经引擎的轮换写回比对提交（ADR-0019）。
    store: Arc<dyn CredentialStore>,
}

#[async_trait]
impl FileBlobRuntime for KimiRuntime {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }
    fn home(&self) -> PathBuf {
        paths::kimi_home()
    }
    fn live_cred_path(&self, home: &Path) -> PathBuf {
        paths::active_cred_path(home)
    }
    fn parse_metadata(&self, blob: &str) -> BlobMetadata {
        kimi_files::parse_metadata(blob)
    }
    async fn refresh(&self, blob: &str) -> Result<RefreshOutcome> {
        oauth::refresh_blob(blob).await
    }
    async fn fetch_quota(&self, access_token: &str, account: &Account) -> Result<Vec<Quota>> {
        kimi_usage::fetch_quota_with_active_recovery(access_token, account, self).await
    }
    fn credential_expiry(&self, blob: &str) -> Option<i64> {
        kimi_files::credential_expiry(blob)
    }

    // 当前账号主动续期（issue #44）：判定与 DeadToken 纪律在 oauth，机制在引擎。
    async fn proactive_renewal_wanted(&self, blob: &str) -> bool {
        oauth::proactive_renewal_wanted(blob).await
    }
    fn proactive_renewal_blocked(&self, home: &Path, blob: &str) -> bool {
        oauth::proactive_renewal_blocked(home, blob)
    }
    fn proactive_renewal_reject(&self, home: &Path, blob: &str) -> Result<Error> {
        oauth::proactive_renewal_reject(home, blob)
    }
    fn proactive_renewal_rotated(&self, home: &Path, blob: &str) -> Result<()> {
        oauth::proactive_renewal_rotated(home, blob)
    }
}

impl KimiRuntime {
    /// 401 协调恢复的轮换结果经引擎的轮换写回比对提交（ADR-0019）。阻塞 IO，调用方在
    /// `spawn_blocking` 里调用；`home` 是本次恢复读取当前登录文件的 Kimi 工作目录。
    fn commit_rotation(
        &self,
        home: &Path,
        account: &Account,
        consumed: &str,
        rotated: &str,
    ) -> Result<()> {
        KimiProvider::commit_rotation_with(
            self,
            self.store.as_ref(),
            self.registry.as_ref(),
            home,
            account,
            consumed,
            rotated,
        )
    }
}

/// 便捷别名：Kimi Provider = 文件型引擎 + Kimi adapter。
pub type KimiProvider = FileBlobProvider<KimiRuntime>;

/// 构造 KimiProvider（与 BetaProvider/AlphaProvider 的 `::new` 调用面一致）。
pub fn new(store: Arc<dyn CredentialStore>, registry: Arc<AccountRegistry>) -> KimiProvider {
    FileBlobProvider::new(
        KimiRuntime {
            registry: registry.clone(),
            store: store.clone(),
        },
        store,
        registry,
    )
}

#[cfg(test)]
mod test_support {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    pub struct MockServer {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        full_requests: Arc<Mutex<Vec<String>>>,
        handle: Option<JoinHandle<()>>,
    }

    impl MockServer {
        pub fn start(responses: Vec<(&'static str, &'static str)>) -> Self {
            Self::spawn(responses, false)
        }

        /// 先按序回完 `responses`，再 accept 下一连接并挂起不写回。
        /// 用于模拟 `/me` 对端挂起；调用方不要 `finish()`（会一直等）。
        pub fn start_then_hang(responses: Vec<(&'static str, &'static str)>) -> Self {
            Self::spawn(responses, true)
        }

        fn spawn(responses: Vec<(&'static str, &'static str)>, hang_after: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let full_requests = Arc::new(Mutex::new(Vec::new()));
            let captured = Arc::clone(&requests);
            let captured_full = Arc::clone(&full_requests);
            let handle = std::thread::spawn(move || {
                for (status, body) in responses {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut buffer = [0_u8; 8192];
                    let count = stream.read(&mut buffer).unwrap();
                    let request = String::from_utf8_lossy(&buffer[..count]).to_string();
                    captured
                        .lock()
                        .unwrap()
                        .push(request.lines().next().unwrap_or_default().to_string());
                    captured_full.lock().unwrap().push(request);
                    write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                }
                if hang_after {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut buffer = [0_u8; 8192];
                    let count = stream.read(&mut buffer).unwrap();
                    let request = String::from_utf8_lossy(&buffer[..count]).to_string();
                    captured
                        .lock()
                        .unwrap()
                        .push(request.lines().next().unwrap_or_default().to_string());
                    captured_full.lock().unwrap().push(request);
                    // 保持连接、不写响应，模拟对端挂起。
                    std::thread::park();
                    drop(stream);
                }
            });
            Self {
                base_url,
                requests,
                full_requests,
                handle: Some(handle),
            }
        }

        pub fn base_url(&self) -> &str {
            &self.base_url
        }

        pub fn received(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        pub fn finish(mut self) -> Vec<String> {
            self.handle.take().unwrap().join().unwrap();
            Arc::try_unwrap(self.requests)
                .unwrap()
                .into_inner()
                .unwrap()
        }

        /// 收取完整请求原文（含请求行、头与 body），用于断言
        /// `connect-protocol-version` 之类的协议头（issue #50）。
        pub fn finish_full(mut self) -> Vec<String> {
            self.handle.take().unwrap().join().unwrap();
            Arc::try_unwrap(self.full_requests)
                .unwrap()
                .into_inner()
                .unwrap()
        }
    }

    /// 401 协调恢复用的 runtime：注册表与凭证存储都落在用例目录 `dir`（交换锁也在此目录）。
    pub fn recovery_runtime(dir: &std::path::Path) -> crate::KimiRuntime {
        crate::KimiRuntime {
            registry: Arc::new(kimi_switch_core::AccountRegistry::new(
                dir.join("registry.toml"),
            )),
            store: Arc::new(kimi_switch_core::FileStore::new(
                dir.join("credentials.json"),
            )),
        }
    }

    /// 测试用 JWT 构造：payload 经最小 base64url（无 padding）编码，拼成 header.payload.sig。
    pub fn jwt_with_payload(payload: &str) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut encoded = String::new();
        for chunk in payload.as_bytes().chunks(3) {
            let b0 = u32::from(chunk[0]);
            let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
            let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
            let n = (b0 << 16) | (b1 << 8) | b2;
            encoded.push(ALPHABET[(n >> 18) as usize & 63] as char);
            encoded.push(ALPHABET[(n >> 12) as usize & 63] as char);
            if chunk.len() > 1 {
                encoded.push(ALPHABET[(n >> 6) as usize & 63] as char);
            }
            if chunk.len() > 2 {
                encoded.push(ALPHABET[n as usize & 63] as char);
            }
        }
        format!("header.{encoded}.sig")
    }
}
