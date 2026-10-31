mod posix;
mod utils;

pub mod allocation;
pub mod proc_channel;

pub mod platform {
    pub use crate::posix::*;
}
