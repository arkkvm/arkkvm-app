//! Path Resolution Demonstration
//!
//! This example demonstrates RemoteFs path resolution and shows how the frontend should correctly use absolute paths.

use anyhow::Result;
use arkkvm::hardware::fs_remote::RemoteFs;
use tracing::{error, info, warn};

/// Path resolution demonstration
#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    info!("RemoteFs Path Resolution Demonstration");
    info!("=============================");
    info!("This demo shows how the frontend accesses the mounted file system using absolute paths");
    info!();

    // Create a RemoteFs instance
    let mut fs_manager = RemoteFs::new();

    info!("Default configuration:");
    info!("  Image file path: {}", fs_manager.image_path().display());
    info!("  Mount point: {}", fs_manager.mount_point().display());
    info!();

    // Attempt to mount (may fail because there is no image file)
    match fs_manager.mount().await {
        Ok(_) => {
            info!("✅ Successfully mounted file system");
            info!();

            // Path resolution demo
            info!("Path resolution demo:");
            info!("The frontend can use any of the following path formats, and they will be resolved correctly:");
            info!();

            let test_paths = vec![
                ("/", "Root directory"),
                ("/test.txt", "File under root directory"),
                ("/documents/readme.txt", "File under subdirectory"),
                ("/documents/subdir/file.txt", "File in a deep directory"),
                ("test.txt", "Relative path file"),
                ("documents/readme.txt", "Relative path subdirectory file"),
            ];

            for (path, description) in test_paths {
                info!("  📁 {} -> {}", path, description);
            }

            info!();
            info!("All paths starting with '/' will be automatically converted to paths relative to the mount point:");
            info!("  '/test.txt' -> '{}/test.txt'", fs_manager.mount_point().display());
            info!("  '/documents/file.txt' -> '{}/documents/file.txt'", fs_manager.mount_point().display());
            info!();

            // Actually test some file operations
            info!("File operation test:");

            // Write file (using an absolute path)
            let content = "This is a test file created using an absolute path.\n";
            match fs_manager.write_file_as_string("/demo_file.txt", content).await {
                Ok(_) => {
                    info!("✅ Successfully created file using absolute path '/demo_file.txt'");

                    // Read file for verification
                    match fs_manager.read_file_as_string("/demo_file.txt").await {
                        Ok(read_content) => {
                            info!("✅ Successfully read file contents: {}", read_content.trim());
                        }
                        Err(e) => error!("❌ Failed to read file: {}", e),
                    }
                }
                Err(e) => error!("❌ Failed to create file: {}", e),
            }

            // Create directory (using an absolute path)
            match fs_manager.create_directory("/demo_dir").await {
                Ok(_) => {
                    info!("✅ Successfully created directory using absolute path '/demo_dir'");

                    // Create a file inside the directory
                    match fs_manager.write_file_as_string("/demo_dir/nested_file.txt", "Nested file content").await {
                        Ok(_) => {
                            info!("✅ Successfully created nested file in the absolute-path directory");

                            // List directory contents
                            match fs_manager.list_directory("/demo_dir").await {
                                Ok(entries) => {
                                    info!("✅ Directory contents ({} items):", entries.len());
                                    for entry in entries {
                                        let entry_type = if entry.is_directory { "📁" } else { "📄" };
                                        info!("  {} {}", entry_type, entry.name);
                                    }
                                }
                                Err(e) => error!("❌ Failed to list directory: {}", e),
                            }
                        }
                        Err(e) => error!("❌ Failed to create nested file: {}", e),
                    }
                }
                Err(e) => error!("❌ Failed to create directory: {}", e),
            }

            // Test path validation
            info!();
            info!("Path validation test:");

            let unsafe_paths = vec![
                "./file.txt",
                "../file.txt",
                "/./file.txt",
                "/../file.txt",
                "/dir/./file.txt",
                "/dir/../file.txt",
            ];

            for unsafe_path in unsafe_paths {
                match fs_manager.write_file_as_string(unsafe_path, "Test content").await {
                    Ok(_) => error!("❌ Insecure path '{}' was incorrectly allowed", unsafe_path),
                    Err(e) => info!("✅ Correctly rejected insecure path '{}': {}", unsafe_path, e),
                }
            }

            // Test mixed path formats
            info!();
            info!("Mixed path format test:");

            // Create a file using a relative path
            match fs_manager.write_file_as_string("relative_file.txt", "Relative path file").await {
                Ok(_) => info!("✅ Successfully created file using relative path 'relative_file.txt'"),
                Err(e) => error!("❌ Failed to create relative path file: {}", e),
            }

            // List the root directory; it should show all created files
            match fs_manager.list_directory("/").await {
                Ok(entries) => {
                    info!("✅ Root directory contents ({} items):", entries.len());
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
                Err(e) => error!("❌ Failed to list root directory: {}", e),
            }

            // Clean up test files
            info!();
            info!("Cleaning up test files...");
            let _ = fs_manager.delete_file("/demo_file.txt").await;
            let _ = fs_manager.delete_file("/demo_dir/nested_file.txt").await;
            let _ = fs_manager.delete_directory("/demo_dir").await;
            let _ = fs_manager.delete_file("relative_file.txt").await;

            // Unmount
            match fs_manager.unmount().await {
                Ok(_) => info!("✅ Successfully unmounted file system"),
                Err(e) => error!("❌ Failed to unmount file system: {}", e),
            }
        }
        Err(e) => {
            warn!("⚠️ Mount failed (this is normal if the image file does not exist): {}", e);
            info!();
            info!("💡 Notes on path resolution and validation:");
            info!("   - The frontend can safely use absolute paths (e.g., '/', '/file.txt', '/dir/file.txt')");
            info!("   - These paths are automatically converted to paths relative to the mount point");
            info!("   - The frontend does not need to know the actual mount point path");
            info!("   - Relative paths also work properly");
            info!("   - Insecure paths (e.g., './file.txt', '../file.txt') are automatically rejected");
            info!("   - This prevents path traversal attacks and ensures file operations stay within the mounted file system");
            info!();
            info!("💡 To run the full demo, make sure you have an available image file");
        }
    }

    info!();
    info!("=============================");
    info!("Path resolution demo completed");
    info!("💡 The frontend can now safely use absolute paths to access the file system");
    info!("💡 Insecure paths are automatically rejected to keep the system safe");

    Ok(())
}
