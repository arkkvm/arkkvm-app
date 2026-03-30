//! Remote filesystem operations module
//!
//! This module provides functionality to mount image files using losetup and mount commands,
//! and perform filesystem operations on the mounted directory.
//!
//! All path parameters support both absolute paths (starting with "/") and relative paths.
//! Absolute paths are automatically converted to be relative to the mount point.
//!
//! Usage example:
//! ```rust
//! use arkkvm::hardware::fs_remote::RemoteFs;
//!
//! let mut fs_manager = RemoteFs::new();
//! fs_manager.mount().await?;
//!
//! // Perform file operations - all these paths are relative to mount point
//! fs_manager.write_file("/test.txt", b"Hello World").await?;  // -> mount_point/test.txt
//! fs_manager.write_file("test.txt", b"Hello World").await?;   // -> mount_point/test.txt
//! fs_manager.write_file("/dir/test.txt", b"Hello World").await?; // -> mount_point/dir/test.txt
//! let data = fs_manager.read_file("/test.txt").await?;
//!
//! // Cleanup
//! fs_manager.unmount().await?;
//! ```

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use rustix::path::Arg;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

pub const IMG_FILE_PATH: &str = "/userdata/arkkvm/imgs"; // file transfer
// const IMG_FILE_NAME: &str = "test_fat32.img"; // file transfer
pub const IMG_FILE_NAME: &str = "ft_disk.img"; // file transfer
const MOUNT_POINT: &str = "/mnt/ft_disk"; // file transfer

lazy_static::lazy_static! {
    pub static ref REMOTE_FS: RwLock<RemoteFs> = RwLock::const_new(RemoteFs::new());
}

/// Get the size of the IMG_FILE_NAME file
///
/// # Returns
/// * `Result<(u64, u64)>` - (data_size, storage_size) in bytes
///   - data_size: File data size (actual written data)
///   - storage_size: File storage size (allocated space on filesystem)
pub async fn get_img_file_size() -> Result<(u64, u64)> {
    let image_path = PathBuf::from(IMG_FILE_PATH).join(IMG_FILE_NAME);
    match fs::metadata(&image_path).await {
        Ok(metadata) => Ok((metadata.len(), metadata.blocks() * 512)),
        Err(_) => Ok((0, 0)), // Return 0 if file doesn't exist
    }
}

/// Remote filesystem manager that mounts image files using system commands
pub struct RemoteFs {
    /// Path to the image file
    image_path: PathBuf,
    /// Mount point directory
    mount_point: PathBuf,
    /// Loop device path (set after mounting)
    loop_device: Option<String>,
    /// Whether the filesystem is currently mounted
    is_mounted: bool,
}

impl Drop for RemoteFs {
    fn drop(&mut self) {
        if self.is_mounted {
            // Note: We can't use async in Drop, so we'll just log a warning
            warn!(
                "RemoteFs dropped while mounted. Call unmount() explicitly to cleanup resources."
            );
        }
    }
}

impl RemoteFs {
    /// Create a new RemoteFs instance with default mount point
    ///
    /// # Arguments
    /// * `image_path` - Path to the image file to mount
    pub fn new() -> Self {
        Self {
            image_path: PathBuf::from(IMG_FILE_PATH).join(IMG_FILE_NAME),
            mount_point: PathBuf::from(MOUNT_POINT),
            loop_device: None,
            is_mounted: false,
        }
    }

    /// Create a new RemoteFs instance with custom mount point
    ///
    /// # Arguments
    /// * `image_path` - Path to the image file to mount
    /// * `mount_point` - Custom mount point directory
    pub fn with_mount_point(
        image_path: impl Into<PathBuf>,
        mount_point: impl Into<PathBuf>,
    ) -> Self {
        Self {
            image_path: image_path.into(),
            mount_point: mount_point.into(),
            loop_device: None,
            is_mounted: false,
        }
    }

    /// Mount the image file to the mount point using losetup and mount commands
    ///
    /// This method performs the following steps:
    /// 1. Creates the mount point directory if it doesn't exist
    /// 2. Uses losetup to find an available loop device
    /// 3. Associates the image file with the loop device
    /// 4. Mounts the loop device to the mount point
    pub async fn mount(&mut self) -> Result<()> {
        if self.is_mounted {
            return Ok(());
        }

        // Ensure image file exists
        if !self.image_path.exists() {
            return Err(anyhow!("Image file does not exist: {:?}", self.image_path));
        }

        // Create mount point directory if it doesn't exist
        fs::create_dir_all(&self.mount_point)
            .await
            .with_context(|| format!("Failed to create mount point: {:?}", self.mount_point))?;

        // Check if mount point is already in use
        if self.is_mount_point_in_use().await? {
            return Err(anyhow!("Mount point {:?} is already in use", self.mount_point));
        }

        // Find available loop device and associate with image
        let loop_device = self.setup_loop_device().await?;
        self.loop_device = Some(loop_device.clone());

        // Mount the loop device
        if let Err(e) = self.mount_loop_device(&loop_device).await {
            error!("Failed to mount loop device {}: {:?}", &loop_device, e);
            return Err(e);
        }

        self.is_mounted = true;
        info!(
            "Successfully mounted {:?} to {:?} using loop device {}",
            self.image_path, self.mount_point, loop_device
        );

        Ok(())
    }

    /// Unmount the filesystem and cleanup loop device
    pub async fn unmount(&mut self) -> Result<()> {
        if !self.is_mounted {
            return Ok(());
        }

        // Unmount the filesystem
        if let Err(e) = self.unmount_filesystem().await {
            error!("Failed to unmount filesystem: {:?}", &e);
            // Continue with cleanup even if unmount fails
            return Err(anyhow!("Failed to unmount filesystem error: {:?}", e));
        }

        // Cleanup loop device
        if let Some(ref loop_device) = self.loop_device {
            if let Err(e) = self.cleanup_loop_device(loop_device).await {
                error!("Failed to cleanup loop device {}: {}", loop_device, e);
                return Err(anyhow!("Failed to cleanup loop device: {:?}", e));
            }
        }

        self.is_mounted = false;
        self.loop_device = None;

        info!("Successfully unmounted and cleaned up filesystem");
        Ok(())
    }

    /// Check if the filesystem is currently mounted
    pub fn is_mounted(&self) -> bool {
        self.is_mounted
    }

    /// Get the mount point path
    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    /// Get the image file path
    pub fn image_path(&self) -> &Path {
        &self.image_path
    }

    /// Get the loop device path (if mounted)
    pub fn loop_device(&self) -> Option<&str> {
        self.loop_device.as_deref()
    }

    /// Read a file from the mounted filesystem
    ///
    /// # Arguments
    /// * `file_path` - Path to the file relative to the mount point (e.g., "/", "/aaa/bbb", "aaa/bbb")
    pub async fn read_file(&self, file_path: &str) -> Result<Vec<u8>> {
        self.ensure_mounted()?;

        let full_path = self.resolve_path(file_path)?;
        debug!("Reading file: {:?}", full_path);

        let mut file = fs::File::open(&full_path)
            .await
            .with_context(|| format!("Failed to open file for reading: {:?}", full_path))?;

        let mut contents = Vec::new();
        file.read_to_end(&mut contents)
            .await
            .with_context(|| format!("Failed to read file contents: {:?}", full_path))?;

        debug!("Successfully read {} bytes from {:?}", contents.len(), full_path);
        Ok(contents)
    }

    /// Read a file as a string from the mounted filesystem
    ///
    /// # Arguments
    /// * `file_path` - Path to the file relative to the mount point (e.g., "/", "/aaa/bbb", "aaa/bbb")
    pub async fn read_file_as_string(&self, file_path: &str) -> Result<String> {
        let contents = self.read_file(file_path).await?;
        String::from_utf8(contents)
            .with_context(|| format!("File {:?} contains invalid UTF-8", file_path))
    }

    /// Write data to a file in the mounted filesystem
    ///
    /// # Arguments
    /// * `file_path` - Path to the file relative to the mount point (e.g., "/", "/aaa/bbb", "aaa/bbb")
    /// * `data` - Data to write to the file
    pub async fn write_file(&self, file_path: &str, data: &[u8]) -> Result<()> {
        self.ensure_mounted()?;

        let full_path = self.resolve_path(file_path)?;
        debug!("Writing {} bytes to file: {:?}", data.len(), full_path);

        // Ensure parent directory exists
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create parent directory: {:?}", parent))?;
        }

        let mut file = fs::File::create(&full_path)
            .await
            .with_context(|| format!("Failed to create file for writing: {:?}", full_path))?;

        file.write_all(data)
            .await
            .with_context(|| format!("Failed to write data to file: {:?}", full_path))?;

        file.sync_all().await.with_context(|| format!("Failed to sync file: {:?}", full_path))?;

        debug!("Successfully wrote {} bytes to {:?}", data.len(), full_path);
        Ok(())
    }

    /// Write a string to a file in the mounted filesystem
    ///
    /// # Arguments
    /// * `file_path` - Path to the file relative to the mount point (e.g., "/", "/aaa/bbb", "aaa/bbb")
    /// * `content` - String content to write to the file
    pub async fn write_file_as_string(&self, file_path: &str, content: &str) -> Result<()> {
        self.write_file(file_path, content.as_bytes()).await
    }

    /// Append data to a file in the mounted filesystem
    ///
    /// # Arguments
    /// * `file_path` - Path to the file relative to the mount point (e.g., "/", "/aaa/bbb", "aaa/bbb")
    /// * `data` - Data to append to the file
    pub async fn append_file(&self, file_path: &str, data: &[u8]) -> Result<()> {
        self.ensure_mounted()?;

        let full_path = self.resolve_path(file_path)?;
        debug!("Appending {} bytes to file: {:?}", data.len(), full_path);

        // Ensure parent directory exists
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create parent directory: {:?}", parent))?;
        }

        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&full_path)
            .await
            .with_context(|| format!("Failed to open file for appending: {:?}", full_path))?;

        file.write_all(data)
            .await
            .with_context(|| format!("Failed to append data to file: {:?}", full_path))?;

        file.sync_all().await.with_context(|| format!("Failed to sync file: {:?}", full_path))?;

        debug!("Successfully appended {} bytes to {:?}", data.len(), full_path);
        Ok(())
    }

    /// Append a string to a file in the mounted filesystem
    ///
    /// # Arguments
    /// * `file_path` - Path to the file relative to the mount point (e.g., "/", "/aaa/bbb", "aaa/bbb")
    /// * `content` - String content to append to the file
    pub async fn append_file_as_string(&self, file_path: &str, content: &str) -> Result<()> {
        self.append_file(file_path, content.as_bytes()).await
    }

    /// Read a file in chunks from the mounted filesystem
    ///
    /// # Arguments
    /// * `file_path` - Path to the file relative to the mount point
    /// * `offset` - Starting offset in bytes
    /// * `size` - Number of bytes to read
    pub async fn read_file_range(
        &self,
        file_path: &str,
        offset: u64,
        size: usize,
    ) -> Result<Vec<u8>> {
        self.ensure_mounted()?;

        let full_path = self.resolve_path(file_path)?;
        debug!("Reading file range: {:?} (offset: {}, size: {})", full_path, offset, size);

        let mut file = fs::File::open(&full_path)
            .await
            .with_context(|| format!("Failed to open file for reading: {:?}", full_path))?;

        // Seek to the specified offset
        use tokio::io::AsyncSeekExt;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .with_context(|| format!("Failed to seek to offset {}: {:?}", offset, full_path))?;

        let mut buffer = vec![0u8; size];
        let mut total_read = 0;

        // Loop reading until buffer is full or end of file is reached
        while total_read < size {
            let bytes_read = file
                .read(&mut buffer[total_read..])
                .await
                .with_context(|| format!("Failed to read file range: {:?}", full_path))?;

            if bytes_read == 0 {
                // Reached end of file
                break;
            }

            total_read += bytes_read;
        }

        buffer.truncate(total_read);
        debug!("Successfully read {} bytes from {:?} at offset {}", total_read, full_path, offset);
        Ok(buffer)
    }

    /// Write data to a file at a specific offset in the mounted filesystem
    ///
    /// # Arguments
    /// * `file_path` - Path to the file relative to the mount point
    /// * `offset` - Starting offset in bytes
    /// * `data` - Data to write
    pub async fn write_file_range(
        &self,
        file_path: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<usize> {
        self.ensure_mounted()?;

        let full_path = self.resolve_path(file_path)?;
        debug!("Writing file range: {:?} (offset: {}, size: {})", full_path, offset, data.len());

        // Ensure parent directory exists
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create parent directory: {:?}", parent))?;
        }

        use tokio::io::AsyncSeekExt;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .open(&full_path)
            .await
            .with_context(|| format!("Failed to open file for writing: {:?}", full_path))?;

        // Get current file size
        let file_size = file
            .seek(std::io::SeekFrom::End(0))
            .await
            .with_context(|| format!("Failed to get file size: {:?}", full_path))?;

        // If we need to extend the file, write zeros to fill the gap
        if offset > file_size {
            file.seek(std::io::SeekFrom::Start(file_size))
                .await
                .with_context(|| format!("Failed to seek to end: {:?}", full_path))?;

            let gap_size = offset - file_size;
            let zeros = vec![0u8; gap_size as usize];
            file.write_all(&zeros)
                .await
                .with_context(|| format!("Failed to fill gap: {:?}", full_path))?;
        }

        // Seek to the write position
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .with_context(|| format!("Failed to seek to offset {}: {:?}", offset, full_path))?;

        // Loop writing data until all data is written
        let mut remaining = data;
        while !remaining.is_empty() {
            let bytes_written = file
                .write(remaining)
                .await
                .with_context(|| format!("Failed to write data: {:?}", full_path))?;

            if bytes_written == 0 {
                return Err(anyhow!("Failed to write any data to file: {:?}", full_path));
            }

            remaining = &remaining[bytes_written..];
        }

        file.sync_all().await.with_context(|| format!("Failed to sync file: {:?}", full_path))?;

        debug!("Successfully wrote {} bytes to {:?} at offset {}", data.len(), full_path, offset);
        Ok(data.len())
    }

    /// Create an async file reader for streaming file operations
    ///
    /// # Arguments
    /// * `file_path` - Path to the file relative to the mount point
    /// * `chunk_size` - Size of each chunk to read
    pub async fn create_async_reader(
        &self,
        file_path: &str,
        chunk_size: usize,
    ) -> Result<AsyncFileReader> {
        self.ensure_mounted()?;

        let full_path = self.resolve_path(file_path)?;
        debug!("Creating async reader: {:?} (chunk size: {})", full_path, chunk_size);

        let mut file = fs::File::open(&full_path)
            .await
            .with_context(|| format!("Failed to open file for reading: {:?}", full_path))?;

        // Get file size
        use tokio::io::AsyncSeekExt;
        let file_size = file
            .seek(std::io::SeekFrom::End(0))
            .await
            .with_context(|| format!("Failed to get file size: {:?}", full_path))?;

        // Reset to beginning
        file.seek(std::io::SeekFrom::Start(0))
            .await
            .with_context(|| format!("Failed to seek to beginning: {:?}", full_path))?;

        Ok(AsyncFileReader { file, chunk_size, current_position: 0, file_size })
    }

    /// Create an async file writer for streaming file operations
    ///
    /// # Arguments
    /// * `file_path` - Path to the file relative to the mount point
    /// * `chunk_size` - Size of each chunk to write
    pub async fn create_async_writer(
        &self,
        file_path: &str,
        chunk_size: usize,
        bytes_written: u64,
    ) -> Result<AsyncFileWriter> {
        self.ensure_mounted()?;

        let full_path = self.resolve_path(file_path)?;
        debug!("Creating async writer: {:?} (chunk size: {})", full_path, chunk_size);

        // Ensure parent directory exists
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create parent directory: {:?}", parent))?;
        }

        let file = if bytes_written > 0 {
            fs::OpenOptions::new().create(true).append(true).write(true).open(&full_path).await?
        } else {
            fs::File::create(&full_path).await?
        };

        Ok(AsyncFileWriter { file, chunk_size, current_position: bytes_written, bytes_written })
    }

    /// Check if a file or directory exists in the mounted filesystem
    ///
    /// # Arguments
    /// * `path` - Path relative to the mount point
    pub async fn exists(&self, path: &str) -> Result<bool> {
        self.ensure_mounted()?;

        let full_path = self.resolve_path(path)?;
        Ok(fs::metadata(&full_path).await.is_ok())
    }

    /// Check if a path is a file
    ///
    /// # Arguments
    /// * `path` - Path relative to the mount point
    pub async fn is_file(&self, path: &str) -> Result<bool> {
        self.ensure_mounted()?;

        let full_path = self.resolve_path(path)?;
        match fs::metadata(&full_path).await {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(_) => Ok(false),
        }
    }

    /// Check if a path is a directory
    ///
    /// # Arguments
    /// * `path` - Path relative to the mount point
    pub async fn is_directory(&self, path: &str) -> Result<bool> {
        self.ensure_mounted()?;

        let full_path = self.resolve_path(path)?;
        match fs::metadata(&full_path).await {
            Ok(metadata) => Ok(metadata.is_dir()),
            Err(_) => Ok(false),
        }
    }

    /// Get file size in bytes
    ///
    /// # Arguments
    /// * `file_path` - Path to the file relative to the mount point
    pub async fn file_size(&self, file_path: &str) -> Result<u64> {
        self.ensure_mounted()?;

        let full_path = self.resolve_path(file_path)?;
        let metadata = fs::metadata(&full_path)
            .await
            .with_context(|| format!("Failed to get file metadata: {:?}", full_path))?;

        Ok(metadata.len())
    }

    /// Create a directory in the mounted filesystem
    ///
    /// # Arguments
    /// * `dir_path` - Path to the directory relative to the mount point
    pub async fn create_directory(&self, dir_path: &str) -> Result<()> {
        self.ensure_mounted()?;

        if dir_path.is_empty() {
            return Err(anyhow!("dir_path is empty"));
        }
        let full_path = self.resolve_path(dir_path)?;
        debug!("Creating directory: {:?}", full_path);

        fs::create_dir_all(&full_path)
            .await
            .with_context(|| format!("Failed to create directory: {:?}", full_path))?;

        debug!("Successfully created directory: {:?}", full_path);
        Ok(())
    }

    /// Delete a file from the mounted filesystem
    ///
    /// # Arguments
    /// * `file_path` - Path to the file relative to the mount point
    pub async fn delete_file(&self, file_path: &str) -> Result<()> {
        self.ensure_mounted()?;

        let full_path = self.resolve_path(file_path)?;
        debug!("Deleting file: {:?}", full_path);

        fs::remove_file(&full_path)
            .await
            .with_context(|| format!("Failed to delete file: {:?}", full_path))?;

        debug!("Successfully deleted file: {:?}", full_path);
        Ok(())
    }

    /// Delete a directory and all its contents from the mounted filesystem
    ///
    /// # Arguments
    /// * `dir_path` - Path to the directory relative to the mount point
    pub async fn delete_directory(&self, dir_path: &str) -> Result<()> {
        self.ensure_mounted()?;

        if dir_path.is_empty() {
            return Err(anyhow!("dir_path is empty"));
        }
        let full_path = self.resolve_path(dir_path)?;
        debug!("Deleting directory: {:?}", full_path);

        fs::remove_dir_all(&full_path)
            .await
            .with_context(|| format!("Failed to delete directory: {:?}", full_path))?;

        debug!("Successfully deleted directory: {:?}", full_path);
        Ok(())
    }

    /// Rename a file or directory in the mounted filesystem
    ///
    /// # Arguments
    /// * `old_path` - Current path relative to the mount point
    /// * `new_path` - New path relative to the mount point
    pub async fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
        self.ensure_mounted()?;

        let old_full_path = self.resolve_path(old_path)?;
        let new_full_path = self.resolve_path(new_path)?;
        debug!("Renaming {:?} to {:?}", old_full_path, new_full_path);

        // Ensure parent directory of new path exists
        if let Some(parent) = new_full_path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create parent directory: {:?}", parent))?;
        }

        fs::rename(&old_full_path, &new_full_path).await.with_context(|| {
            format!("Failed to rename {:?} to {:?}", old_full_path, new_full_path)
        })?;

        debug!("Successfully renamed {:?} to {:?}", old_full_path, new_full_path);
        Ok(())
    }

    /// List files and directories in a directory
    ///
    /// # Arguments
    /// * `dir_path` - Path to the directory relative to the mount point (default: "/")
    pub async fn list_directory(&self, dir_path: &str) -> Result<Vec<FileInfo>> {
        self.ensure_mounted()?;

        if dir_path.is_empty() {
            return Err(anyhow!("dir_path is empty"));
        }
        let full_path = self.resolve_path(dir_path)?;
        info!("Listing directory: {:?}", full_path);

        let mut entries = fs::read_dir(&full_path)
            .await
            .with_context(|| format!("Failed to read directory: {:?}", full_path))?;

        let mut file_info_list = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let metadata = entry.metadata().await?;
            let modified_timestamp = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs());

            let name = entry.file_name().to_string_lossy().to_string();
            let is_directory = metadata.is_dir();

            if dir_path == "/" {
                if name == "System Volume Information" && is_directory {
                    continue;
                }
            } else {
                if (name == "." || name == "..") && is_directory {
                    continue;
                }
            }

            let file_info = FileInfo {
                name: entry.file_name().to_string_lossy().to_string(),
                size: metadata.len(),
                is_file: metadata.is_file(),
                is_dir: metadata.is_dir(),
                modified: modified_timestamp,
            };
            file_info_list.push(file_info);
        }

        info!(
            "Found {} entries in {:?}: data: {:?}",
            file_info_list.len(),
            file_info_list,
            full_path
        );
        Ok(file_info_list)
    }

    /// Get detailed information about files and directories in a directory
    ///
    /// # Arguments
    /// * `dir_path` - Path to the directory relative to the mount point (default: "/")
    // pub async fn list_directory_detailed(&self, dir_path: &str) -> Result<Vec<FileInfo>> {
    //     self.ensure_mounted()?;

    //     if dir_path.is_empty() {
    //         return Err(anyhow!("dir_path is empty"));
    //     }
    //     let full_path = if dir_path == "/" { &self.mount_point } else { &self.mount_point.join(&dir_path[1..]) };
    //     debug!("Listing directory with details: {:?}", full_path);

    //     let mut entries = fs::read_dir(&full_path)
    //         .await
    //         .with_context(|| format!("Failed to read directory: {:?}", full_path))?;

    //     let mut file_info_list = Vec::new();
    //     while let Some(entry) = entries.next_entry().await? {
    //         let metadata = entry.metadata().await?;
    //         let modified_timestamp = metadata
    //             .modified()
    //             .ok()
    //             .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
    //             .map(|duration| duration.as_secs());

    //         let file_info = FileInfo {
    //             name: entry.file_name().to_string_lossy().to_string(),
    //             size: metadata.len(),
    //             is_file: metadata.is_file(),
    //             is_dir: metadata.is_dir(),
    //             modified: modified_timestamp,
    //         };
    //         file_info_list.push(file_info);
    //     }

    //     debug!("Found {} entries in {:?}", file_info_list.len(), full_path);
    //     Ok(file_info_list)
    // }

    /// Copy a file within the mounted filesystem
    ///
    /// # Arguments
    /// * `source_path` - Source file path relative to the mount point
    /// * `dest_path` - Destination file path relative to the mount point
    pub async fn copy_file(&self, source_path: &str, dest_path: &str) -> Result<()> {
        self.ensure_mounted()?;

        let source_full_path = self.resolve_path(source_path)?;
        let dest_full_path = self.resolve_path(dest_path)?;
        debug!("Copying file from {:?} to {:?}", source_full_path, dest_full_path);

        // Ensure parent directory of destination exists
        if let Some(parent) = dest_full_path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create parent directory: {:?}", parent))?;
        }

        fs::copy(&source_full_path, &dest_full_path).await.with_context(|| {
            format!("Failed to copy file from {:?} to {:?}", source_full_path, dest_full_path)
        })?;

        debug!("Successfully copied file from {:?} to {:?}", source_full_path, dest_full_path);
        Ok(())
    }

    pub async fn create_empty_file(&self, file_path: &str) -> Result<()> {
        self.ensure_mounted()?;

        let full_path = self.resolve_path(file_path)?;

        // Ensure parent directory exists
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create parent directory: {:?}", parent))?;
        }

        // Create the file and set its size
        let _file = fs::File::create(&full_path)
            .await
            .with_context(|| format!("Failed to create file: {:?}", full_path))?;

        Ok(())
    }

    /// Get file size in bytes
    ///
    /// # Arguments
    /// * `file_path` - Path to the file relative to the mount point
    ///
    /// # Returns
    /// * `Result<u64>` - File size in bytes
    pub async fn get_file_size(&self, file_path: &str) -> Result<u64> {
        self.ensure_mounted()?;

        let full_path = self.resolve_path(file_path)?;
        debug!("Getting file size: {:?}", full_path);

        let metadata = fs::metadata(&full_path)
            .await
            .with_context(|| format!("Failed to get file metadata: {:?}", full_path))?;

        let size = metadata.len();
        debug!("File size: {} bytes for {:?}", size, full_path);
        Ok(size)
    }

    pub async fn get_file_info(&self, file_path: &str) -> Result<FileInfo> {
        self.ensure_mounted()?;

        let full_path = self.resolve_path(file_path)?;
        debug!("Getting file size: {:?}", full_path);

        let metadata = fs::metadata(&full_path)
            .await
            .with_context(|| format!("Failed to get file metadata: {:?}", full_path))?;

        let modified_timestamp = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs());

        Ok(FileInfo {
            name: full_path.file_name().unwrap_or_default().to_string_lossy().to_string(),
            size: metadata.len(),
            is_file: metadata.is_file(),
            is_dir: metadata.is_dir(),
            modified: modified_timestamp,
        })
    }

    /// Get file storage and data sizes
    ///
    /// # Arguments
    /// * `file_path` - Path to the file relative to the mount point
    ///
    /// # Returns
    /// * `Result<(u64, u64)>` - (storage_size, data_size) in bytes
    ///   - storage_size: File storage size (allocated space on filesystem)
    ///   - data_size: File data size (actual written data)
    pub async fn get_file_sizes(&self, file_path: &str) -> Result<(u64, u64)> {
        self.ensure_mounted()?;

        let full_path = self.resolve_path(file_path)?;
        debug!("Getting file sizes: {:?}", full_path);

        // Get file metadata for data size
        let metadata = fs::metadata(&full_path)
            .await
            .with_context(|| format!("Failed to get file metadata: {:?}", full_path))?;

        let data_size = metadata.len();
        // Use stat command to get file blocks and block size for storage size
        let output = Command::new("stat")
            .args(["-c", "%b", &full_path.to_string_lossy()])
            .output()
            .await
            .context("Failed to execute stat command")?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to get file storage size: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let blocks_str =
            String::from_utf8(output.stdout).context("Invalid UTF-8 in stat output")?;
        let blocks_str = blocks_str.trim();
        let blocks = blocks_str
            .parse::<u64>()
            .with_context(|| format!("Failed to parse blocks: {}", blocks_str))?;

        // Get block size
        let block_size_output = Command::new("stat")
            .args(["-c", "%B", &full_path.to_string_lossy()])
            .output()
            .await
            .context("Failed to execute stat command for block size")?;

        if !block_size_output.status.success() {
            return Err(anyhow!(
                "Failed to get block size: {}",
                String::from_utf8_lossy(&block_size_output.stderr)
            ));
        }

        let block_size_str = String::from_utf8(block_size_output.stdout)?;
        let block_size_str = block_size_str.trim();
        let block_size = block_size_str
            .parse::<u64>()
            .with_context(|| format!("Failed to parse block size: {}", block_size_str))?;

        let storage_size = blocks * block_size;

        debug!(
            "File sizes for {:?}: storage={} bytes ({} blocks * {} bytes), data={} bytes",
            full_path, storage_size, blocks, block_size, data_size
        );

        Ok((storage_size, data_size))
    }

    /// Get total size of the mounted filesystem
    ///
    /// # Returns
    /// * `Result<u64>` - Total size in bytes
    pub async fn get_total_size(&self) -> Result<u64> {
        self.ensure_mounted()?;

        debug!("Getting total size for mount point: {:?}", self.mount_point);

        // Use df command to get filesystem total size
        let output = Command::new("df")
            .args(["-B1", &self.mount_point.to_string_lossy()])
            .output()
            .await
            .context("Failed to execute df command")?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to get filesystem total size: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let output_str = String::from_utf8(output.stdout).context("Invalid UTF-8 in df output")?;

        // Parse df output to get total size
        // df output format: Filesystem 1K-blocks Used Available Use% Mounted-on
        let lines: Vec<&str> = output_str.trim().split('\n').collect();
        if lines.len() < 2 {
            return Err(anyhow!("Invalid df output format"));
        }

        let data_line = lines[1];
        let fields: Vec<&str> = data_line.split_whitespace().collect();
        if fields.len() < 4 {
            return Err(anyhow!("Invalid df output format: insufficient fields"));
        }

        // Parse total size (2nd field, 1-indexed)
        let total_size = fields[1]
            .parse::<u64>()
            .with_context(|| format!("Failed to parse total size: {}", fields[1]))?;

        debug!("Total size: {} bytes for {:?}", total_size, self.mount_point);
        Ok(total_size)
    }

    /// Get used size of the mounted filesystem
    ///
    /// # Returns
    /// * `Result<u64>` - Used size in bytes
    pub async fn get_used_size(&self) -> Result<u64> {
        self.ensure_mounted()?;

        debug!("Getting used size for mount point: {:?}", self.mount_point);

        // Use df command to get filesystem usage
        let output = Command::new("df")
            .args(["-B1", &self.mount_point.to_string_lossy()])
            .output()
            .await
            .context("Failed to execute df command")?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to get filesystem usage: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let output_str = String::from_utf8(output.stdout).context("Invalid UTF-8 in df output")?;

        // Parse df output to get used size
        // df output format: Filesystem 1K-blocks Used Available Use% Mounted-on
        let lines: Vec<&str> = output_str.trim().split('\n').collect();
        if lines.len() < 2 {
            return Err(anyhow!("Invalid df output format"));
        }

        let data_line = lines[1];
        let fields: Vec<&str> = data_line.split_whitespace().collect();
        if fields.len() < 4 {
            return Err(anyhow!("Invalid df output format: insufficient fields"));
        }

        // Parse used size (3rd field, 1-indexed)
        let used_size = fields[2]
            .parse::<u64>()
            .with_context(|| format!("Failed to parse used size: {}", fields[2]))?;

        debug!("Used size: {} bytes for {:?}", used_size, self.mount_point);
        Ok(used_size)
    }

    /// Get available size of the mounted filesystem
    ///
    /// # Returns
    /// * `Result<u64>` - Available size in bytes
    pub async fn get_available_size(&self) -> Result<u64> {
        self.ensure_mounted()?;

        debug!("Getting available size for mount point: {:?}", self.mount_point);

        // Use df command to get filesystem available size
        let output = Command::new("df")
            .args(["-B1", &self.mount_point.to_string_lossy()])
            .output()
            .await
            .context("Failed to execute df command")?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to get filesystem available size: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let output_str = String::from_utf8(output.stdout).context("Invalid UTF-8 in df output")?;

        // Parse df output to get available size
        // df output format: Filesystem 1K-blocks Used Available Use% Mounted-on
        let lines: Vec<&str> = output_str.trim().split('\n').collect();
        if lines.len() < 2 {
            return Err(anyhow!("Invalid df output format"));
        }

        let data_line = lines[1];
        let fields: Vec<&str> = data_line.split_whitespace().collect();
        if fields.len() < 4 {
            return Err(anyhow!("Invalid df output format: insufficient fields"));
        }

        // Parse available size (4th field, 1-indexed)
        let available_size = fields[3]
            .parse::<u64>()
            .with_context(|| format!("Failed to parse available size: {}", fields[3]))?;

        debug!("Available size: {} bytes for {:?}", available_size, self.mount_point);
        Ok(available_size)
    }

    /// Get filesystem usage information
    ///
    /// # Returns
    /// * `Result<(u64, u64, u64)>` - (total_size, used_size, available_size) in bytes
    pub async fn get_filesystem_usage(&self) -> Result<FileSystemInfo> {
        self.ensure_mounted()?;

        debug!("Getting filesystem usage for mount point: {:?}", self.mount_point);

        // Use df command to get filesystem usage
        let output = Command::new("df")
            .args(&[self.mount_point.to_string_lossy().to_string().as_str()])
            .output()
            .await
            .context("Failed to execute df command")?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to get filesystem usage: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let output_str = String::from_utf8(output.stdout).context("Invalid UTF-8 in df output")?;

        // Parse df output to get usage information
        // df output format: Filesystem 1K-blocks Used Available Use% Mounted-on
        let lines: Vec<&str> = output_str.trim().split('\n').collect();
        if lines.len() < 2 {
            return Err(anyhow!("Invalid df output format"));
        }

        let data_line = lines[1];
        let fields: Vec<&str> = data_line.split_whitespace().collect();
        if fields.len() < 4 {
            return Err(anyhow!("Invalid df output format: insufficient fields"));
        }

        // Parse sizes (2nd, 3rd, 4th fields, 1-indexed)
        let total_size = fields[1]
            .parse::<u64>()
            .with_context(|| format!("Failed to parse total size: {}", fields[1]))?;
        let used_size = fields[2]
            .parse::<u64>()
            .with_context(|| format!("Failed to parse used size: {}", fields[2]))?;
        let available_size = fields[3]
            .parse::<u64>()
            .with_context(|| format!("Failed to parse available size: {}", fields[3]))?;

        debug!(
            "Filesystem usage for {:?}: total={} bytes, used={} bytes, available={} bytes",
            self.mount_point, total_size, used_size, available_size
        );

        Ok(FileSystemInfo {
            total_space: total_size,
            free_space: available_size,
            used_space: used_size,
        })
    }

    /// Repair FAT32 filesystem to fix disk usage issues
    ///
    /// This method unmounts the filesystem, runs fsck.vfat to repair it,
    /// and then remounts it. This is useful when df shows incorrect disk usage
    /// (e.g., showing several GB used when there are no files).
    ///
    /// # Returns
    /// * `Result<()>` - Returns empty on success
    ///
    /// # How it works
    /// 1. Unmounts the filesystem if mounted
    /// 2. Detaches the loop device
    /// 3. Runs fsck.vfat (or dosfsck) with -a (auto-repair) flag on the image file
    /// 4. Reattaches the loop device and remounts the filesystem
    ///
    /// # Note
    /// This operation requires root privileges or appropriate permissions
    /// to run fsck.vfat on the image file.
    pub async fn repair_filesystem(&mut self) -> Result<()> {
        info!("Starting FAT32 filesystem repair for: {:?}", self.image_path);
        
        // Step 1: Unmount if mounted
        let was_mounted = self.is_mounted;
        // let loop_device_backup = self.loop_device.clone();
        
        if was_mounted {
            info!("Unmounting filesystem before repair");
            self.unmount().await?;
        }

        // Step 2: Run fsck.vfat to repair the filesystem
        // Try fsck.vfat first (common on Linux), fallback to dosfsck
        let fsck_commands = ["fsck.vfat", "dosfsck"];
        let mut repair_success = false;
        let mut last_error = None;

        for cmd_name in &fsck_commands {
            let output = Command::new(cmd_name)
                .args(&["-a", "-v"]) // -a: auto-repair, -v: verbose
                .arg(&self.image_path)
                .output()
                .await;

            match output {
                Ok(output) => {
                    if output.status.success() {
                        info!("Successfully repaired filesystem using {}", cmd_name);
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        if !stdout.is_empty() {
                            info!("fsck output: {}", stdout);
                        }
                        if !stderr.is_empty() {
                            debug!("fsck stderr: {}", stderr);
                        }
                        repair_success = true;
                        break;
                    } else {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        warn!("{} failed: {}", cmd_name, stderr);
                        last_error = Some(format!("{} failed: {}", cmd_name, stderr));
                    }
                }
                Err(e) => {
                    debug!("Command {} not found or failed: {}", cmd_name, e);
                    last_error = Some(format!("Command {} not found: {}", cmd_name, e));
                }
            }
        }

        if !repair_success {
            return Err(anyhow!(
                "Failed to repair filesystem. Tried: {}. Last error: {}",
                fsck_commands.join(", "),
                last_error.unwrap_or_else(|| "Unknown error".to_string())
            ));
        }

        // Step 3: Remount if it was mounted before
        if was_mounted {
            info!("Remounting filesystem after repair");
            self.mount().await?;
        }

        info!("FAT32 filesystem repair completed successfully");
        Ok(())
    }
    /// Reformat the filesystem to FAT32
    ///
    /// This method reformats the image file to FAT32 filesystem, which is useful
    /// when the filesystem has been formatted to a different filesystem type by
    /// other platforms (e.g., Windows, macOS) and cannot be used anymore.
    ///
    /// **WARNING**: This operation will **DELETE ALL DATA** on the filesystem!
    ///
    /// # Returns
    /// * `Result<()>` - Returns empty on success
    ///
    /// # How it works
    /// 1. Unmounts the filesystem if mounted
    /// 2. Detaches the loop device
    /// 3. Formats the image file to FAT32 using mkfs.vfat (or mkdosfs)
    /// 4. Optionally remounts the filesystem
    ///
    /// # Parameters
    /// * `remount` - Whether to remount the filesystem after formatting (default: true)
    ///
    /// # Note
    /// - This operation requires root privileges or appropriate permissions
    /// - All existing data will be permanently lost
    /// - The image file size remains unchanged, only the filesystem structure is recreated
    pub async fn format_filesystem(&mut self) -> Result<()> {
        info!("Starting FAT32 filesystem format for: {:?}", self.image_path);
        warn!("WARNING: This will delete all data on the filesystem!");
        
        // Step 1: Unmount if mounted
        let was_mounted = self.is_mounted;
        
        if was_mounted {
            info!("Unmounting filesystem before format");
            self.unmount().await?;
        }

        // Step 2: Check if image file exists
        if !self.image_path.exists() {
            return Err(anyhow!("Image file does not exist: {:?}", self.image_path));
        }

        // Step 3: Format the filesystem to FAT32
        // Try mkfs.vfat first (common on Linux), fallback to mkdosfs
        let mkfs_commands = ["mkfs.vfat", "mkdosfs"];
        let mut format_success = false;
        let mut last_error = None;

        for cmd_name in &mkfs_commands {
            let output = Command::new(cmd_name)
                .args(&["-F", "32"]) // -F 32: Force FAT32 format
                .arg(&self.image_path)
                .output()
                .await;

            match output {
                Ok(output) => {
                    if output.status.success() {
                        info!("Successfully formatted filesystem using {}", cmd_name);
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        if !stdout.is_empty() {
                            info!("mkfs output: {}", stdout);
                        }
                        if !stderr.is_empty() {
                            debug!("mkfs stderr: {}", stderr);
                        }
                        format_success = true;
                        break;
                    } else {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        warn!("{} failed: {}", cmd_name, stderr);
                        last_error = Some(format!("{} failed: {}", cmd_name, stderr));
                    }
                }
                Err(e) => {
                    debug!("Command {} not found or failed: {}", cmd_name, e);
                    last_error = Some(format!("Command {} not found: {}", cmd_name, e));
                }
            }
        }

        if !format_success {
            return Err(anyhow!(
                "Failed to format filesystem. Tried: {}. Last error: {}",
                mkfs_commands.join(", "),
                last_error.unwrap_or_else(|| "Unknown error".to_string())
            ));
        }

        // Step 4: Remount if requested and was mounted before
        if was_mounted {
            info!("Remounting filesystem after format");
            self.mount().await?;
        }

        info!("FAT32 filesystem format completed successfully");
        Ok(())
    }
}

impl RemoteFs {
    /// Ensure the filesystem is mounted before performing operations
    fn ensure_mounted(&self) -> Result<()> {
        if !self.is_mounted {
            Err(anyhow!("Filesystem is not mounted. Call mount() first."))
        } else {
            Ok(())
        }
    }

    /// Convert a user-specified path to a path relative to the mount point
    ///
    /// This method ensures that all paths are treated as relative to the mount point,
    /// regardless of whether the user specifies absolute paths (starting with "/") or relative paths.
    /// It also validates the path to prevent directory traversal attacks and rejects unsafe paths.
    ///
    /// # Arguments
    /// * `user_path` - Path specified by the user (e.g., "/", "/aaa/bbb", "aaa/bbb")
    ///
    /// # Returns
    /// * `PathBuf` - Path relative to the mount point
    ///
    /// # Errors
    /// * Returns an error if the path contains unsafe components like ".." or "."
    ///
    /// # Examples
    /// - "/" -> mount_point
    /// - "/aaa/bbb" -> mount_point/aaa/bbb
    /// - "aaa/bbb" -> mount_point/aaa/bbb
    /// - "" -> mount_point (empty string treated as root)
    fn resolve_path(&self, user_path: &str) -> Result<PathBuf> {
        if user_path.is_empty() || user_path == "/" {
            // Empty string or root directory maps to mount point
            Ok(self.mount_point.clone())
        } else if user_path.starts_with('/') {
            // Absolute path starting with "/" - remove the leading "/" and join to mount point
            let relative_path = &user_path[1..];
            if relative_path.is_empty() {
                Ok(self.mount_point.clone())
            } else {
                // Validate and normalize path by removing duplicate slashes and checking for unsafe components
                let path_components: Vec<&str> =
                    relative_path.split('/').filter(|s| !s.is_empty()).collect();

                // Check for unsafe path components
                for component in &path_components {
                    if component == &".." || component == &"." {
                        return Err(anyhow!(
                            "Unsafe path component '{}' is not allowed in path '{}'",
                            component,
                            user_path
                        ));
                    }
                }

                let normalized_path = path_components.join("/");

                if normalized_path.is_empty() {
                    Ok(self.mount_point.clone())
                } else {
                    Ok(self.mount_point.join(normalized_path))
                }
            }
        } else {
            // Relative path - validate and join directly to mount point
            let path_components: Vec<&str> = user_path.split('/').collect();

            // Check for unsafe path components
            for component in &path_components {
                if component == &".." || component == &"." {
                    return Err(anyhow!(
                        "Unsafe path component '{}' is not allowed in path '{}'",
                        component,
                        user_path
                    ));
                }
            }

            Ok(self.mount_point.join(user_path))
        }
    }

    /// Check if the mount point is already in use
    async fn is_mount_point_in_use(&self) -> Result<bool> {
        let output = Command::new("mountpoint")
            .arg("-q")
            .arg(&self.mount_point)
            .output()
            .await
            .context("Failed to execute mountpoint command")?;

        Ok(output.status.success())
    }

    /// Setup loop device and associate it with the image file
    async fn setup_loop_device(&self) -> Result<String> {
        // First, try to find an available loop device
        let output = Command::new("losetup")
            .args(["-f"])
            .output()
            .await
            .context("Failed to execute losetup -f command")?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to find available loop device: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let loop_device = String::from_utf8(output.stdout)
            .context("Invalid UTF-8 in losetup output")?
            .trim()
            .to_string();

        debug!("Found available loop device: {}", loop_device);

        // Associate the image file with the loop device
        let output = Command::new("losetup")
            .args([&loop_device, self.image_path.to_str().unwrap()])
            .output()
            .await
            .context("Failed to execute losetup command")?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to associate image with loop device: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        debug!("Associated image {:?} with loop device {}", self.image_path, loop_device);
        Ok(loop_device)
    }

    /// Mount the loop device to the mount point
    async fn mount_loop_device(&self, loop_device: &str) -> Result<()> {
        // Try to detect filesystem type first
        // let fs_type = self.detect_filesystem_type(loop_device).await?;

        debug!("Mount image {:?} with loop device {}", self.image_path, loop_device);

        let mut cmd = Command::new("mount");

        cmd.args(&[
            // "-t",
            // fs_type.as_str(),
            // "-o",
            // "loop",
            loop_device,
            &self.mount_point.to_string_lossy(),
        ]);

        let output = cmd.output().await.context("Failed to execute mount command")?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to mount loop device {} to {:?}: {}",
                // fs_type.as_str(),
                loop_device,
                self.mount_point,
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        debug!("Successfully mounted {} to {:?}", loop_device, self.mount_point);
        Ok(())
    }

    /// Detect filesystem type of the loop device
    async fn detect_filesystem_type(&self, loop_device: &str) -> Result<String> {
        // Try to detect filesystem type using blkid
        let output = Command::new("blkid")
            .arg("-s")
            .arg("TYPE")
            .arg("-o")
            .arg("value")
            .arg(loop_device)
            .output()
            .await
            .context("Failed to execute blkid command")?;

        if output.status.success() {
            let fs_type =
                String::from_utf8(output.stdout).context("Invalid UTF-8 in blkid output")?;

            let mut fs_types = fs_type.split(" ");
            let Some(fs_type) = fs_types.find(|s| s.starts_with("TYPE=")) else {
                return Err(anyhow!("filesystem type not found"));
            };
            let fs_type = fs_type.replace("TYPE=", "").replace("\"", "").replace("\n", "");

            debug!("Detected filesystem type: {}", fs_type);
            return Ok(fs_type);
        }

        // Fallback: try common filesystem types
        warn!("Could not detect filesystem type, trying common types");
        Ok("vfat".to_owned())
    }

    /// Unmount the filesystem from the mount point
    async fn unmount_filesystem(&self) -> Result<()> {
        let output = Command::new("umount")
            .arg(&self.mount_point)
            .output()
            .await
            .context("Failed to execute umount command")?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to unmount {:?}: {}",
                self.mount_point,
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        debug!("Successfully unmounted {:?}", self.mount_point);
        Ok(())
    }

    /// Cleanup loop device
    async fn cleanup_loop_device(&self, loop_device: &str) -> Result<()> {
        let output = Command::new("losetup")
            .args(["-d", loop_device])
            .output()
            .await
            .context("Failed to execute losetup -d command")?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to detach loop device {}: {}",
                loop_device,
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        debug!("Successfully detached loop device {}", loop_device);
        Ok(())
    }
}

/// File information structure for detailed directory listings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfo {
    /// File or directory name
    pub name: String,
    /// File size in bytes (0 for directories)
    pub size: u64,
    /// Whether this is a file
    pub is_file: bool,
    /// Whether this is a directory
    pub is_dir: bool,
    /// Last modification time as Unix timestamp (seconds since epoch, if available)
    pub modified: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileSystemInfo {
    /// Total space in bytes
    pub total_space: u64,
    /// Free space in bytes
    pub free_space: u64,
    /// Used space in bytes
    pub used_space: u64,
}

/// Async file reader for streaming file operations
pub struct AsyncFileReader {
    file: fs::File,
    chunk_size: usize,
    current_position: u64,
    file_size: u64,
}

impl AsyncFileReader {
    /// Read the next chunk of data
    ///
    /// # Returns
    /// * `Result<Option<Vec<u8>>>` - Next chunk of data, or None if end of file
    pub async fn read_next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        if self.current_position >= self.file_size {
            return Ok(None);
        }

        let remaining_bytes = self.file_size - self.current_position;
        let read_size = std::cmp::min(self.chunk_size, remaining_bytes as usize);

        let mut buffer = vec![0u8; read_size];
        let mut total_read = 0;

        // Loop reading until buffer is full or end of file is reached
        while total_read < read_size {
            let bytes_read =
                self.file.read(&mut buffer[total_read..]).await.with_context(|| {
                    format!("Failed to read chunk at position: {}", self.current_position)
                })?;

            if bytes_read == 0 {
                // Reached end of file
                break;
            }

            total_read += bytes_read;
        }

        buffer.truncate(total_read);
        self.current_position += total_read as u64;

        debug!(
            "Read chunk: {} bytes (position: {}/{})",
            total_read, self.current_position, self.file_size
        );

        if total_read == 0 { Ok(None) } else { Ok(Some(buffer)) }
    }

    /// Skip a specified number of bytes
    ///
    /// # Arguments
    /// * `bytes` - Number of bytes to skip
    ///
    /// # Returns
    /// * `Result<u64>` - Actual bytes skipped
    pub async fn skip_bytes(&mut self, bytes: u64) -> Result<u64> {
        let old_position = self.current_position;
        self.current_position = std::cmp::min(self.file_size, self.current_position + bytes);
        let skipped = self.current_position - old_position;

        use tokio::io::AsyncSeekExt;
        self.file
            .seek(std::io::SeekFrom::Start(self.current_position))
            .await
            .with_context(|| format!("Failed to seek to position: {}", self.current_position))?;

        debug!(
            "Skipped bytes: {} (position: {}/{})",
            skipped, self.current_position, self.file_size
        );
        Ok(skipped)
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Get current read position
    pub fn position(&self) -> u64 {
        self.current_position
    }

    /// Get total file size
    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    /// Check if at end of file
    pub fn is_eof(&self) -> bool {
        self.current_position >= self.file_size
    }

    /// Get remaining bytes to read
    pub fn remaining_bytes(&self) -> u64 {
        if self.current_position >= self.file_size {
            0
        } else {
            self.file_size - self.current_position
        }
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        Ok(self.file.shutdown().await?)
    }
}

/// Async file writer for streaming file operations
pub struct AsyncFileWriter {
    file: fs::File,
    chunk_size: usize,
    current_position: u64,
    bytes_written: u64,
}

impl AsyncFileWriter {
    /// Write a chunk of data
    ///
    /// # Arguments
    /// * `data` - Data to write
    ///
    /// # Returns
    /// * `Result<usize>` - Number of bytes written
    pub async fn write_chunk(&mut self, data: &[u8]) -> Result<usize> {
        // First, seek to the correct write position
        use tokio::io::AsyncSeekExt;
        self.file
            .seek(std::io::SeekFrom::Start(self.current_position))
            .await
            .with_context(|| format!("Failed to seek to position: {}", self.current_position))?;

        let mut remaining = data;
        let mut total_written = 0;

        // Loop writing until all data is written
        while !remaining.is_empty() {
            let bytes_written = self.file.write(remaining).await.with_context(|| {
                format!("Failed to write chunk at position: {}", self.current_position)
            })?;

            if bytes_written == 0 {
                return Err(anyhow!(
                    "Failed to write any data at position: {}",
                    self.current_position
                ));
            }

            remaining = &remaining[bytes_written..];
            total_written += bytes_written;
        }

        // Update position information
        self.current_position += total_written as u64;
        self.bytes_written += total_written as u64;

        debug!("Wrote chunk: {} bytes (position: {})", total_written, self.current_position);

        Ok(total_written)
    }

    /// Flush the file to disk
    pub async fn flush(&mut self) -> Result<()> {
        self.file.sync_all().await.context("Failed to flush file")?;
        Ok(())
    }

    /// Get current write position
    pub fn position(&self) -> u64 {
        self.current_position
    }

    /// Get total bytes written
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Set file size (truncate or extend)
    ///
    /// # Arguments
    /// * `size` - New file size
    pub async fn set_size(&mut self, size: u64) -> Result<()> {
        use tokio::io::AsyncSeekExt;
        self.file
            .seek(std::io::SeekFrom::Start(size))
            .await
            .with_context(|| format!("Failed to set file size to: {}", size))?;

        self.file.sync_all().await.context("Failed to sync file after setting size")?;

        Ok(())
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        Ok(self.file.shutdown().await?)
    }
}
