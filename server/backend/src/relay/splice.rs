#![allow(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]

//! Linux `splice(2)` zero-copy bidirectional TCP relay engine.
//!
//! Each direction moves bytes kernel-to-kernel (`in_socket -> 256 KiB pipe -> out_socket`)
//! without copying payloads through userspace. Both sockets are duplicated via `dup` into
//! independent `OwnedFd`s registered with Tokio's `AsyncFd`, leaving the caller's original
//! `TcpStream`s untouched so any setup error (`dup` / `pipe2` / `AsyncFd`) falls back
//! cleanly to the bounded userspace copy path.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::io::unix::AsyncFd;
use tokio::net::TcpStream;
use tracing::debug;

use super::CancellationFlag;

const PIPE_SIZE: libc::c_int = 256 * 1024;
const DRAIN_SLICE: usize = 256 * 1024;
const NOTSENT_LOWAT: u32 = 1024 * 1024;
const BATCH_BYTES: usize = 4 * 1024 * 1024;
const BATCH_OPS: u32 = 64;

struct Pipe {
    r: OwnedFd,
    w: OwnedFd,
}

impl Pipe {
    fn new() -> io::Result<Self> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` is a valid writable 2-element array; `pipe2` writes two fresh file
        // descriptors on success and does not retain the pointer after returning.
        let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `pipe2` succeeded, so `fds[0]` and `fds[1]` are newly allocated, open,
        // unowned file descriptors that we immediately transfer into `OwnedFd`s.
        let pipe = unsafe {
            Self {
                r: OwnedFd::from_raw_fd(fds[0]),
                w: OwnedFd::from_raw_fd(fds[1]),
            }
        };
        // Best-effort enlarge kernel pipe buffer to 256 KiB; fall back to default on failure.
        // SAFETY: `pipe.r` is a valid open pipe file descriptor owned by `pipe`; `F_SETPIPE_SZ`
        // expects a single `c_int` byte capacity argument.
        let resize_rc = unsafe { libc::fcntl(pipe.r.as_raw_fd(), libc::F_SETPIPE_SZ, PIPE_SIZE) };
        if resize_rc < 0 {
            debug!(
                error = %io::Error::last_os_error(),
                requested = PIPE_SIZE,
                "F_SETPIPE_SZ failed; continuing with kernel default pipe capacity"
            );
        }
        Ok(pipe)
    }
}

struct SpliceSock {
    fd: Arc<AsyncFd<OwnedFd>>,
}

impl SpliceSock {
    fn dup(stream: &TcpStream) -> io::Result<Self> {
        let dup = socket2::SockRef::from(stream).try_clone()?;
        dup.set_nonblocking(true)?;
        let fd: OwnedFd = dup.into();
        // Best-effort set TCP_NOTSENT_LOWAT to avoid app-limited wakeups on high-BDP links.
        let lowat = NOTSENT_LOWAT;
        // SAFETY: `fd` is a valid open TCP socket owned by this function, and `&lowat` points
        // to an initialized `u32` whose byte length matches `size_of::<u32>()`.
        let lowat_rc = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::IPPROTO_TCP,
                libc::TCP_NOTSENT_LOWAT,
                (&lowat as *const u32).cast::<libc::c_void>(),
                std::mem::size_of::<u32>() as libc::socklen_t,
            )
        };
        if lowat_rc < 0 {
            debug!(
                error = %io::Error::last_os_error(),
                lowat = NOTSENT_LOWAT,
                "setsockopt(TCP_NOTSENT_LOWAT) failed; continuing with kernel default"
            );
        }
        // SAFETY: `fd` is an `OwnedFd` holding a valid, open non-blocking TCP socket
        // descriptor that remains open and unchanged for the entire lifetime of `AsyncFd`.
        let async_fd = unsafe { AsyncFd::register(fd)? };
        Ok(Self {
            fd: Arc::new(async_fd),
        })
    }

    fn raw_fd(&self) -> libc::c_int {
        self.fd.get_ref().as_raw_fd()
    }
}

fn splice_sys(fd_in: libc::c_int, fd_out: libc::c_int, len: usize) -> io::Result<usize> {
    // SAFETY: `fd_in` and `fd_out` are valid open non-blocking socket/pipe descriptors kept
    // alive by the caller's `OwnedFd`/`Arc<AsyncFd>`, and null offsets are valid because
    // sockets and pipes are non-seekable.
    let n = unsafe {
        libc::splice(
            fd_in,
            std::ptr::null_mut(),
            fd_out,
            std::ptr::null_mut(),
            len,
            libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK,
        )
    };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

fn shutdown_wr(sock: &SpliceSock) {
    // SAFETY: `sock.raw_fd()` is a valid open socket file descriptor kept alive by `sock`.
    unsafe {
        libc::shutdown(sock.raw_fd(), libc::SHUT_WR);
    }
}

fn shutdown_rdwr(sock: &SpliceSock) {
    // SAFETY: `sock.raw_fd()` is a valid open socket file descriptor kept alive by `sock`.
    unsafe {
        libc::shutdown(sock.raw_fd(), libc::SHUT_RDWR);
    }
}

fn teardown(a: &SpliceSock, b: &SpliceSock) {
    shutdown_rdwr(a);
    shutdown_rdwr(b);
}

struct AbortOnDropHandle<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDropHandle<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn pump_one_direction(
    in_sock: Arc<SpliceSock>,
    out_sock: Arc<SpliceSock>,
    pipe: Pipe,
    bytes_counter: Arc<AtomicU64>,
    total_counter: Arc<AtomicU64>,
    cancel: CancellationFlag,
) {
    let pr = pipe.r.as_raw_fd();
    let pw = pipe.w.as_raw_fd();

    let cancel_wait = cancel.cancelled();
    tokio::pin!(cancel_wait);

    let mut batch_bytes: usize = 0;
    let mut batch_ops: u32 = 0;

    loop {
        if cancel.is_cancelled() {
            teardown(&in_sock, &out_sock);
            return;
        }

        // Fast non-blocking fill: socket -> pipe.
        let n = match splice_sys(in_sock.raw_fd(), pw, PIPE_SIZE as usize) {
            Ok(0) => {
                // Clean read EOF with empty pipe: propagate TCP half-close to peer write side
                // without cancelling the reverse direction.
                shutdown_wr(&out_sock);
                return;
            }
            Ok(n) => {
                batch_ops += 1;
                n
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                let mut guard = match tokio::select! {
                    biased;
                    _ = &mut cancel_wait => {
                        teardown(&in_sock, &out_sock);
                        return;
                    }
                    res = in_sock.fd.readable() => res,
                } {
                    Ok(g) => g,
                    Err(_) => {
                        cancel.cancel();
                        teardown(&in_sock, &out_sock);
                        return;
                    }
                };
                guard.clear_ready();
                batch_bytes = 0;
                batch_ops = 0;
                continue;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                // Hard socket read error (e.g. ECONNRESET): tear down both directions promptly.
                cancel.cancel();
                teardown(&in_sock, &out_sock);
                return;
            }
        };

        // Drain pipe -> out_sock until all `n` bytes are delivered.
        let mut delivered: usize = 0;
        while delivered < n {
            if cancel.is_cancelled() {
                teardown(&in_sock, &out_sock);
                return;
            }
            match splice_sys(pr, out_sock.raw_fd(), (n - delivered).min(DRAIN_SLICE)) {
                Ok(0) => {
                    cancel.cancel();
                    teardown(&in_sock, &out_sock);
                    return;
                }
                Ok(m) => {
                    delivered += m;
                    let delta = m as u64;
                    bytes_counter.fetch_add(delta, Ordering::Relaxed);
                    total_counter.fetch_add(delta, Ordering::Relaxed);
                    batch_ops += 1;
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    let mut guard = match tokio::select! {
                        biased;
                        _ = &mut cancel_wait => {
                            teardown(&in_sock, &out_sock);
                            return;
                        }
                        res = out_sock.fd.writable() => res,
                    } {
                        Ok(g) => g,
                        Err(_) => {
                            cancel.cancel();
                            teardown(&in_sock, &out_sock);
                            return;
                        }
                    };
                    guard.clear_ready();
                    batch_bytes = 0;
                    batch_ops = 0;
                    continue;
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    // Hard socket write error (e.g. EPIPE / ECONNRESET): tear down both directions.
                    cancel.cancel();
                    teardown(&in_sock, &out_sock);
                    return;
                }
            }
        }

        batch_bytes += n;
        // Cooperative yield to prevent saturated splice pumps from starving sibling tasks.
        if batch_bytes >= BATCH_BYTES || batch_ops >= BATCH_OPS {
            tokio::task::yield_now().await;
            batch_bytes = 0;
            batch_ops = 0;
        }
    }
}

/// Forward data bidirectionally between `a` and `b` using Linux `splice(2)` zero-copy pipes.
///
/// Returns `Err` only if setup (`dup`, `pipe2`, or `AsyncFd` registration) fails, leaving
/// `a` and `b` untouched so the caller can fall back cleanly to userspace copy.
pub async fn splice_bidirectional(
    a: &TcpStream,
    b: &TcpStream,
    bytes_a_to_b: Arc<AtomicU64>,
    bytes_b_to_a: Arc<AtomicU64>,
    total_a_to_b: Arc<AtomicU64>,
    total_b_to_a: Arc<AtomicU64>,
    cancel: CancellationFlag,
) -> io::Result<()> {
    let sa = Arc::new(SpliceSock::dup(a)?);
    let sb = Arc::new(SpliceSock::dup(b)?);
    let p_ab = Pipe::new()?;
    let p_ba = Pipe::new()?;

    let mut g1 = AbortOnDropHandle(tokio::spawn(pump_one_direction(
        Arc::clone(&sa),
        Arc::clone(&sb),
        p_ab,
        bytes_a_to_b,
        total_a_to_b,
        cancel.clone(),
    )));
    let mut g2 = AbortOnDropHandle(tokio::spawn(pump_one_direction(
        Arc::clone(&sb),
        Arc::clone(&sa),
        p_ba,
        bytes_b_to_a,
        total_b_to_a,
        cancel.clone(),
    )));

    tokio::select! {
        r1 = &mut g1.0 => {
            if r1.is_err() {
                cancel.cancel();
                teardown(&sa, &sb);
                return Ok(());
            }
            let _ = (&mut g2.0).await;
        }
        r2 = &mut g2.0 => {
            if r2.is_err() {
                cancel.cancel();
                teardown(&sa, &sb);
                return Ok(());
            }
            let _ = (&mut g1.0).await;
        }
    }

    Ok(())
}
