//! Unix 真实伪终端（PTY）封装：基于 `libc::openpty`、`setsid`、`TIOCSCTTY` 与 `TIOCSWINSZ`。
//!
//! 本模块是 `rc-tools` 中唯一包含 `unsafe` 代码的底层 POSIX 胶水层，
//! 每个 `unsafe` 块均附带完整的 `// SAFETY:` 说明。

#![allow(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]
#![deny(missing_docs)]

use std::collections::VecDeque;
use std::io;
use std::os::unix::io::RawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::file_transfer::default_home_dir;

/// 串行化 `openpty` 与 `FD_CLOEXEC` 设置，防止并发 `fork`/`exec` 泄漏未设 `CLOEXEC` 的 PTY 描述符。
static OPENPTY_LOCK: Mutex<()> = Mutex::new(());

/// 清除文件描述符上的 `O_NONBLOCK` 标志（供文件传输安全打开普通文件后恢复阻塞读）。
pub(crate) fn clear_nonblock_fd(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` 来自调用方持有的有效打开文件描述符，`F_GETFL`/`F_SETFL` 仅修改该打开文件描述的状态标志。
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if (flags & libc::O_NONBLOCK) != 0
            && libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// 若 `fd` 落在 `0..=2`（标准输入/输出/错误），原子复制到 `>= 3` 并设 `FD_CLOEXEC`，再关闭原低号 fd。
fn relocate_if_stdio_fd(fd: RawFd) -> Result<RawFd, io::Error> {
    if fd > libc::STDERR_FILENO {
        return Ok(fd);
    }
    // SAFETY: `fd` 刚由 `openpty` 创建并由本线程独占；`F_DUPFD_CLOEXEC` 原子分配 `>= 3` 且带 `FD_CLOEXEC` 的新描述符。
    let new_fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, libc::STDERR_FILENO + 1) };
    if new_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` 已被成功复制到 `new_fd`，且归本函数独占，关闭旧低号 fd 不会影响其他代码。
    unsafe {
        libc::close(fd);
    }
    Ok(new_fd)
}

/// 后台读取缓冲上限（128 KiB），超出时丢弃最旧字节以防内存无界增长。
pub const PTY_OUTPUT_BUFFER_CAP: usize = 128 * 1024;

/// 单个 PTY 非阻塞待写队列上限（64 KiB），防止 `Ctrl+S` 挂起终端时阻塞或内存无界增长。
pub const PTY_PENDING_WRITE_CAP: usize = 64 * 1024;

/// 单个 Unix 伪终端子进程句柄。
///
/// master fd 以 `O_NONBLOCK` 打开，写入遇到 `EAGAIN`/`EWOULDBLOCK` 时缓存在有界
/// [`PTY_PENDING_WRITE_CAP`]（64 KiB）队列中并在后续 [`PtyProcess::try_flush`] /
/// [`PtyProcess::try_read_output`] 刷出，确保永不阻塞调用方事件循环；
/// 后台独立线程持续轮询读取 master fd；析构时仅对尚未退出的子进程发送 `SIGHUP`、
/// 强制终止并回收（防止 PID 复用误杀），随后 join 读取线程并关闭 master fd。
pub struct PtyProcess {
    master_fd: RawFd,
    child: Child,
    exited: bool,
    exit_code: Option<i32>,
    title: String,
    cwd: String,
    pending_write: VecDeque<u8>,
    output_buf: Arc<Mutex<VecDeque<u8>>>,
    stop: Arc<AtomicBool>,
    reader_eof: Arc<AtomicBool>,
    reader_thread: Option<JoinHandle<()>>,
}

impl PtyProcess {
    /// 启动新的交互式 Shell 伪终端。
    pub fn spawn(cols: u16, rows: u16) -> Result<Self, String> {
        let cols = cols.max(1);
        let rows = rows.max(1);

        let mut master_fd: RawFd = -1;
        let mut slave_fd: RawFd = -1;
        let mut ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        let (master_fd, slave_fd) = {
            let _guard = OPENPTY_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());

            // SAFETY: `master_fd`, `slave_fd`, `ws` 均为有效栈变量指针；`name` 与 `termp` 传空指针为 POSIX `openpty` 允许的标准用法。
            let open_rc = unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::addr_of_mut!(ws).cast(),
                )
            };
            if open_rc != 0 {
                return Err(format!("openpty failed: {}", io::Error::last_os_error()));
            }

            // SAFETY: `master_fd` 和 `slave_fd` 刚由 `openpty` 成功创建且在 `OPENPTY_LOCK` 保护下归本函数独占。
            unsafe {
                let _ = libc::fcntl(master_fd, libc::F_SETFD, libc::FD_CLOEXEC);
                let _ = libc::fcntl(slave_fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }

            let master_fd = match relocate_if_stdio_fd(master_fd) {
                Ok(fd) => fd,
                Err(e) => {
                    // SAFETY: 重定位失败，关闭尚未移交所有权的两个 fd 防止泄漏。
                    unsafe {
                        libc::close(master_fd);
                        libc::close(slave_fd);
                    }
                    return Err(format!("relocate master_fd failed: {e}"));
                }
            };
            let slave_fd = match relocate_if_stdio_fd(slave_fd) {
                Ok(fd) => fd,
                Err(e) => {
                    // SAFETY: 重定位失败，关闭尚未移交所有权的两个 fd 防止泄漏。
                    unsafe {
                        libc::close(master_fd);
                        libc::close(slave_fd);
                    }
                    return Err(format!("relocate slave_fd failed: {e}"));
                }
            };

            // SAFETY: `master_fd` 已重定位且归本函数独占，设置 `O_NONBLOCK` 防止写入阻塞宿主事件循环。
            unsafe {
                let flags = libc::fcntl(master_fd, libc::F_GETFL);
                if flags >= 0 {
                    let _ = libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
            }
            (master_fd, slave_fd)
        };

        let shell = resolve_shell();
        let title = Path::new(&shell)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("shell")
            .to_string();

        let cwd_path = resolve_initial_cwd();
        let cwd = cwd_path.to_string_lossy().into_owned();

        let mut cmd = Command::new(&shell);
        cmd.env("TERM", "xterm-256color")
            .env("LANG", "en_US.UTF-8")
            .current_dir(&cwd_path);

        // SAFETY: `pre_exec` 闭包仅在 `fork` 后的子进程中、执行 `exec` 之前运行，
        // 且闭包内仅调用 async-signal-safe 的 POSIX 系统调用（`setsid`/`ioctl`/`dup2`/`fcntl`/`close`）。
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                #[allow(clippy::unnecessary_cast)]
                if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0 as libc::c_int) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::dup2(slave_fd, libc::STDIN_FILENO) < 0
                    || libc::dup2(slave_fd, libc::STDOUT_FILENO) < 0
                    || libc::dup2(slave_fd, libc::STDERR_FILENO) < 0
                {
                    return Err(io::Error::last_os_error());
                }
                for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                    if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                if slave_fd > libc::STDERR_FILENO {
                    libc::close(slave_fd);
                }
                if master_fd > libc::STDERR_FILENO {
                    libc::close(master_fd);
                }
                Ok(())
            });
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                // SAFETY: 子进程启动失败，立刻关闭两个尚未移交所有权的 fd，避免泄漏。
                unsafe {
                    libc::close(slave_fd);
                    libc::close(master_fd);
                }
                return Err(format!("spawn shell '{shell}' failed: {e}"));
            }
        };

        // SAFETY: 父进程必须关闭 `slave_fd`，否则子进程退出后读取 `master_fd` 不会收到 EOF。
        unsafe {
            libc::close(slave_fd);
        }

        let output_buf = Arc::new(Mutex::new(VecDeque::with_capacity(4096)));
        let stop = Arc::new(AtomicBool::new(false));
        let reader_eof = Arc::new(AtomicBool::new(false));

        let reader_buf = Arc::clone(&output_buf);
        let reader_stop = Arc::clone(&stop);
        let reader_eof_flag = Arc::clone(&reader_eof);

        let reader_thread = thread::Builder::new()
            .name("rc-pty-reader".to_string())
            .spawn(move || {
                pty_reader_loop(master_fd, reader_buf, reader_stop, reader_eof_flag);
            })
            .map_err(|e| {
                let _ = child.kill();
                let _ = child.wait();
                // SAFETY: 线程创建失败且子进程已回收，立刻关闭 `master_fd`。
                unsafe {
                    libc::close(master_fd);
                }
                format!("spawn pty reader thread failed: {e}")
            })?;

        Ok(Self {
            master_fd,
            child,
            exited: false,
            exit_code: None,
            title,
            cwd,
            pending_write: VecDeque::new(),
            output_buf,
            stop,
            reader_eof,
            reader_thread: Some(reader_thread),
        })
    }

    /// 返回终端标题（Shell 名称）。
    pub fn title(&self) -> &str {
        &self.title
    }

    /// 返回初始工作目录。
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// 返回当前尚未刷入内核 PTY 缓冲区的待写字节数。
    pub fn pending_write_len(&self) -> usize {
        self.pending_write.len()
    }

    /// 返回子进程是否已被观察并回收退出。
    pub fn has_exited(&self) -> bool {
        self.exited
    }

    /// 非阻塞尝试将待写队列中积压的输入字节刷入 `master_fd`。
    pub fn try_flush(&mut self) -> Result<(), String> {
        while !self.pending_write.is_empty() {
            let (head, _) = self.pending_write.as_slices();
            if head.is_empty() {
                self.pending_write.make_contiguous();
                continue;
            }
            // SAFETY: `self.master_fd` 在整个 `PtyProcess` 存活期有效且已设 `O_NONBLOCK`，
            // `head` 指向有效只读内存切片。
            let n = unsafe { libc::write(self.master_fd, head.as_ptr().cast(), head.len()) };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if err.kind() == io::ErrorKind::WouldBlock {
                    return Ok(());
                }
                return Err(format!("pty write failed: {err}"));
            }
            if n == 0 {
                return Ok(());
            }
            self.pending_write.drain(..n as usize);
        }
        Ok(())
    }

    /// 非阻塞向伪终端写入用户输入字节；若内核 PTY 缓冲已满（如 `Ctrl+S` 流控挂起），
    /// 自动暂存至最多 [`PTY_PENDING_WRITE_CAP`]（64 KiB）有界队列，永不阻塞调用方；
    /// 若剩余空间不足则原子拒绝并返回 `Err`，绝不截断 UTF-8 或 ANSI 转义序列。
    pub fn write_input(&mut self, data: &[u8]) -> Result<(), String> {
        if !self.pending_write.is_empty() {
            self.try_flush()?;
        }

        if !self.pending_write.is_empty() {
            let avail = PTY_PENDING_WRITE_CAP.saturating_sub(self.pending_write.len());
            if data.len() > avail {
                return Err(format!(
                    "pty pending write buffer full ({PTY_PENDING_WRITE_CAP} bytes)"
                ));
            }
            self.pending_write.extend(data);
            return Ok(());
        }

        let mut offset = 0usize;
        while offset < data.len() {
            let chunk = &data[offset..];
            // SAFETY: `self.master_fd` 在整个 `PtyProcess` 存活期有效且已设 `O_NONBLOCK`，
            // `chunk` 指向有效只读内存切片。
            let n = unsafe { libc::write(self.master_fd, chunk.as_ptr().cast(), chunk.len()) };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if err.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                return Err(format!("pty write failed: {err}"));
            }
            if n == 0 {
                break;
            }
            offset += n as usize;
        }

        if offset < data.len() {
            let remaining = &data[offset..];
            let avail = PTY_PENDING_WRITE_CAP.saturating_sub(self.pending_write.len());
            if remaining.len() > avail {
                return Err(format!(
                    "pty pending write buffer full ({PTY_PENDING_WRITE_CAP} bytes)"
                ));
            }
            self.pending_write.extend(remaining);
        }
        Ok(())
    }

    /// 调整伪终端窗口行列尺寸（`TIOCSWINSZ`）。
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        let ws = libc::winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `self.master_fd` 在 `PtyProcess` 存活期间保持打开，`&ws` 为有效 `winsize` 结构体指针。
        #[allow(clippy::unnecessary_cast)]
        let rc = unsafe { libc::ioctl(self.master_fd, libc::TIOCSWINSZ as _, &ws) };
        if rc != 0 {
            return Err(format!(
                "pty TIOCSWINSZ failed: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    /// 非阻塞拉取后台读取线程已缓存的全部终端输出字节，同时顺带刷新积压的待写输入。
    pub fn try_read_output(&mut self) -> Vec<u8> {
        self.try_read_output_up_to(usize::MAX)
    }

    /// 非阻塞拉取最多 `max_bytes` 字节终端输出，同时顺带刷新积压的待写输入。
    pub(crate) fn try_read_output_up_to(&mut self, max_bytes: usize) -> Vec<u8> {
        if !self.pending_write.is_empty() {
            let _ = self.try_flush();
        }
        if max_bytes == 0 {
            return Vec::new();
        }
        let Ok(mut guard) = self.output_buf.lock() else {
            return Vec::new();
        };
        if guard.is_empty() {
            return Vec::new();
        }
        let take = guard.len().min(max_bytes);
        guard.drain(..take).collect()
    }

    /// 非阻塞查询子进程是否已经退出，返回退出码（信号终止时返回 `128 + signal`）。
    pub fn try_wait(&mut self) -> Option<i32> {
        if self.exited {
            return self.exit_code;
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                let code = if let Some(code) = status.code() {
                    code
                } else if let Some(sig) = status.signal() {
                    128 + sig
                } else {
                    -1
                };
                self.exited = true;
                self.exit_code = Some(code);
                Some(code)
            }
            Ok(None) => None,
            Err(_) => {
                self.exited = true;
                self.exit_code = Some(-1);
                Some(-1)
            }
        }
    }

    /// 后台读取线程是否已读到 EOF/挂断并结束。
    pub fn is_reader_eof(&self) -> bool {
        self.reader_eof.load(Ordering::Acquire)
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        if !self.exited {
            if let Ok(Some(_)) = self.child.try_wait() {
                self.exited = true;
            }
        }
        if !self.exited {
            let pid = self.child.id() as libc::pid_t;
            // SAFETY: `self.master_fd` 仍有效，且子进程尚未被 `wait` 回收（PID 不会被内核复用）；
            // 先向 PTY 前台进程组与子进程会话进程组发送 `SIGHUP` 请求优雅退出。
            unsafe {
                let fg_pgid = libc::tcgetpgrp(self.master_fd);
                if fg_pgid > 0 {
                    let _ = libc::kill(-fg_pgid, libc::SIGHUP);
                }
                if pid > 0 {
                    if pid != fg_pgid {
                        let _ = libc::kill(-pid, libc::SIGHUP);
                    }
                    let _ = libc::kill(pid, libc::SIGHUP);
                }
            }

            let deadline = Instant::now() + Duration::from_millis(60);
            while Instant::now() < deadline {
                match self.child.try_wait() {
                    Ok(Some(_)) => {
                        self.exited = true;
                        break;
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(5)),
                    Err(_) => {
                        self.exited = true;
                        break;
                    }
                }
            }

            if !self.exited {
                let _ = self.child.kill();
                let _ = self.child.wait();
                self.exited = true;
            }
        }

        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }

        // SAFETY: 读取线程已 join 退出，此时没有任何其他线程访问 `self.master_fd`，可安全关闭一次。
        unsafe {
            libc::close(self.master_fd);
        }
    }
}

fn pty_reader_loop(
    master_fd: RawFd,
    output_buf: Arc<Mutex<VecDeque<u8>>>,
    stop: Arc<AtomicBool>,
    reader_eof: Arc<AtomicBool>,
) {
    let mut buf = [0u8; 8192];
    while !stop.load(Ordering::Acquire) {
        let mut pfd = libc::pollfd {
            fd: master_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` 指向合法单元素 `pollfd`，`master_fd` 在本线程被 `Drop` join 前始终有效。
        let poll_rc = unsafe { libc::poll(&mut pfd, 1, 25) };
        if poll_rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if poll_rc == 0 {
            continue;
        }

        if (pfd.revents & libc::POLLIN) != 0 {
            // SAFETY: `master_fd` 有效且 `buf` 为可写栈缓冲区。
            let n = unsafe { libc::read(master_fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                push_bounded(&output_buf, &buf[..n as usize]);
                continue;
            }
            if n == 0 {
                break;
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted || err.kind() == io::ErrorKind::WouldBlock {
                continue;
            }
            // PTY slave 关闭后常见 `EIO`，视作正常 EOF。
            break;
        }

        if (pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)) != 0 {
            break;
        }
    }
    reader_eof.store(true, Ordering::Release);
}

fn push_bounded(output_buf: &Arc<Mutex<VecDeque<u8>>>, data: &[u8]) {
    let Ok(mut guard) = output_buf.lock() else {
        return;
    };
    if data.len() >= PTY_OUTPUT_BUFFER_CAP {
        guard.clear();
        guard.extend(&data[data.len() - PTY_OUTPUT_BUFFER_CAP..]);
        return;
    }
    let overflow = (guard.len() + data.len()).saturating_sub(PTY_OUTPUT_BUFFER_CAP);
    if overflow > 0 {
        guard.drain(..overflow);
    }
    guard.extend(data);
}

fn resolve_shell() -> String {
    if let Ok(shell) = std::env::var("SHELL") {
        let trimmed = shell.trim();
        if !trimmed.is_empty() && Path::new(trimmed).exists() {
            return trimmed.to_string();
        }
    }
    #[cfg(target_os = "macos")]
    {
        if Path::new("/bin/zsh").exists() {
            return "/bin/zsh".to_string();
        }
    }
    "/bin/sh".to_string()
}

fn resolve_initial_cwd() -> PathBuf {
    let home = default_home_dir();
    if home.is_dir() {
        return home;
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn master_fd_is_nonblocking_and_write_never_blocks_on_ctrl_s() {
        let mut pty = PtyProcess::spawn(80, 24).expect("spawn pty");

        // SAFETY: `pty.master_fd` is valid for the lifetime of `pty`.
        let flags = unsafe { libc::fcntl(pty.master_fd, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_ne!(
            flags & libc::O_NONBLOCK,
            0,
            "master_fd must have O_NONBLOCK"
        );

        // Pause terminal output with Ctrl+S and burst large input; must return immediately without blocking.
        let started = Instant::now();
        let _ = pty.write_input(&[0x13]);
        let burst = vec![b'a'; 32 * 1024];
        for _ in 0..8 {
            let _ = pty.write_input(&burst);
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "write_input blocked under flow-control pause"
        );
        assert!(pty.pending_write_len() <= PTY_PENDING_WRITE_CAP);
    }

    #[test]
    fn try_wait_sets_exited_flag_and_is_idempotent() {
        let mut pty = PtyProcess::spawn(80, 24).expect("spawn pty");
        assert!(!pty.has_exited());
        pty.write_input(b"exit 0\n").expect("write exit");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut code = None;
        while Instant::now() < deadline {
            let _ = pty.try_read_output();
            if let Some(c) = pty.try_wait() {
                code = Some(c);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(code, Some(0));
        assert!(pty.has_exited());
        assert_eq!(pty.try_wait(), Some(0));
    }

    #[test]
    fn write_input_rejects_overflow_atomically_without_slicing_utf8() {
        let mut pty = PtyProcess::spawn(80, 24).expect("spawn pty");
        assert!(
            pty.master_fd > libc::STDERR_FILENO,
            "master_fd must be relocated above STDERR_FILENO"
        );
        // SAFETY: `pty.master_fd` is valid for the lifetime of `pty`.
        let fd_flags = unsafe { libc::fcntl(pty.master_fd, libc::F_GETFD) };
        assert!(fd_flags >= 0);
        assert_ne!(
            fd_flags & libc::FD_CLOEXEC,
            0,
            "master_fd must have FD_CLOEXEC"
        );

        // Pre-fill pending_write leaving only 2 bytes of room, then attempt to write a 3-byte UTF-8 char.
        // First fill the kernel buffer or directly pre-seed pending_write so try_flush cannot drain all
        // when flow-control is paused, OR verify atomic rejection when remaining > avail.
        let _ = pty.write_input(&[0x13]); // Ctrl+S pause
        let fill = vec![b'x'; 16 * 1024];
        while pty.pending_write_len() == 0 {
            if pty.write_input(&fill).is_err() {
                break;
            }
        }
        // Top up pending_write to leave exactly 2 bytes free.
        let cur = pty.pending_write_len();
        if cur < PTY_PENDING_WRITE_CAP - 2 {
            pty.pending_write
                .extend(std::iter::repeat(b'y').take(PTY_PENDING_WRITE_CAP - 2 - cur));
        } else if cur > PTY_PENDING_WRITE_CAP - 2 {
            pty.pending_write.truncate(PTY_PENDING_WRITE_CAP - 2);
        }
        let before_len = pty.pending_write_len();
        assert_eq!(before_len, PTY_PENDING_WRITE_CAP - 2);

        // Attempt to write 3-byte UTF-8 "中" ([0xE4, 0xB8, 0xAD]): must fail atomically without pushing partial bytes!
        let cjk = "中".as_bytes();
        assert_eq!(cjk.len(), 3);
        let res = pty.write_input(cjk);
        assert!(res.is_err(), "expected atomic overflow rejection");
        assert_eq!(
            pty.pending_write_len(),
            before_len,
            "pending_write must not be partially mutated on overflow"
        );
        assert_ne!(
            pty.pending_write.back().copied(),
            Some(0xE4),
            "must not slice leading UTF-8 byte into pending_write"
        );
    }
}
