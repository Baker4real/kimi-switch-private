//! 序列化同会话 GUI 的单实例注册，封住插件 mutex 已存在、IPC 窗口尚未创建的空窗。

use std::io;
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

/// 只持有到 Tauri build 完成；不得跨越应用的消息循环。
pub(crate) struct StartupGate(HANDLE);

impl StartupGate {
    pub(crate) fn acquire() -> io::Result<Self> {
        Self::acquire_named("Local\\kimi-switch-startup")
    }

    fn acquire_named(name: &str) -> io::Result<Self> {
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: 名称以 NUL 结尾，句柄由本对象管理，不继承。
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: 有效 mutex 句柄；仅序列化插件注册，异常卡住时最多等 10 秒。
        let wait = unsafe { WaitForSingleObject(handle, 10_000) };
        match wait {
            // 重复实例由插件直接 exit，不执行 Drop；Windows 会把锁标记为 abandoned。
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self(handle)),
            _ => {
                let err = if wait == WAIT_FAILED {
                    io::Error::last_os_error()
                } else {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "等待另一份 Kimi 换号器完成启动超时",
                    )
                };
                // SAFETY: 未取得锁，只关闭本次打开的句柄。
                unsafe { CloseHandle(handle) };
                Err(err)
            }
        }
    }
}

impl Drop for StartupGate {
    fn drop(&mut self) {
        // SAFETY: 句柄归本对象所有，获取与释放都发生在同一线程。
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::StartupGate;
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn test_name() -> String {
        format!(
            "Local\\kimi-switch-startup-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    /// 同时恢复多个会话：第二份 GUI 必须等第一份完成注册后，才进入单实例判定。
    #[test]
    fn concurrent_startup_waits_until_first_registration_finishes() {
        let name = test_name();
        let first = StartupGate::acquire_named(&name).unwrap();
        let (attempt_tx, attempt_rx) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        let second = std::thread::spawn(move || {
            attempt_tx.send(()).unwrap();
            let _gate = StartupGate::acquire_named(&name).unwrap();
            entered_tx.send(()).unwrap();
        });
        attempt_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let entered_early = entered_rx.recv_timeout(Duration::from_millis(100)).is_ok();
        drop(first);
        second.join().unwrap();
        assert!(
            !entered_early,
            "第一份尚未建立 IPC 窗口时，第二份不能进入注册"
        );
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    /// 单实例插件直接退出第二份进程，来不及 Drop；下一份必须能接管被放弃的锁。
    #[test]
    fn startup_recovers_after_lock_owner_exits() {
        let name = test_name();
        let abandoned_name = name.clone();
        let handle = std::thread::spawn(move || {
            let gate = StartupGate::acquire_named(&abandoned_name).unwrap();
            let handle = gate.0 as usize;
            std::mem::forget(gate);
            handle
        })
        .join()
        .unwrap();
        let recovered = StartupGate::acquire_named(&name);
        // SAFETY: 前一线程已退出，句柄来自它创建的 mutex；只补关被 forget 的那一份句柄。
        unsafe { windows_sys::Win32::Foundation::CloseHandle(handle as _) };
        assert!(recovered.is_ok(), "所有者退出后应能继续启动");
    }
}
