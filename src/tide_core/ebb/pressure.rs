//! Optional Linux PSI event source. No worker, service manager or vendor tool.
use crate::config::EbbMode;
use smithay::reexports::calloop::RegistrationToken;
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
        unix::fs::OpenOptionsExt,
    },
    path::{Path, PathBuf},
};

#[derive(Default)]
pub(crate) struct PressureState {
    pub mode: Option<EbbMode>,
    pub registration: Option<RegistrationToken>,
    pub source: Option<String>,
}

pub(crate) struct PressureWatch {
    epoll: OwnedFd,
    _pressure: File,
    pub source: String,
}
impl AsFd for PressureWatch {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.epoll.as_fd()
    }
}

impl PressureWatch {
    pub fn open(mode: EbbMode) -> io::Result<Self> {
        let trigger = match mode {
            EbbMode::Aggressive => b"some 100000 2000000\0".as_slice(),
            _ => b"some 200000 2000000\0".as_slice(),
        };
        let mut candidates = Vec::new();
        if let Some(path) = std::env::var_os("MEMORY_PRESSURE_WATCH") {
            let payload = std::env::var("MEMORY_PRESSURE_WRITE")
                .ok()
                .and_then(|value| decode_trigger(&value))
                .unwrap_or_else(|| trigger.to_vec());
            candidates.push((PathBuf::from(path), payload));
        }
        if let Ok(cgroups) = std::fs::read_to_string("/proc/self/cgroup") {
            if let Some(group) = cgroups.lines().find_map(|line| line.strip_prefix("0::")) {
                // Membership is kernel-provided, still reject traversal before
                // joining the standard unified-hierarchy mount.
                if !Path::new(group)
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
                {
                    candidates.push((
                        Path::new("/sys/fs/cgroup")
                            .join(group.trim_start_matches('/'))
                            .join("memory.pressure"),
                        trigger.to_vec(),
                    ));
                }
            }
        }
        candidates.push((PathBuf::from("/proc/pressure/memory"), trigger.to_vec()));
        for (path, payload) in candidates {
            if let Ok(watch) = Self::at(&path, &payload) {
                return Ok(watch);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "PSI triggers unavailable; using sampled memory availability",
        ))
    }

    fn at(path: &Path, trigger: &[u8]) -> io::Result<Self> {
        let mut pressure = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        // Only write a trigger to the kernel PSI interface, never an arbitrary
        // path supplied through an environment variable.
        let canonical = path.canonicalize()?;
        let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // Safety: fstatfs writes one correctly-sized stack value for our owned fd.
        if unsafe { libc::fstatfs(pressure.as_raw_fd(), filesystem.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // Safety: fstatfs succeeded and initialized the structure.
        let kind = unsafe { filesystem.assume_init() }.f_type;
        if !((kind == libc::CGROUP2_SUPER_MAGIC
            && canonical
                .file_name()
                .is_some_and(|name| name == "memory.pressure"))
            || (kind == libc::PROC_SUPER_MAGIC && canonical == Path::new("/proc/pressure/memory")))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a kernel memory PSI interface",
            ));
        }
        pressure.write_all(trigger)?;
        // calloop's generic READ interest cannot request POLLPRI. A private
        // epoll fd translates the kernel's priority notifications to READ.
        // Safety: creates a new CLOEXEC descriptor, checked before ownership.
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // Safety: fd is a newly-created, uniquely-owned descriptor.
        let epoll = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut event = libc::epoll_event {
            events: (libc::EPOLLPRI | libc::EPOLLERR | libc::EPOLLHUP | libc::EPOLLET) as u32,
            u64: 1,
        };
        // Safety: both fds are owned for the entire watch lifetime; the event
        // pointer is valid and epoll_ctl copies it synchronously.
        if unsafe {
            libc::epoll_ctl(
                epoll.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                pressure.as_raw_fd(),
                &mut event,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            epoll,
            _pressure: pressure,
            source: canonical.display().to_string(),
        })
    }

    pub fn notified(&self) -> io::Result<bool> {
        let mut event = libc::epoll_event { events: 0, u64: 0 };
        // Safety: writable space for one event, owned live fd, nonblocking timeout.
        let count = unsafe { libc::epoll_wait(self.epoll.as_raw_fd(), &mut event, 1, 0) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        if count == 0 {
            return Ok(false);
        }
        if event.events & (libc::EPOLLERR | libc::EPOLLHUP) as u32 != 0 {
            return Err(io::Error::other("PSI monitor closed"));
        }
        Ok(event.events & libc::EPOLLPRI as u32 != 0)
    }
}

/// systemd's optional WRITE payload is base64; accepting it does not require
/// systemd. A PSI trigger is tiny, so bound both encoded and decoded data.
fn decode_trigger(value: &str) -> Option<Vec<u8>> {
    if value.len() > 512 || value.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::new();
    for (index, block) in value.as_bytes().chunks(4).enumerate() {
        let mut bits = 0_u32;
        let mut padding = 0;
        for byte in block {
            let number = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' => {
                    padding += 1;
                    0
                }
                _ => return None,
            };
            if padding > 0 && *byte != b'=' {
                return None;
            }
            bits = bits << 6 | u32::from(number);
        }
        if padding > 2 || (padding > 0 && (index + 1) * 4 != value.len()) {
            return None;
        }
        out.push((bits >> 16) as u8);
        if padding < 2 {
            out.push((bits >> 8) as u8);
        }
        if padding < 1 {
            out.push(bits as u8);
        }
    }
    if out.is_empty()
        || out.len() > 256
        || !out
            .iter()
            .all(|b| b.is_ascii() && (*b == 0 || !b.is_ascii_control() || *b == b'\n'))
    {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_base64_pressure_payload() {
        assert_eq!(
            decode_trigger("c29tZSAyMDAwMDAgMjAwMDAwMAA="),
            Some(b"some 200000 2000000\0".to_vec())
        );
        assert_eq!(decode_trigger("bad"), None);
        assert_eq!(decode_trigger("===="), None);
        assert_eq!(decode_trigger(&"A".repeat(516)), None);
    }
}
