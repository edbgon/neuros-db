//! Core library for building Neuros (WOID) audio databases.
//!
//! Format reference: positron's developer guide (doc/database.html,
//! mdb.html, sai.html, pai.html, quirks.html).

pub mod builder;
pub mod dedupe;
pub mod device;
pub mod overrides;
pub mod rawdisk;
pub mod reader;
pub mod scan;
pub mod sync;
pub mod templates;
pub mod textfix;
pub mod verify;
pub mod vfs;
pub mod words;

use std::path::PathBuf;

pub use builder::{BuildOptions, DbFile, build_audio};
pub use device::{
    AddLayout, DeviceInfo, Drive, PlannedAdd, Source, WriteOptions, WriteReport, add_files, inspect, plan_add,
    write_database,
};
pub use rawdisk::SwapMode;
pub use scan::{ScanItem, ScanOptions, scan};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt database: {0}")]
    Corrupt(String),
    #[error("missing file: {0}")]
    Missing(PathBuf),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// One track as it will be stored in the audio database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    /// "FM Radio" / "Microphone" for device recordings, else None.
    pub recording: Option<String>,
    /// Only used for ordering tracks within an album.
    pub disc: Option<u32>,
    pub track_no: Option<u32>,
    pub duration_secs: u32,
    pub size_kb: u32,
    /// Path as the device sees it, e.g. `C:/MUSIC/Artist/song.mp3`.
    pub device_path: String,
}
