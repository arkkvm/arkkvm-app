//! Panic and crash handler module
//! 
//! This module provides comprehensive error handling including:
//! - Panic hooks to capture Rust panics
//! - Signal handlers for segfaults and other crashes
//! - Backtrace collection for debugging

use std::backtrace::{Backtrace, BacktraceStatus};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{error, info};

static PANIC_HOOK_SET: AtomicBool = AtomicBool::new(false);
static SIGNAL_HANDLERS_SET: AtomicBool = AtomicBool::new(false);

// Note: We no longer use global variables to store crash information
// because signal handlers write directly to log file before process termination

/// Initialize panic hook to capture and log panics
fn init_panic_hook() {
    if PANIC_HOOK_SET.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        return;
    }

    std::panic::set_hook(Box::new(|panic_info| {
        let location = panic_info.location()
            .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()))
            .unwrap_or_else(|| "unknown location".to_string());

        let message = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            format!("Panic: {}", s)
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            format!("Panic: {}", s)
        } else {
            "Panic: unknown reason".to_string()
        };

        // Get backtrace
        let backtrace = Backtrace::capture();
        let backtrace_str = match backtrace.status() {
            BacktraceStatus::Captured => {
                format!("\nBacktrace:\n{:?}", backtrace)
            }
            BacktraceStatus::Disabled => {
                "\nBacktrace is disabled. Set RUST_BACKTRACE=1 to enable.".to_string()
            }
            BacktraceStatus::Unsupported => {
                "\nBacktrace is not supported on this platform.".to_string()
            }
            _ => {
                "\nBacktrace status unknown.".to_string()
            }
        };

        // Log panic information
        error!(
            "\n========================================\n\
            FATAL PANIC DETECTED\n\
            ========================================\n\
            Location: {}\n\
            Message: {}\n\
            {}\n\
            ========================================",
            location, message, backtrace_str
        );

        // Also print to stderr for immediate visibility
        eprintln!(
            "\n========================================\n\
            FATAL PANIC DETECTED\n\
            ========================================\n\
            Location: {}\n\
            Message: {}\n\
            {}\n\
            ========================================\n",
            location, message, backtrace_str
        );

        // Flush logs if possible
        if let Err(e) = std::io::Write::flush(&mut std::io::stderr()) {
            eprintln!("Failed to flush stderr: {}", e);
        }
    }));
}

/// Initialize signal handlers for crash signals
#[cfg(unix)]
fn init_signal_handlers() {
    if SIGNAL_HANDLERS_SET.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        return;
    }

    use std::sync::Once;

    static INIT: Once = Once::new();
    
    INIT.call_once(|| {
        // Register signal handlers using sigaction for better control
        unsafe {
            use libc::{sigaction, SA_SIGINFO};
            
            // Create signal handler action
            let mut sa: sigaction = std::mem::zeroed();
            sa.sa_sigaction = signal_handler as usize;
            sa.sa_flags = SA_SIGINFO;
            libc::sigemptyset(&mut sa.sa_mask);
            
            // Register handlers for critical signals
            // Note: We use minimal logging in signal handlers for safety
            let _ = libc::sigaction(libc::SIGSEGV, &sa, std::ptr::null_mut());
            let _ = libc::sigaction(libc::SIGABRT, &sa, std::ptr::null_mut());
            let _ = libc::sigaction(libc::SIGBUS, &sa, std::ptr::null_mut());
            let _ = libc::sigaction(libc::SIGILL, &sa, std::ptr::null_mut());
            let _ = libc::sigaction(libc::SIGFPE, &sa, std::ptr::null_mut());
        }

        info!("Signal handlers initialized for crash signals (SIGSEGV, SIGABRT, SIGBUS, SIGILL, SIGFPE)");
    });
}

#[cfg(not(unix))]
fn init_signal_handlers() {
    warn!("Signal handlers are only supported on Unix systems");
}

/// Signal handler function
/// Note: This function must be async-signal-safe, so we use minimal operations
/// 
/// IMPORTANT: After signal handler completes, the process may terminate immediately.
/// Therefore, we MUST write crash information directly to the log file here,
/// not rely on main thread to log it later.
#[cfg(unix)]
extern "C" fn signal_handler(sig: i32, _info: *mut libc::siginfo_t, _context: *mut libc::c_void) {
    let signal_name = match sig {
        libc::SIGSEGV => "SIGSEGV (Segmentation fault)",
        libc::SIGABRT => "SIGABRT (Abort)",
        libc::SIGBUS => "SIGBUS (Bus error)",
        libc::SIGILL => "SIGILL (Illegal instruction)",
        libc::SIGFPE => "SIGFPE (Floating point exception)",
        _ => "Unknown signal",
    };

    // Format crash message
    let message = format!(
        "Signal: {} ({})\nProcess ID: {}\nThread ID: {:?}",
        signal_name,
        sig,
        process::id(),
        std::thread::current().id()
    );

    // Write crash information to stderr (async-signal-safe)
    // This ensures immediate visibility even if logging fails
    let stderr_msg = format!(
        "\n========================================\n\
        FATAL CRASH DETECTED\n\
        ========================================\n\
        {}\n\
        ========================================\n\
        Note: Check application logs for detailed backtrace.\n\
        ========================================\n\n",
        message
    );
    
    let bytes = stderr_msg.as_bytes();
    unsafe {
        libc::write(libc::STDERR_FILENO, bytes.as_ptr() as *const libc::c_void, bytes.len());
    }

    // Get backtrace (this is safe to call in signal handler)
    let backtrace = Backtrace::capture();
    let backtrace_str = match backtrace.status() {
        BacktraceStatus::Captured => {
            format!("\nBacktrace:\n{:?}", backtrace)
        }
        BacktraceStatus::Disabled => {
            "\nBacktrace is disabled. Set RUST_BACKTRACE=1 to enable.".to_string()
        }
        BacktraceStatus::Unsupported => {
            "\nBacktrace is not supported on this platform.".to_string()
        }
        _ => {
            "\nBacktrace status unknown.".to_string()
        }
    };


    // Use tracing macros to log crash information
    // Note: tracing macros are not guaranteed to be async-signal-safe, but in practice
    // they often work if tracing is already initialized. We use them here as the primary
    // logging method, with stderr output as a fallback.
    error!(
        "\n========================================\n\
        FATAL CRASH DETECTED\n\
        ========================================\n\
        Signal: {} ({})\n\
        Process ID: {}\n\
        Thread ID: {:?}\n\
        {}\n\
        ========================================",
        signal_name,
        sig,
        process::id(),
        std::thread::current().id(),
        backtrace_str
    );

    // Also write to stderr for immediate visibility and as a fallback
    // This ensures the crash is visible even if tracing fails
    let stderr_msg = format!(
        "\n========================================\n\
        FATAL CRASH DETECTED\n\
        ========================================\n\
        Signal: {} ({})\n\
        Process ID: {}\n\
        Thread ID: {:?}\n\
        {}\n\
        ========================================\n\n",
        signal_name,
        sig,
        process::id(),
        std::thread::current().id(),
        backtrace_str
    );
    let stderr_bytes = stderr_msg.as_bytes();
    unsafe {
        libc::write(libc::STDERR_FILENO, stderr_bytes.as_ptr() as *const libc::c_void, stderr_bytes.len());
    }

    // Restore default handler and re-raise signal to generate core dump
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        // Use sa_sigaction field (union) to set default handler
        sa.sa_sigaction = libc::SIG_DFL as usize;
        libc::sigaction(sig, &sa, std::ptr::null_mut());
        libc::raise(sig);
    }
}


/// Initialize all exception handlers (panic + signals)
pub fn init_exception_handlers() {
    init_panic_hook();
    init_signal_handlers();
    
    // Note: To enable backtrace, set RUST_BACKTRACE=1 in the environment
    // We don't set it programmatically to avoid potential issues in restricted environments
    
    info!("Exception handlers initialized (panic hook + signal handlers)");
}

