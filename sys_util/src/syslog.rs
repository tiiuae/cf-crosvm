// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Facilities for sending log message to syslog.
//!
//! Every function exported by this module is thread-safe. Each function will silently fail until
//! `syslog::init()` is called and returns `Ok`.
//!
//! # Examples
//!
//! ```
//! use sys_util::{error, syslog, warn};
//!
//! fn main() {
//!     if let Err(e) = syslog::init() {
//!         println!("failed to initiailize syslog: {}", e);
//!         return;
//!     }
//!     warn!("this is your {} warning", "final");
//!     error!("something went horribly wrong: {}", "out of RAMs");
//! }
//! ```

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt::{self, Display};
use std::fs::File;
use std::io;
use std::io::{stderr, Cursor, ErrorKind, Write};
use std::mem;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::ptr::null;
use std::sync::{MutexGuard, Once};

use libc::{
    closelog, fcntl, localtime_r, openlog, time, time_t, tm, F_GETFD, LOG_NDELAY, LOG_PERROR,
    LOG_PID, LOG_USER,
};

use sync::Mutex;

use crate::getpid;

const SYSLOG_PATH: &str = "/dev/log";

/// The priority (i.e. severity) of a syslog message.
///
/// See syslog man pages for information on their semantics.
#[derive(Copy, Clone, Debug)]
pub enum Priority {
    Emergency = 0,
    Alert = 1,
    Critical = 2,
    Error = 3,
    Warning = 4,
    Notice = 5,
    Info = 6,
    Debug = 7,
}

impl Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::Priority::*;

        let string = match self {
            Emergency => "EMERGENCY",
            Alert => "ALERT",
            Critical => "CRITICAL",
            Error => "ERROR",
            Warning => "WARNING",
            Notice => "NOTICE",
            Info => "INFO",
            Debug => "DEBUG",
        };

        write!(f, "{}", string)
    }
}

/// The facility of a syslog message.
///
/// See syslog man pages for information on their semantics.
#[derive(Copy, Clone)]
pub enum Facility {
    Kernel = 0,
    User = 1 << 3,
    Mail = 2 << 3,
    Daemon = 3 << 3,
    Auth = 4 << 3,
    Syslog = 5 << 3,
    Lpr = 6 << 3,
    News = 7 << 3,
    Uucp = 8 << 3,
    Local0 = 16 << 3,
    Local1 = 17 << 3,
    Local2 = 18 << 3,
    Local3 = 19 << 3,
    Local4 = 20 << 3,
    Local5 = 21 << 3,
    Local6 = 22 << 3,
    Local7 = 23 << 3,
}

/// Errors returned by `syslog::init()`.
#[derive(Debug)]
pub enum Error {
    /// Initialization was never attempted.
    NeverInitialized,
    /// Initialization has previously failed and can not be retried.
    Poisoned,
    /// Error while creating socket.
    Socket(io::Error),
    /// Error while attempting to connect socket.
    Connect(io::Error),
    // There was an error using `open` to get the lowest file descriptor.
    GetLowestFd(io::Error),
    // The guess of libc's file descriptor for the syslog connection was invalid.
    InvalidFd,
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::Error::*;

        match self {
            NeverInitialized => write!(f, "initialization was never attempted"),
            Poisoned => write!(f, "initialization previously failed and cannot be retried"),
            Socket(e) => write!(f, "failed to create socket: {}", e),
            Connect(e) => write!(f, "failed to connect socket: {}", e),
            GetLowestFd(e) => write!(f, "failed to get lowest file descriptor: {}", e),
            InvalidFd => write!(f, "guess of fd for syslog connection was invalid"),
        }
    }
}

fn get_proc_name() -> Option<String> {
    env::args_os()
        .next()
        .map(PathBuf::from)
        .and_then(|s| s.file_name().map(OsStr::to_os_string))
        .map(OsString::into_string)
        .and_then(Result::ok)
}

// Uses libc's openlog function to get a socket to the syslogger. By getting the socket this way, as
// opposed to connecting to the syslogger directly, libc's internal state gets initialized for other
// libraries (e.g. minijail) that make use of libc's syslog function. Note that this function
// depends on no other threads or signal handlers being active in this process because they might
// create FDs.
//
// TODO(zachr): Once https://android-review.googlesource.com/470998 lands, there won't be any
// libraries in use that hard depend on libc's syslogger. Remove this and go back to making the
// connection directly once minjail is ready.
fn openlog_and_get_socket() -> Result<UnixDatagram, Error> {
    // closelog first in case there was already a file descriptor open.  Safe because it takes no
    // arguments and just closes an open file descriptor.  Does nothing if the file descriptor
    // was not already open.
    unsafe {
        closelog();
    }

    // Ordinarily libc's FD for the syslog connection can't be accessed, but we can guess that the
    // FD that openlog will be getting is the lowest unused FD. To guarantee that an FD is opened in
    // this function we use the LOG_NDELAY to tell openlog to connect to the syslog now. To get the
    // lowest unused FD, we open a dummy file (which the manual says will always return the lowest
    // fd), and then close that fd. Voilà, we now know the lowest numbered FD. The call to openlog
    // will make use of that FD, and then we just wrap a `UnixDatagram` around it for ease of use.
    let fd = File::open("/dev/null")
        .map_err(Error::GetLowestFd)?
        .as_raw_fd();

    unsafe {
        // Safe because openlog accesses no pointers because `ident` is null, only valid flags are
        // used, and it returns no error.
        openlog(null(), LOG_NDELAY | LOG_PERROR | LOG_PID, LOG_USER);
        // For safety, ensure the fd we guessed is valid. The `fcntl` call itself only reads the
        // file descriptor table of the current process, which is trivially safe.
        if fcntl(fd, F_GETFD) >= 0 {
            Ok(UnixDatagram::from_raw_fd(fd))
        } else {
            Err(Error::InvalidFd)
        }
    }
}

struct State {
    stderr: bool,
    socket: Option<UnixDatagram>,
    file: Option<File>,
    proc_name: Option<String>,
}

impl State {
    fn new() -> Result<State, Error> {
        let s = openlog_and_get_socket()?;
        Ok(State {
            stderr: true,
            socket: Some(s),
            file: None,
            proc_name: get_proc_name(),
        })
    }
}

static STATE_ONCE: Once = Once::new();
static mut STATE: *const Mutex<State> = 0 as *const _;

fn new_mutex_ptr<T>(inner: T) -> *const Mutex<T> {
    Box::into_raw(Box::new(Mutex::new(inner)))
}

/// Initialize the syslog connection and internal variables.
///
/// This should only be called once per process before any other threads have been spawned or any
/// signal handlers have been registered. Every call made after the first will have no effect
/// besides return `Ok` or `Err` appropriately.
pub fn init() -> Result<(), Error> {
    let mut err = Error::Poisoned;
    STATE_ONCE.call_once(|| match State::new() {
        // Safe because STATE mutation is guarded by `Once`.
        Ok(state) => unsafe { STATE = new_mutex_ptr(state) },
        Err(e) => err = e,
    });

    if unsafe { STATE.is_null() } {
        Err(err)
    } else {
        Ok(())
    }
}

fn lock() -> Result<MutexGuard<'static, State>, Error> {
    // Safe because we assume that STATE is always in either a valid or NULL state.
    let state_ptr = unsafe { STATE };
    if state_ptr.is_null() {
        return Err(Error::NeverInitialized);
    }
    // Safe because STATE only mutates once and we checked for NULL.
    let state = unsafe { &*state_ptr };
    let guard = state.lock();
    Ok(guard)
}

// Attempts to lock and retrieve the state. Returns from the function silently on failure.
macro_rules! lock {
    () => {
        match lock() {
            Ok(s) => s,
            _ => return,
        };
    };
}

/// Replaces the process name reported in each syslog message.
///
/// The default process name is the _file name_ of `argv[0]`. For example, if this program was
/// invoked as
///
/// ```bash
/// $ path/to/app --delete everything
/// ```
///
/// the default process name would be _app_.
///
/// Does nothing if syslog was never initialized.
pub fn set_proc_name<T: Into<String>>(proc_name: T) {
    let mut state = lock!();
    state.proc_name = Some(proc_name.into());
}

/// Enables or disables echoing log messages to the syslog.
///
/// The default behavior is **enabled**.
///
/// If `enable` goes from `true` to `false`, the syslog connection is closed. The connection is
/// reopened if `enable` is set to `true` after it became `false`.
///
/// Returns an error if syslog was never initialized or the syslog connection failed to be
/// established.
///
/// # Arguments
/// * `enable` - `true` to enable echoing to syslog, `false` to disable echoing to syslog.
pub fn echo_syslog(enable: bool) -> Result<(), Error> {
    let state_ptr = unsafe { STATE };
    if state_ptr.is_null() {
        return Err(Error::NeverInitialized);
    }
    let mut state = lock().map_err(|_| Error::Poisoned)?;

    match state.socket.take() {
        Some(_) if enable => {}
        Some(s) => {
            // Because `openlog_and_get_socket` actually just "borrows" the syslog FD, this module
            // does not own the syslog connection and therefore should not destroy it.
            mem::forget(s);
        }
        None if enable => {
            let s = openlog_and_get_socket()?;
            state.socket = Some(s);
        }
        _ => {}
    }
    Ok(())
}

/// Replaces the optional `File` to echo log messages to.
///
/// The default behavior is to not echo to a file. Passing `None` to this function restores that
/// behavior.
///
/// Does nothing if syslog was never initialized.
///
/// # Arguments
/// * `file` - `Some(file)` to echo to `file`, `None` to disable echoing to the file previously passed to `echo_file`.
pub fn echo_file(file: Option<File>) {
    let mut state = lock!();
    state.file = file;
}

/// Enables or disables echoing log messages to the `std::io::stderr()`.
///
/// The default behavior is **enabled**.
///
/// Does nothing if syslog was never initialized.
///
/// # Arguments
/// * `enable` - `true` to enable echoing to stderr, `false` to disable echoing to stderr.
pub fn echo_stderr(enable: bool) {
    let mut state = lock!();
    state.stderr = enable;
}

/// Retrieves the file descriptors owned by the global syslogger.
///
/// Does nothing if syslog was never initialized. If their are any file descriptors, they will be
/// pushed into `fds`.
///
/// Note that the `stderr` file descriptor is never added, as it is not owned by syslog.
pub fn push_fds(fds: &mut Vec<RawFd>) {
    let state = lock!();
    fds.extend(state.socket.iter().map(|s| s.as_raw_fd()));
    fds.extend(state.file.iter().map(|f| f.as_raw_fd()));
}

/// Should only be called after `init()` was called.
fn send_buf(socket: &UnixDatagram, buf: &[u8]) {
    const SEND_RETRY: usize = 2;

    for _ in 0..SEND_RETRY {
        match socket.send(&buf[..]) {
            Ok(_) => break,
            Err(e) => match e.kind() {
                ErrorKind::ConnectionRefused
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::NotConnected => {
                    let res = socket.connect(SYSLOG_PATH);
                    if res.is_err() {
                        break;
                    }
                }
                _ => {}
            },
        }
    }
}

fn get_localtime() -> tm {
    unsafe {
        // Safe because tm is just a struct of plain data.
        let mut tm: tm = mem::zeroed();
        let mut now: time_t = 0;
        // Safe because we give time a valid pointer and can never fail.
        time(&mut now as *mut _);
        // Safe because we give localtime_r valid pointers and can never fail.
        localtime_r(&now, &mut tm as *mut _);
        tm
    }
}

/// Records a log message with the given details.
///
/// Note that this will fail silently if syslog was not initialized.
///
/// # Arguments
/// * `pri` - The `Priority` (i.e. severity) of the log message.
/// * `fac` - The `Facility` of the log message. Usually `Facility::User` should be used.
/// * `file_line` - Optional tuple of the name of the file that generated the
///                 log and the line number within that file.
/// * `args` - The log's message to record, in the form of `format_args!()`  return value
///
/// # Examples
///
/// ```
/// # use sys_util::syslog;
/// # fn main() {
/// #   if let Err(e) = syslog::init() {
/// #       println!("failed to initiailize syslog: {}", e);
/// #       return;
/// #   }
/// syslog::log(syslog::Priority::Error,
///             syslog::Facility::User,
///             Some((file!(), line!())),
///             format_args!("hello syslog"));
/// # }
/// ```
pub fn log(pri: Priority, fac: Facility, file_line: Option<(&str, u32)>, args: fmt::Arguments) {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let mut state = lock!();
    let mut buf = [0u8; 1024];
    if let Some(socket) = &state.socket {
        let tm = get_localtime();
        let prifac = (pri as u8) | (fac as u8);
        let res = {
            let mut buf_cursor = Cursor::new(&mut buf[..]);
            write!(
                &mut buf_cursor,
                "<{}>{} {:02} {:02}:{:02}:{:02} {}[{}]: ",
                prifac,
                MONTHS[tm.tm_mon as usize],
                tm.tm_mday,
                tm.tm_hour,
                tm.tm_min,
                tm.tm_sec,
                state.proc_name.as_ref().map(|s| s.as_ref()).unwrap_or("-"),
                getpid()
            )
            .and_then(|()| {
                if let Some((file_name, line)) = &file_line {
                    write!(&mut buf_cursor, " [{}:{}] ", file_name, line)
                } else {
                    Ok(())
                }
            })
            .and_then(|()| write!(&mut buf_cursor, "{}", args))
            .map(|()| buf_cursor.position() as usize)
        };

        if let Ok(len) = &res {
            send_buf(&socket, &buf[..*len])
        }
    }

    let res = {
        let mut buf_cursor = Cursor::new(&mut buf[..]);
        if let Some((file_name, line)) = &file_line {
            write!(&mut buf_cursor, "[{}:{}:{}] ", pri, file_name, line)
        } else {
            Ok(())
        }
        .and_then(|()| writeln!(&mut buf_cursor, "{}", args))
        .map(|()| buf_cursor.position() as usize)
    };
    if let Ok(len) = &res {
        if let Some(file) = &mut state.file {
            let _ = file.write_all(&buf[..*len]);
        }
        if state.stderr {
            let _ = stderr().write_all(&buf[..*len]);
        }
    }
}

/// A macro for logging at an arbitrary priority level.
///
/// Note that this will fail silently if syslog was not initialized.
#[macro_export]
macro_rules! log {
    ($pri:expr, $($args:tt)+) => ({
        $crate::syslog::log($pri, $crate::syslog::Facility::User, Some((file!(), line!())), format_args!($($args)+))
    })
}

/// A macro for logging an error.
///
/// Note that this will fail silently if syslog was not initialized.
#[macro_export]
macro_rules! error {
    ($($args:tt)+) => ($crate::log!($crate::syslog::Priority::Error, $($args)*))
}

/// A macro for logging a warning.
///
/// Note that this will fail silently if syslog was not initialized.
#[macro_export]
macro_rules! warn {
    ($($args:tt)+) => ($crate::log!($crate::syslog::Priority::Warning, $($args)*))
}

/// A macro for logging info.
///
/// Note that this will fail silently if syslog was not initialized.
#[macro_export]
macro_rules! info {
    ($($args:tt)+) => ($crate::log!($crate::syslog::Priority::Info, $($args)*))
}

/// A macro for logging debug information.
///
/// Note that this will fail silently if syslog was not initialized.
#[macro_export]
macro_rules! debug {
    ($($args:tt)+) => ($crate::log!($crate::syslog::Priority::Debug, $($args)*))
}

// Struct that implements io::Write to be used for writing directly to the syslog
pub struct Syslogger {
    buf: String,
    priority: Priority,
    facility: Facility,
}

impl Syslogger {
    pub fn new(p: Priority, f: Facility) -> Syslogger {
        Syslogger {
            buf: String::new(),
            priority: p,
            facility: f,
        }
    }
}

impl io::Write for Syslogger {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let parsed_str = String::from_utf8_lossy(buf);
        self.buf.push_str(&parsed_str);

        if let Some(last_newline_idx) = self.buf.rfind('\n') {
            for line in self.buf[..last_newline_idx].lines() {
                log(self.priority, self.facility, None, format_args!("{}", line));
            }

            self.buf.drain(..=last_newline_idx);
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use libc::{shm_open, shm_unlink, O_CREAT, O_EXCL, O_RDWR};

    use std::ffi::CStr;
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::io::FromRawFd;

    #[test]
    fn init_syslog() {
        init().unwrap();
    }

    #[test]
    fn fds() {
        init().unwrap();
        let mut fds = Vec::new();
        push_fds(&mut fds);
        assert!(fds.len() >= 1);
        for fd in fds {
            assert!(fd >= 0);
        }
    }

    #[test]
    fn syslog_log() {
        init().unwrap();
        log(
            Priority::Error,
            Facility::User,
            Some((file!(), line!())),
            format_args!("hello syslog"),
        );
    }

    #[test]
    fn proc_name() {
        init().unwrap();
        log(
            Priority::Error,
            Facility::User,
            Some((file!(), line!())),
            format_args!("before proc name"),
        );
        set_proc_name("sys_util-test");
        log(
            Priority::Error,
            Facility::User,
            Some((file!(), line!())),
            format_args!("after proc name"),
        );
    }

    #[test]
    fn syslog_file() {
        init().unwrap();
        let shm_name = CStr::from_bytes_with_nul(b"/crosvm_shm\0").unwrap();
        let mut file = unsafe {
            shm_unlink(shm_name.as_ptr());
            let fd = shm_open(shm_name.as_ptr(), O_RDWR | O_CREAT | O_EXCL, 0666);
            assert!(fd >= 0, "error creating shared memory;");
            shm_unlink(shm_name.as_ptr());
            File::from_raw_fd(fd)
        };

        let syslog_file = file.try_clone().expect("error cloning shared memory file");
        echo_file(Some(syslog_file));

        const TEST_STR: &'static str = "hello shared memory file";
        log(
            Priority::Error,
            Facility::User,
            Some((file!(), line!())),
            format_args!("{}", TEST_STR),
        );

        file.seek(SeekFrom::Start(0))
            .expect("error seeking shared memory file");
        let mut buf = String::new();
        file.read_to_string(&mut buf)
            .expect("error reading shared memory file");
        assert!(buf.contains(TEST_STR));
    }

    #[test]
    fn macros() {
        init().unwrap();
        error!("this is an error {}", 3);
        warn!("this is a warning {}", "uh oh");
        info!("this is info {}", true);
        debug!("this is debug info {:?}", Some("helpful stuff"));
    }

    #[test]
    fn syslogger_char() {
        init().unwrap();
        let mut syslogger = Syslogger::new(Priority::Info, Facility::Daemon);

        let string = "Writing chars to syslog";
        for c in string.chars() {
            syslogger.write(&[c as u8]).expect("error writing char");
        }

        syslogger
            .write(&['\n' as u8])
            .expect("error writing newline char");
    }

    #[test]
    fn syslogger_line() {
        init().unwrap();
        let mut syslogger = Syslogger::new(Priority::Info, Facility::Daemon);

        let s = "Writing string to syslog\n";
        syslogger
            .write(&s.as_bytes())
            .expect("error writing string");
    }

    #[test]
    fn syslogger_partial() {
        init().unwrap();
        let mut syslogger = Syslogger::new(Priority::Info, Facility::Daemon);

        let s = "Writing partial string";
        // Should not log because there is no newline character
        syslogger
            .write(&s.as_bytes())
            .expect("error writing string");
    }
}
