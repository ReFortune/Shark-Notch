#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
#[macro_use]
mod log;
#[cfg(windows)]
mod app;
#[cfg(windows)]
mod diag;
#[cfg(windows)]
mod gfx;
#[cfg(windows)]
mod services;
#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() {
    std::process::exit(startup::run());
}

#[cfg(windows)]
mod startup {
    use crate::app::{self, Options};
    use crate::win::{paths, single};
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };

    fn parse_args() -> Options {
        let mut o = Options::default();
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            match a.as_str() {
                "--selftest" => o.selftest = true,
                "--out" => o.out = args.next().map(Into::into),
                "--config" => o.config = args.next().map(Into::into),
                "--no-exclude" => o.no_exclude = true,
                "--light-probe" => o.light_probe = true,
                "--registry-probe" => o.registry_probe = true,
                "--console" => o.console = true,
                _ => {}
            }
        }
        o
    }

    pub fn run() -> i32 {
        let opts = parse_args();
        // A GUI-subsystem exe has no console; attach the parent's so `--selftest` can print to it.
        if opts.selftest || opts.console {
            unsafe {
                let _ = AttachConsole(ATTACH_PARENT_PROCESS);
            }
        }
        // Per-monitor v2: coordinates are physical pixels and windows get WM_DPICHANGED.
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        // The self-test works in a scratch folder: it must never read or overwrite a real user's
        // saved data (pins, tasks) and it runs with the default configuration.
        if opts.selftest {
            let dir = std::env::temp_dir().join("SharkNotch-selftest");
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::create_dir_all(&dir);
            paths::set_data_dir(dir);
        }
        crate::log::init(
            &paths::log_path(),
            crate::log::Level::Info,
            opts.selftest || opts.console,
        );
        // Self-tests must not collide with a real running instance.
        let _guard = if opts.selftest {
            None
        } else {
            single::acquire("SharkNotch.Instance")
        };
        if !opts.selftest && _guard.is_none() {
            return 0; // another instance is running and has been told
        }
        crate::info!(
            "Shark Notch {} starting (pid {})",
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        );
        let code = app::run(opts);
        crate::info!("exiting with code {code}");
        code
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "shark-notch only runs on Windows. On other systems use `cargo test -p notch-core` and `cargo run -p notch-preview`."
    );
}
