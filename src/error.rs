use super::*;

#[cfg(feature = "time")]
#[cfg_attr(docsrs, doc(cfg(feature = "time")))]
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum SendTimeoutError<T> {
    Timeout(T),
    Disconnected(T),
}

#[cfg(feature = "time")]
impl<T> fmt::Debug for SendTimeoutError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendTimeoutError::Timeout(..) => "SendTimeoutError::Timeout(..)".fmt(f),
            SendTimeoutError::Disconnected(..) => "SendTimeoutError::Disconnected(..)".fmt(f),
        }
    }
}

#[cfg(feature = "time")]
impl<T> fmt::Display for SendTimeoutError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendTimeoutError::Timeout(..) => "timed out sending on a channel".fmt(f),
            SendTimeoutError::Disconnected(..) => "sending on a closed channel".fmt(f),
        }
    }
}

#[cfg(feature = "time")]
impl<T> std::error::Error for SendTimeoutError<T> {}

#[cfg(feature = "time")]
#[cfg_attr(docsrs, doc(cfg(feature = "time")))]
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum RecvTimeoutError {
    Timeout,
    Disconnected,
}

#[cfg(feature = "time")]
impl fmt::Debug for RecvTimeoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecvTimeoutError::Timeout => "RecvTimeoutError::Timeout".fmt(f),
            RecvTimeoutError::Disconnected => "RecvTimeoutError::Disconnected".fmt(f),
        }
    }
}

#[cfg(feature = "time")]
impl fmt::Display for RecvTimeoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecvTimeoutError::Timeout => "timed out receiving on an empty channel".fmt(f),
            RecvTimeoutError::Disconnected => "receiving on a closed channel".fmt(f),
        }
    }
}

#[cfg(feature = "time")]
impl std::error::Error for RecvTimeoutError {}

#[cfg(feature = "sync")]
#[cfg_attr(docsrs, doc(cfg(feature = "sync")))]
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum SendError<T> {
    Disconnected(T),
}

#[cfg(feature = "sync")]
impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "SendError::Disconnected(..)".fmt(f)
    }
}

#[cfg(feature = "sync")]
impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "sending on a closed channel".fmt(f)
    }
}

#[cfg(feature = "sync")]
impl<T> std::error::Error for SendError<T> {}

#[cfg(feature = "sync")]
#[cfg_attr(docsrs, doc(cfg(feature = "sync")))]
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum RecvError {
    Disconnected,
}

#[cfg(feature = "sync")]
impl fmt::Debug for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "RecvError::Disconnected".fmt(f)
    }
}

#[cfg(feature = "sync")]
impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "receiving on a closed channel".fmt(f)
    }
}

#[cfg(feature = "sync")]
impl std::error::Error for RecvError {}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum TrySendError<T> {
    Full(T),
    Disconnected(T),
}

impl<T> fmt::Debug for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrySendError::Full(..) => "SendTimeoutError::Full(..)".fmt(f),
            TrySendError::Disconnected(..) => "SendTimeoutError::Disconnected(..)".fmt(f),
        }
    }
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrySendError::Full(..) => "sending on a full channel".fmt(f),
            TrySendError::Disconnected(..) => "sending on a closed channel".fmt(f),
        }
    }
}

impl<T> std::error::Error for TrySendError<T> {}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum TryRecvError {
    Empty,
    Disconnected,
}

impl fmt::Debug for TryRecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TryRecvError::Empty => "TryRecvError::Empty".fmt(f),
            TryRecvError::Disconnected => "TryRecvError::Disconnected".fmt(f),
        }
    }
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TryRecvError::Empty => "receiving on an empty channel".fmt(f),
            TryRecvError::Disconnected => "receiving on a closed channel".fmt(f),
        }
    }
}

impl std::error::Error for TryRecvError {}
