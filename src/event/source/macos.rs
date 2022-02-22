use libc::{c_int, fd_set, FD_ISSET, FD_SET, FD_SETSIZE, FD_ZERO};
use mio::{net::UnixStream, unix::SourceFd, Interest, Token};
use std::{
    borrow::Borrow,
    cmp,
    collections::HashMap,
    fmt, io, mem,
    os::unix::io::{AsRawFd, RawFd},
    ptr,
    sync::Mutex,
    time::Duration,
};

pub struct Poll {
    registry: Registry,
}

pub struct Registry {
    selector: Mutex<PosixSelect>,
}

struct PosixSelect {
    read_fds: HashMap<RawFd, Token>,
    write_fds: HashMap<RawFd, Token>,
}

pub trait HasRawFd {
    fn raw_fd(&self) -> RawFd;
}

impl HasRawFd for SourceFd<'_> {
    fn raw_fd(&self) -> RawFd {
        *self.0
    }
}

impl HasRawFd for Signals {
    fn raw_fd(&self) -> RawFd {
        self.0.get_read().as_raw_fd()
    }
}

impl Poll {
    pub fn new() -> io::Result<Poll> {
        PosixSelect::new().map(|selector| Poll {
            registry: Registry {
                selector: Mutex::new(selector),
            },
        })
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn poll(&mut self, events: &mut Events, timeout: Option<Duration>) -> io::Result<()> {
        self.registry
            .selector
            .lock()
            .unwrap()
            .select(events, timeout)
    }
}

impl fmt::Debug for Poll {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Poll").finish()
    }
}

impl Registry {
    pub fn register<S>(&self, source: &mut S, token: Token, interests: Interest) -> io::Result<()>
    where
        S: HasRawFd + ?Sized,
    {
        self.selector
            .lock()
            .unwrap()
            .register(source.raw_fd(), token, interests)
    }

    pub fn reregister<S>(
        &mut self,
        source: &mut S,
        token: Token,
        interests: Interest,
    ) -> io::Result<()>
    where
        S: HasRawFd + ?Sized,
    {
        self.selector
            .lock()
            .unwrap()
            .reregister(source.raw_fd(), token, interests)
    }

    pub fn deregister<S>(&mut self, source: &mut S) -> io::Result<()>
    where
        S: HasRawFd + ?Sized,
    {
        self.selector.lock().unwrap().deregister(source.raw_fd())
    }
}

impl PosixSelect {
    fn new() -> io::Result<PosixSelect> {
        Ok(PosixSelect {
            read_fds: HashMap::new(),
            write_fds: HashMap::new(),
        })
    }

    fn select(&self, events: &mut Events, timeout: Option<Duration>) -> io::Result<()> {
        let timeout = timeout
            .map(|to| libc::timeval {
                tv_sec: cmp::min(to.as_secs(), libc::time_t::max_value() as u64) as libc::time_t,
                tv_usec: libc::c_int::from((to.subsec_nanos() / 1000u32) as i32),
            })
            .as_mut()
            .map(|s| s as *mut _)
            .unwrap_or(ptr::null_mut());

        let mut rfds: fd_set = unsafe { mem::MaybeUninit::uninit().assume_init() };
        let mut wfds: fd_set = unsafe { mem::MaybeUninit::uninit().assume_init() };

        unsafe {
            FD_ZERO(&mut rfds);
            FD_ZERO(&mut wfds);
        }

        let mut nfds: libc::c_int = 0;

        for (&fd, _) in self.read_fds.iter() {
            if nfds < fd {
                nfds = fd;
            }
            unsafe { FD_SET(fd, &mut rfds) };
        }

        for (&fd, _) in self.write_fds.iter() {
            if nfds < fd {
                nfds = fd;
            }
            unsafe { FD_SET(fd, &mut wfds) };
        }

        nfds += 1;

        let ret = unsafe { libc::select(nfds, &mut rfds, &mut wfds, ptr::null_mut(), timeout) };

        if ret == -1 {
            return Err(io::Error::last_os_error());
        }

        events.clear();

        if ret > 0 {
            for (&fd, _) in self.read_fds.iter() {
                if unsafe { FD_ISSET(fd, &rfds) } {
                    events.push(Event {
                        fd,
                        token: self.read_fds.get(&fd).unwrap().clone(),
                    });
                }
            }

            for (&fd, _) in self.write_fds.iter() {
                if unsafe { FD_ISSET(fd, &wfds) } {
                    events.push(Event {
                        fd,
                        token: self.read_fds.get(&fd).unwrap().clone(),
                    });
                }
            }
        }

        Ok(())
    }

    fn register(&mut self, fd: RawFd, token: Token, interests: Interest) -> io::Result<()> {
        if fd >= FD_SETSIZE as RawFd {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fd greater than FD_SETSIZE",
            ));
        }

        if interests.is_readable() && self.read_fds.contains_key(&fd)
            || interests.is_writable() && self.write_fds.contains_key(&fd)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "fd already exists",
            ));
        }

        if interests.is_readable() {
            self.read_fds.insert(fd, token);
        }

        if interests.is_writable() {
            self.write_fds.insert(fd, token);
        }

        Ok(())
    }

    pub fn reregister(&mut self, fd: RawFd, token: Token, interests: Interest) -> io::Result<()> {
        if interests.is_readable() {
            self.read_fds.insert(fd, token);
        }

        if interests.is_writable() {
            self.write_fds.insert(fd, token);
        }

        Ok(())
    }

    fn deregister(&mut self, fd: RawFd) -> io::Result<()> {
        self.read_fds.remove(&fd);
        self.write_fds.remove(&fd);

        Ok(())
    }
}

pub type Events = Vec<Event>;

pub struct Event {
    fd: RawFd,
    token: Token,
}

impl Event {
    pub fn token(&self) -> Token {
        self.token
    }
    pub fn fd(&self) -> RawFd {
        self.fd
    }
}

use signal_hook::iterator::backend::{self, SignalDelivery};
use signal_hook::iterator::exfiltrator::SignalOnly;

pub struct Signals(SignalDelivery<UnixStream, SignalOnly>);

pub use backend::Pending;

impl Signals {
    pub fn new<I, S>(signals: I) -> Result<Self, io::Error>
    where
        I: IntoIterator<Item = S>,
        S: Borrow<c_int>,
    {
        let (read, write) = UnixStream::pair()?;
        let delivery = SignalDelivery::with_pipe(read, write, SignalOnly::default(), signals)?;
        Ok(Self(delivery))
    }

    pub fn add_signal(&self, signal: c_int) -> Result<(), io::Error> {
        self.0.handle().add_signal(signal)
    }

    pub fn pending(&mut self) -> Pending<SignalOnly> {
        self.0.pending()
    }
}
