include!("lib.rs");

mod file_store;
mod mmap_store;

pub use file_store::{FileCommitStore, FileCommitStoreError};
pub use mmap_store::{MmapCommitStore, MmapCommitStoreError};
