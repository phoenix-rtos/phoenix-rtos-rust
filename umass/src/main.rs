//! umass-rs: the USB Mass Storage driver, as a USB process driver.
//!
//! ```text
//! umass-rs [-r]
//! ```
//!
//! `-r` mounts the first device inserted as the root filesystem; otherwise
//! the driver waits for one to be mounted before it starts.

#![deny(unsafe_code)]

use std::process::ExitCode;

#[cfg(target_os = "phoenix")]
fn main() -> ExitCode {
    use std::time::Duration;

    /// Priority and number of the threads handling USB events, as the C driver.
    const USB_PRIO: i32 = 4;
    const USB_THREADS: u32 = 1;

    let mut auto_mount = umass::driver::AutoMount::Nothing;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-r" => auto_mount = umass::driver::AutoMount::Root,
            _ => {
                eprintln!("Usage: umass-rs [opts]\n  -r   Mount as rootfs\n  -h   Print this help");
                return if arg == "-h" { ExitCode::SUCCESS } else { ExitCode::FAILURE };
            }
        }
    }
    if auto_mount != umass::driver::AutoMount::Root {
        while phoenix::msg::lookup("/").is_err() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    match umass::driver::start(auto_mount) {
        Ok(driver) => umass::sys::usb::run(driver, USB_PRIO, USB_THREADS),
        Err(e) => {
            eprintln!("umass: failed to start: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "phoenix"))]
fn main() -> ExitCode {
    eprintln!("umass-rs runs on Phoenix-RTOS");
    ExitCode::FAILURE
}
