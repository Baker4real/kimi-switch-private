//! 跟随启动在 Windows 上的系统探针与执行器（issue #197，ADR-0021）。

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::StationsAndDesktops::{
    GetProcessWindowStation, GetUserObjectInformationW, UOI_FLAGS, USEROBJECTFLAGS,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetCurrentProcessId, CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP,
    DETACHED_PROCESS, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::WSF_VISIBLE;

use super::LaunchPlan;

/// 当前进程是否在桌面交互会话里。两条判据都满足才算交互：
///
/// - 会话 id 不为 0。Vista 起会话 0 只跑服务（会话 0 隔离），没有用户桌面；Win32-OpenSSH 的
///   sshd 是会话 0 里的服务，经 SSH 登录起的用户进程、WinRM 远程会话也都在会话 0（本机 sshd
///   在会话 0，2026-10-29 核实）。
/// - 进程所在的窗口站带 `WSF_VISIBLE`。只有交互窗口站 WinSta0 可见，服务用的非交互窗口站
///   （如 `Service-0x0-3e7$`）不可见。
///
/// 不拿「是不是控制台会话」（`WTSGetActiveConsoleSessionId`）当判据：RDP 会话不在控制台上，
/// 却是用户看得见的交互桌面。断开的 RDP 会话仍算交互，用户重连后 GUI 还在。任一项查不到时
/// 判为非交互：宁可不拉起，也不在看不见的会话里多开一份 GUI。
pub(crate) fn interactive_session() -> bool {
    session_is_interactive(current_session_id(), window_station_visible())
}

fn session_is_interactive(session_id: Option<u32>, station_visible: Option<bool>) -> bool {
    session_id.is_some_and(|id| id != 0) && station_visible == Some(true)
}

fn current_session_id() -> Option<u32> {
    // SAFETY: 无参数，恒成功。
    session_of(unsafe { GetCurrentProcessId() })
}

/// 进程所在的会话 id；进程已退出或无权查询（如别的用户的进程）时为 None。
fn session_of(pid: u32) -> Option<u32> {
    let mut session = 0;
    // SAFETY: session 是有效的输出指针。
    let ok = unsafe { ProcessIdToSessionId(pid, &mut session) };
    (ok != 0).then_some(session)
}

fn window_station_visible() -> Option<bool> {
    // SAFETY: 返回的窗口站句柄归系统所有，不需要（也不应）关闭。
    let station = unsafe { GetProcessWindowStation() };
    if station.is_null() {
        return None;
    }
    let mut flags = USEROBJECTFLAGS::default();
    // SAFETY: 缓冲是一个完整的 USEROBJECTFLAGS，长度与之一致；不需要回写所需长度。
    let ok = unsafe {
        GetUserObjectInformationW(
            station,
            UOI_FLAGS,
            std::ptr::from_mut(&mut flags).cast(),
            std::mem::size_of::<USEROBJECTFLAGS>() as u32,
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(flags.dwFlags & WSF_VISIBLE as u32 != 0)
}

/// 本 Windows 会话里是否有映像名为 `image`（不分大小写）的进程。按会话 id 判定，不依赖单实例
/// 插件的互斥量命名；别的会话（别的用户、SSH 会话）里的同名进程不算。
pub(crate) fn gui_running(image: &str) -> bool {
    current_session_id().is_some_and(|session| process_running_in_session(image, session))
}

/// 枚举进程快照找同名且同会话的进程。快照失败时按没在运行处理：多拉起的那一个由 GUI 的
/// 单实例收敛（后到的进程把参数转交给已有实例后退出）。
fn process_running_in_session(image: &str, session: u32) -> bool {
    // SAFETY: 快照句柄在函数末尾关闭。
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        tracing::warn!(
            err = %io::Error::last_os_error(),
            "on-session-start: process snapshot failed"
        );
        return false;
    }
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut found = false;
    // SAFETY: 快照句柄有效，entry.dwSize 已按结构大小设置。
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while more {
        let len = entry
            .szExeFile
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(entry.szExeFile.len());
        if String::from_utf16_lossy(&entry.szExeFile[..len]).eq_ignore_ascii_case(image)
            && session_of(entry.th32ProcessID) == Some(session)
        {
            found = true;
            break;
        }
        // SAFETY: 同上。
        more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    // SAFETY: 快照句柄来自成功的 CreateToolhelp32Snapshot，只关一次。
    unsafe { CloseHandle(snapshot) };
    found
}

/// 按计划创建进程，创建成功即返回，不等它。要求脱离但 Job 不允许时返回错误，由入口记录
/// launch_failed；不能降级成随宿主关闭的 GUI。
///
/// - 计划不继承句柄时以 `bInheritHandles = FALSE` 创建，子进程拿不到本进程的任何句柄，包括
///   hook 执行方交给本进程的 stdout / stderr 管道。std 的 `Command` 恒以 TRUE 创建进程，即使
///   三个标准句柄都设成 `Stdio::null()`，本进程里可继承的管道句柄照样会被子进程继承，hook
///   执行方就一直等管道关闭，直到 hook 超时。
/// - 不设 `STARTF_USESTDHANDLES`：配合 `DETACHED_PROCESS`，子进程的标准句柄为空，与从资源
///   管理器启动 GUI 一样。
/// - 脱离进程树：`DETACHED_PROCESS`（不挂 hook 的控制台）、`CREATE_NEW_PROCESS_GROUP`（收不到
///   发给 hook 进程组的 Ctrl 事件），再加 `CREATE_BREAKAWAY_FROM_JOB`：hook 执行方若把本进程
///   放进带 KILL_ON_JOB_CLOSE 的 Job，只有脱离它，GUI 才能在 Kimi Code 退出后活下来。Job
///   不允许 breakaway 时这次创建返回 `ERROR_ACCESS_DENIED`，不再去掉标志重试。Job 嵌套时
///   拒不拒绝只看直接所在的那层（2026-10-29 本机实测：嵌在不允许 breakaway 的 Job 里、本身
///   允许的 Job 照样能脱离）；外层 Job 的策略不归本进程管。
pub(crate) fn launch(plan: &LaunchPlan) -> io::Result<()> {
    let flags = if plan.detach {
        DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB
    } else {
        0
    };
    create_process(plan, flags)
}

fn create_process(plan: &LaunchPlan, flags: PROCESS_CREATION_FLAGS) -> io::Result<()> {
    let program = wide_nul(plan.program.as_os_str());
    let directory = wide_nul(plan.working_dir.as_os_str());
    // 命令行：带引号的程序路径（Windows 文件名不含 `"`），再接空格分隔的参数。参数都是固定的
    // 选项字面量，不含空白与引号，不需要转义。
    let mut command_line: Vec<u16> = vec![u16::from(b'"')];
    command_line.extend(plan.program.as_os_str().encode_wide());
    command_line.push(u16::from(b'"'));
    for arg in &plan.args {
        debug_assert!(
            !arg.contains(|c: char| c.is_whitespace() || c == '"'),
            "argument needs quoting: {arg}"
        );
        command_line.push(u16::from(b' '));
        command_line.extend(arg.encode_utf16());
    }
    command_line.push(0);
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: 程序路径与工作目录以 NUL 结尾、在调用期间有效；命令行缓冲可写（CreateProcessW
    // 可能改写它）；startup、info 是初始化好的结构；不传安全属性与环境块（继承本进程环境）。
    let ok = unsafe {
        CreateProcessW(
            program.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            i32::from(plan.inherit_handles),
            flags,
            std::ptr::null(),
            directory.as_ptr(),
            &startup,
            &mut info,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // 不等 GUI：立刻关掉进程与主线程句柄。
    // SAFETY: 两个句柄来自成功的 CreateProcessW，各关一次。
    unsafe {
        CloseHandle(info.hThread);
        CloseHandle(info.hProcess);
    }
    Ok(())
}

fn wide_nul(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    //! 真实执行器的演练：测试进程像 hook 执行方那样经管道接一个「hook 唤起的 CLI」子进程的
    //! stdout / stderr，必要时先把它放进带 KILL_ON_JOB_CLOSE 的 Job；子进程用真实执行器拉起
    //! 一个常驻子进程后退出。两个子进程都是测试二进制自己（被忽略的用例入口，只在带标记
    //! 文件的目录里干活），演练绝不拉起 GUI。

    use std::io::Read;
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };

    use crate::LaunchPlan;

    /// 子进程入口只在带这个标记文件的工作目录里干活：手动 `--ignored` 跑全部用例时什么都不做。
    const MARKER: &str = "follow-launch-rehearsal";
    /// 测试把「hook 唤起的 CLI」放进 Job 之后才放行它拉起常驻子进程。
    const HOOK_GO: &str = "hook-go";
    /// 常驻子进程在计划给的工作目录里写下自己的 pid。
    const SLEEPER_STARTED: &str = "sleeper-started";
    const SLEEPER_STOP: &str = "sleeper-stop";
    /// 计划给常驻子进程的工作目录，与「hook 唤起的 CLI」自己的工作目录不同。
    const SLEEPER_DIR: &str = "gui-dir";

    fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if done() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// 带 KILL_ON_JOB_CLOSE 的 Job，模拟 hook 执行方把 CLI 放进去。drop 即关闭唯一的句柄，
    /// 结束 Job 里的全部进程，相当于 Kimi Code 退出。
    struct Job(HANDLE);

    impl Job {
        fn new(limits: u32) -> Self {
            // SAFETY: 无名 Job、默认安全属性；返回的句柄由 Drop 关闭。
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            assert!(!job.is_null(), "{}", std::io::Error::last_os_error());
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = limits;
            // SAFETY: info 是完整初始化的 JOBOBJECT_EXTENDED_LIMIT_INFORMATION，长度与之一致。
            let ok = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&info).cast(),
                    std::mem::size_of_val(&info) as u32,
                )
            };
            assert!(ok != 0, "{}", std::io::Error::last_os_error());
            Self(job)
        }

        fn assign(&self, child: &Child) {
            // SAFETY: 两个句柄在调用期间都有效。
            let ok = unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) };
            assert!(ok != 0, "{}", std::io::Error::last_os_error());
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: 句柄来自 CreateJobObjectW，只关一次。
            unsafe { CloseHandle(self.0) };
        }
    }

    /// 常驻子进程：起来后先打开它的进程句柄（它在等停止文件，pid 不会被复用）。drop 时让它
    /// 退出，必要时直接结束它，再关句柄。
    struct Sleeper {
        dir: PathBuf,
        process: HANDLE,
    }

    impl Sleeper {
        fn open(dir: &Path) -> Option<Self> {
            let pid: u32 = std::fs::read_to_string(dir.join(SLEEPER_STARTED))
                .ok()?
                .trim()
                .parse()
                .ok()?;
            // SAFETY: 只请求等待与结束权限；失败返回空句柄。
            let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
            (!process.is_null()).then(|| Self {
                dir: dir.to_path_buf(),
                process,
            })
        }

        fn exits_within(&self, timeout: Duration) -> bool {
            // SAFETY: 句柄有效，带 SYNCHRONIZE 权限。
            unsafe {
                WaitForSingleObject(self.process, timeout.as_millis() as u32) == WAIT_OBJECT_0
            }
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = std::fs::write(self.dir.join(SLEEPER_STOP), "");
            if !self.exits_within(Duration::from_secs(5)) {
                // SAFETY: 句柄带 PROCESS_TERMINATE，指向本演练拉起的常驻子进程。
                unsafe { TerminateProcess(self.process, 1) };
                self.exits_within(Duration::from_secs(5));
            }
            // SAFETY: 句柄来自 OpenProcess，只关一次。
            unsafe { CloseHandle(self.process) };
        }
    }

    /// 后台读完子进程的 stdout 与 stderr；两条管道都读到 EOF 后才送出合并的输出。
    fn drain(child: &mut Child) -> mpsc::Receiver<String> {
        let mut stdout = child.stdout.take().expect("stdout pipe");
        let mut stderr = child.stderr.take().expect("stderr pipe");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let errors = std::thread::spawn(move || {
                let mut text = String::new();
                let _ = stderr.read_to_string(&mut text);
                text
            });
            let mut text = String::new();
            let _ = stdout.read_to_string(&mut text);
            text.push_str(&errors.join().unwrap_or_default());
            let _ = tx.send(text);
        });
        rx
    }

    /// 一次演练的可观察结果。
    struct Rehearsal {
        /// 「hook 唤起的 CLI」正常退出，即真实执行器返回了 Ok。
        hook_exit_ok: bool,
        /// 执行器是否确实创建了常驻子进程。
        child_started: bool,
        /// 「hook 唤起的 CLI」退出后 5 秒内，接它 stdout / stderr 的管道读到 EOF，且常驻
        /// 子进程这时仍在运行。
        pipes_closed: bool,
        /// 关掉 Job 后常驻子进程是否还活着；没有 Job 时为 None。
        survived_job_close: Option<bool>,
        /// 子进程的 stdout 与 stderr。
        output: String,
    }

    fn rehearse(job_limits: Option<u32>) -> Rehearsal {
        let dir = tempfile::tempdir().unwrap();
        let sleeper_dir = dir.path().join(SLEEPER_DIR);
        std::fs::create_dir_all(&sleeper_dir).unwrap();
        std::fs::write(dir.path().join(MARKER), "").unwrap();
        std::fs::write(sleeper_dir.join(MARKER), "").unwrap();

        let job = job_limits.map(Job::new);
        let mut hook = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "follow_launch_sys::tests::rehearsal_hook_child",
                "--nocapture",
            ])
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pipes = drain(&mut hook);
        if let Some(job) = &job {
            job.assign(&hook);
        }
        std::fs::write(dir.path().join(HOOK_GO), "").unwrap();

        let hook_exit_ok = wait_until(Duration::from_secs(30), || {
            hook.try_wait().unwrap().is_some()
        }) && hook.wait().unwrap().success();
        let _ = hook.kill();
        let sleeper = Sleeper::open(&sleeper_dir);
        let child_started = sleeper.is_some();
        let early = pipes.recv_timeout(Duration::from_secs(5)).ok();
        let pipes_closed = early.is_some()
            && sleeper
                .as_ref()
                .is_some_and(|sleeper| !sleeper.exits_within(Duration::ZERO));
        let survived_job_close = job.map(|job| {
            drop(job);
            sleeper
                .as_ref()
                .is_some_and(|sleeper| !sleeper.exits_within(Duration::from_secs(1)))
        });
        drop(sleeper);
        let output = early
            .or_else(|| pipes.recv_timeout(Duration::from_secs(10)).ok())
            .unwrap_or_default();
        Rehearsal {
            hook_exit_ok,
            child_started,
            pipes_closed,
            survived_job_close,
            output,
        }
    }

    /// hook 执行方不必等 GUI：「hook 唤起的 CLI」用真实执行器拉起常驻子进程后退出，接它
    /// stdout / stderr 的管道立刻读到 EOF，常驻子进程照常运行在计划给的工作目录里。
    #[test]
    fn real_launch_leaves_hook_pipes_to_close() {
        // cargo 自身可能在禁止 breakaway 的 Job 中；用可脱离的 hook Job 演练成功路径。
        let rehearsal = rehearse(Some(JOB_OBJECT_LIMIT_BREAKAWAY_OK));

        assert!(rehearsal.hook_exit_ok, "{}", rehearsal.output);
        assert!(
            rehearsal.pipes_closed,
            "常驻子进程不得持有 hook 交给 CLI 的标准输入输出管道：{}",
            rehearsal.output
        );
    }

    /// Job 允许 breakaway：GUI 脱离 hook 所在的 Job，Job 关闭（Kimi Code 退出）后照常运行。
    /// breakaway 只看直接所在的 Job：cargo 给测试进程套的 Job 不允许 breakaway，演练用的 Job
    /// 嵌在它里面照样能脱离（2026-10-29 本机实测），所以本用例在 cargo 下同样有效。
    #[test]
    fn real_launch_breaks_away_from_kill_on_close_job() {
        let rehearsal = rehearse(Some(
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK,
        ));

        assert!(rehearsal.hook_exit_ok, "{}", rehearsal.output);
        assert!(rehearsal.pipes_closed, "{}", rehearsal.output);
        assert_eq!(
            rehearsal.survived_job_close,
            Some(true),
            "脱离 Job 的 GUI 不随 Job 关闭结束：{}",
            rehearsal.output
        );
    }

    /// Job 不允许 breakaway：必须拒绝拉起，不能创建一份会随宿主关闭的 GUI 并报告成功。
    #[test]
    fn real_launch_refuses_job_that_denies_breakaway() {
        let rehearsal = rehearse(Some(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE));

        assert!(
            !rehearsal.hook_exit_ok,
            "必须报告创建失败：{}",
            rehearsal.output
        );
        assert!(
            !rehearsal.child_started,
            "不应留下依附宿主的 GUI：{}",
            rehearsal.output
        );
    }

    /// 交互会话判据：会话 0（服务会话，经 SSH 登录的用户进程也在这里）或窗口站不可见（服务
    /// 窗口站）判为非交互；控制台会话与 RDP 这类非 0 会话、可见窗口站判为交互；任一项查不到
    /// 判为非交互。
    #[test]
    fn interactive_session_criteria() {
        assert!(
            !super::session_is_interactive(Some(0), Some(true)),
            "会话 0：服务、SSH"
        );
        assert!(
            !super::session_is_interactive(Some(2), Some(false)),
            "服务窗口站"
        );
        assert!(
            super::session_is_interactive(Some(1), Some(true)),
            "控制台会话"
        );
        assert!(
            super::session_is_interactive(Some(3), Some(true)),
            "RDP 会话不在控制台上，同样可交互"
        );
        assert!(!super::session_is_interactive(None, Some(true)));
        assert!(!super::session_is_interactive(Some(1), None));
    }

    /// 真实探针：本测试进程的会话 id 与窗口站标志都查得到。拿本测试进程自己的映像名核对
    /// 「本会话里有没有这个程序的进程」：同一会话查得到，映像名不分大小写；换一个会话号、
    /// 或换一个不存在的映像名就查不到。
    #[test]
    fn real_probes_find_own_process_only_in_own_session() {
        let session = super::current_session_id().expect("session id of the test process");
        assert!(super::window_station_visible().is_some());
        let image = std::env::current_exe()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        assert!(super::process_running_in_session(&image, session));
        assert!(super::process_running_in_session(
            &image.to_uppercase(),
            session
        ));
        assert!(!super::process_running_in_session(
            &image,
            session.wrapping_add(1)
        ));
        assert!(!super::process_running_in_session(
            "kimi-switch-no-such-program.exe",
            session
        ));
    }

    /// 演练里的「hook 唤起的 CLI」：等测试把它放进 Job，再用真实执行器拉起常驻子进程，
    /// 等它起来后退出。
    #[test]
    #[ignore = "rehearsal child: spawned by the real_launch_* tests"]
    fn rehearsal_hook_child() {
        if !Path::new(MARKER).exists() {
            return;
        }
        assert!(wait_until(Duration::from_secs(10), || Path::new(HOOK_GO).exists()));
        let working_dir = std::env::current_dir().unwrap().join(SLEEPER_DIR);
        let plan = LaunchPlan {
            program: std::env::current_exe().unwrap(),
            args: vec![
                "--ignored",
                "--exact",
                "follow_launch_sys::tests::rehearsal_sleeper",
            ],
            working_dir: working_dir.clone(),
            detach: true,
            inherit_handles: false,
        };
        super::launch(&plan).expect("real launch");
        assert!(
            wait_until(Duration::from_secs(10), || working_dir
                .join(SLEEPER_STARTED)
                .exists()),
            "常驻子进程没有在计划给的工作目录里起来"
        );
    }

    /// 演练里的常驻子进程：在工作目录写下自己的 pid，等停止文件或 30 秒后退出。
    #[test]
    #[ignore = "rehearsal sleeper: launched by rehearsal_hook_child"]
    fn rehearsal_sleeper() {
        if !Path::new(MARKER).exists() {
            return;
        }
        std::fs::write("sleeper-started.tmp", std::process::id().to_string()).unwrap();
        std::fs::rename("sleeper-started.tmp", SLEEPER_STARTED).unwrap();
        wait_until(Duration::from_secs(30), || Path::new(SLEEPER_STOP).exists());
    }
}
