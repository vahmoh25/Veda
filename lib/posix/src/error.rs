//! System call results and `errno`.

use vipc::IpcError;
use vproto::fs::FsError;

use crate::linux::errno::*;

/// The result of a system call: its value, or an `errno` value (returned to
/// musl negated).
pub type SysResult = Result<usize, isize>;

/// The `errno` for a file system error.
pub fn fs(e: FsError) -> isize {
    match e {
        FsError::NotFound => ENOENT,
        FsError::NotDir => ENOTDIR,
        FsError::IsDir => EISDIR,
        FsError::Exists => EEXIST,
        FsError::ReadOnly => EROFS,
        FsError::Invalid => EINVAL,
        FsError::NoSpace => ENOSPC,
        FsError::Io => EIO,
        FsError::BadFd => EBADF,
        FsError::TooMany => ENFILE,
        FsError::NotEmpty => ENOTEMPTY,
        FsError::Denied => EACCES,
    }
}

/// The `errno` for a kernel error.
pub fn kernel(e: vabi::Error) -> isize {
    use vabi::Error as K;
    match e {
        K::InvalidArgs | K::OutOfRange => EINVAL,
        K::BadHandle | K::WrongType => EBADF,
        K::AccessDenied => EACCES,
        K::NoMemory | K::LimitReached => ENOMEM,
        K::NotFound => ENOENT,
        K::ShouldWait => EAGAIN,
        K::TimedOut => ETIMEDOUT,
        K::PeerClosed => EPIPE,
        K::AlreadyExists => EEXIST,
        K::NotSupported | K::UnknownSyscall => ENOSYS,
        K::Busy => EBUSY,
        K::Fault => EFAULT,
        K::BufferTooSmall | K::BadState | K::Canceled | K::Io | K::Internal => EIO,
    }
}

/// Flattens the two error layers of a call to a service.
pub fn ipc<T>(r: Result<Result<T, FsError>, IpcError>) -> Result<T, isize> {
    match r {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(fs(e)),
        Err(_) => Err(EIO),
    }
}

/// The value musl gets back for a result.
pub fn encode(r: SysResult) -> isize {
    match r {
        Ok(v) => v as isize,
        Err(e) => -e,
    }
}
