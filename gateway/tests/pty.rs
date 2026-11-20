//! RG28: `io::open_serial` opens a PTY slave, which has no modem lines
//! (TIOCMBIC fails with ENOTTY), and carries bytes both ways, so a PTY
//! can be a local serial link (mw-posix-node's
//! `MW_POSIX_NODE_LOCAL_SERIAL`).
#![cfg(target_os = "linux")]

use embedded_io_async::{Read, Write};
use mw_gateway::io::open_serial;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

/// A PTY pair: the master as a file, the slave's path, and the slave's
/// fd, kept open so the master never sees a hangup.
fn openpty() -> (std::fs::File, String, OwnedFd) {
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: openpty writes two fds into the given ints; the name,
    // termios and winsize arguments may be null.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(rc, 0, "openpty: {}", io::Error::last_os_error());
    // SAFETY: both fds were just opened by openpty and nothing else owns
    // them.
    let (master, slave) = unsafe {
        (
            std::fs::File::from_raw_fd(master),
            OwnedFd::from_raw_fd(slave),
        )
    };
    let path = std::fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd())).unwrap();
    (master, path.to_str().unwrap().to_owned(), slave)
}

#[tokio::test]
async fn rg28_open_serial_takes_a_pty_slave_and_carries_bytes_both_ways() {
    let (mut master, path, _slave) = openpty();
    let mut link = open_serial(&path, 115_200).expect("a PTY slave opens");

    io::Write::write_all(&mut master, b"to the link").unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 32];
    while got.len() < 11 {
        let n = tokio::time::timeout(Duration::from_secs(5), link.read(&mut buf))
            .await
            .expect("bytes from the master")
            .unwrap();
        got.extend_from_slice(&buf[..n]);
    }
    assert_eq!(got, b"to the link");

    link.write_all(b"to the master").await.unwrap();
    let back = tokio::task::spawn_blocking(move || {
        let mut got = Vec::new();
        let mut buf = [0u8; 32];
        while got.len() < 13 {
            let n = io::Read::read(&mut master, &mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
        }
        got
    });
    let back = tokio::time::timeout(Duration::from_secs(5), back)
        .await
        .expect("bytes on the master")
        .unwrap();
    assert_eq!(back, b"to the master");
}
