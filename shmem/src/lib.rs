#[cfg(unix)]
mod posix;
mod utils;
#[cfg(windows)]
mod windows;

pub mod proc_channel;

pub mod platform {
    #[cfg(unix)]
    pub use crate::posix::*;
    #[cfg(windows)]
    pub use crate::windows::*;
}
