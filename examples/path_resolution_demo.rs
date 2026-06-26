//! Path resolution demo
//!
//! Demonstrates RemoteFs path resolution and how the frontend should use absolute paths.

use anyhow::Result;
use arkkvm::hardware::fs_remote::RemoteFs;
use tracing::{error, info, warn};

/// Demonstrate path resolution
#[tokio::main]
async fn main() -> Result<()> {
    // init logging
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    
    info!("RemoteFs path resolution demo");
    info!("=============================");
    info!("This demo shows how the frontend can use absolute paths on the mounted filesystem");
    info!();
    
    // create RemoteFs instance
    let mut fs_manager = RemoteFs::new();
    
    info!("Default config:");
    info!("  image path: {}", fs_manager.image_path().display());
    info!("  mount point: {}", fs_manager.mount_point().display());
    info!();
    
    // try mount (may fail if no image file exists)
    match fs_manager.mount().await {
        Ok(_) => {
            info!("✅ filesystem mounted");
            info!();
            
            // path resolution demo
            info!("Path resolution demo:");
            info!("The frontend may use any of the following path forms; all are resolved correctly:");
            info!();
            
            let test_paths = vec![
                ("/", "root directory"),
                ("/test.txt", "file at root"),
                ("/documents/readme.txt", "file in subdirectory"),
                ("/documents/subdir/file.txt", "file in nested directory"),
                ("test.txt", "relative path file"),
                ("documents/readme.txt", "relative path in subdirectory"),
            ];
            
            for (path, description) in test_paths {
                info!("  📁 {} -> {}", path, description);
            }
            
            info!();
            info!("Paths starting with '/' are converted relative to the mount point:");
            info!("  '/test.txt' -> '{}/test.txt'", fs_manager.mount_point().display());
            info!("  '/documents/file.txt' -> '{}/documents/file.txt'", fs_manager.mount_point().display());
            info!();
            
            // file operation tests
            info!("File operation tests:");
            
            // write file (absolute path)
            let content = "Test file created via absolute path.\n";
            match fs_manager.write_file_as_string("/demo_file.txt", content).await {
                Ok(_) => {
                    info!("✅ created '/demo_file.txt' via absolute path");
                    
                    // read back
                    match fs_manager.read_file_as_string("/demo_file.txt").await {
                        Ok(read_content) => {
                            info!("✅ read file content: {}", read_content.trim());
                        }
                        Err(e) => error!("❌ failed to read file: {}", e),
                    }
                }
                Err(e) => error!("❌ failed to create file: {}", e),
            }
            
            // create directory (absolute path)
            match fs_manager.create_directory("/demo_dir").await {
                Ok(_) => {
                    info!("✅ created '/demo_dir' via absolute path");
                    
                    // nested file
                    match fs_manager.write_file_as_string("/demo_dir/nested_file.txt", "nested file content").await {
                        Ok(_) => {
                            info!("✅ created nested file under absolute path directory");
                            
                            // list directory
                            match fs_manager.list_directory("/demo_dir").await {
                                Ok(entries) => {
                                    info!("✅ directory entries ({}):", entries.len());
                                    for entry in entries {
                                        let entry_type = if entry.is_directory { "📁" } else { "📄" };
                                        info!("  {} {}", entry_type, entry.name);
                                    }
                                }
                                Err(e) => error!("❌ failed to list directory: {}", e),
                            }
                        }
                        Err(e) => error!("❌ failed to create nested file: {}", e),
                    }
                }
                Err(e) => error!("❌ failed to create directory: {}", e),
            }
            
            // path validation tests
            info!();
            info!("Path validation tests:");
            
            let unsafe_paths = vec![
                "./file.txt",
                "../file.txt",
                "/./file.txt",
                "/../file.txt",
                "/dir/./file.txt",
                "/dir/../file.txt",
            ];
            
            for unsafe_path in unsafe_paths {
                match fs_manager.write_file_as_string(unsafe_path, "test content").await {
                    Ok(_) => error!("❌ unsafe path '{}' was incorrectly allowed", unsafe_path),
                    Err(e) => info!("✅ correctly rejected unsafe path '{}': {}", unsafe_path, e),
                }
            }
            
            // mixed path formats
            info!();
            info!("Mixed path format tests:");
            
            // relative path file
            match fs_manager.write_file_as_string("relative_file.txt", "relative path file").await {
                Ok(_) => info!("✅ created 'relative_file.txt' via relative path"),
                Err(e) => error!("❌ failed to create relative path file: {}", e),
            }
            
            // list root
            match fs_manager.list_directory("/").await {
                Ok(entries) => {
                    info!("✅ root directory entries ({}):", entries.len());
                    for entry in entries {
                        let entry_type = if entry.is_directory { "📁" } else { "📄" };
                        let size_info = if entry.is_file && entry.size > 0 {
                            format!(" ({} bytes)", entry.size)
                        } else {
                            "".to_string()
                        };
                        info!("  {} {}{}", entry_type, entry.name, size_info);
                    }
                }
                Err(e) => error!("❌ failed to list root directory: {}", e),
            }
            
            // cleanup
            info!();
            info!("Cleaning up test files...");
            let _ = fs_manager.delete_file("/demo_file.txt").await;
            let _ = fs_manager.delete_file("/demo_dir/nested_file.txt").await;
            let _ = fs_manager.delete_directory("/demo_dir").await;
            let _ = fs_manager.delete_file("relative_file.txt").await;
            
            // unmount
            match fs_manager.unmount().await {
                Ok(_) => info!("✅ filesystem unmounted"),
                Err(e) => error!("❌ failed to unmount filesystem: {}", e),
            }
        }
        Err(e) => {
            warn!("⚠️  mount failed (expected if no image file exists): {}", e);
            info!();
            info!("💡 Path resolution and validation notes:");
            info!("   - Frontend may safely use absolute paths (e.g. '/', '/file.txt', '/dir/file.txt')");
            info!("   - These are converted relative to the mount point automatically");
            info!("   - Frontend does not need to know the actual mount point path");
            info!("   - Relative paths work as well");
            info!("   - Unsafe paths (e.g. './file.txt', '../file.txt') are rejected");
            info!("   - Prevents path traversal; file ops stay within the mounted filesystem");
            info!();
            info!("💡 For the full demo, ensure a usable image file is available");
        }
    }
    
    info!();
    info!("=============================");
    info!("Path resolution demo complete");
    info!("💡 Frontend may safely use absolute paths to access the filesystem");
    info!("💡 Unsafe paths are rejected to keep the system secure");
    
    Ok(())
}
