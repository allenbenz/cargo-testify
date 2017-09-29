use notify::{RecommendedWatcher, Watcher, Event};

#[cfg(not(target_os = "windows"))]
use notify_rust::Notification;
#[cfg(target_os = "windows")]
use winrt_notification;

use std::process::{Command, Stdio};
use std::time::Instant;
use std::io::{BufRead, BufReader};
use std::sync::{Arc, Mutex};
use std::thread;
use std::process;
use std::sync::mpsc::channel;
use std::path::Path;

use config::Config;
use report_builder::ReportBuilder;
use report::{Outcome, Report};

pub struct Reactor<'a> {
    config: Config<'a>,
    last_run_at: Instant,
    report_builder: ReportBuilder
}

impl<'a> Reactor<'a> {
    pub fn new(config: Config<'a>) -> Self {
        Self {
            config,
            last_run_at: Instant::now(),
            report_builder: ReportBuilder::new()
        }
    }

    pub fn start(&mut self) {
        let (tx, rx) = channel();
        let mut watcher: RecommendedWatcher = Watcher::new(tx).expect("Failed to obtain a watcher");
        watcher.watch(&self.config.project_dir).expect("Failed to start watcher");

        self.last_run_at = Instant::now();
        self.run_tests();

        loop {
            match rx.recv() {
                Ok(event) => {
                    if self.should_react(event) {
                        self.run_tests();
                        self.last_run_at = Instant::now();
                    }
                },
                Err(err) => {
                    eprintln!("Unexpected error occurred:");
                    eprintln!("  {:?}", err);
                    process::exit(1);
                }
            }
        }
    }

    fn should_react(&self, event: Event) -> bool {
        // ignore event if tests just finished very recently
        if Instant::now() - self.last_run_at < self.config.ignore_duration {
            return false;
        }

        match event.path {
            Some(path) => filter_allows(self.config.project_dir.as_path(), path.as_path()),
            None => false
        }
    }

    /// Spawn `cargo test` and catch stdout and stderr, then build report and call notifier.
    /// TODO: Number of things can and have to be improved here:
    ///   * Preserve color output of `cargo test`
    ///   * Is it possible intercept stdout and stderr in one thread using futures?
    fn run_tests(&self) {
        let mut args = self.config.cargo_test_args.clone();
        args.insert(0, "test");

        let result = Command::new("cargo")
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        match result {
            Ok(mut child) => {
                // Catch stdout
                let stdout = child.stdout.take().unwrap();
                let stdout_buf_reader = BufReader::new(stdout);
                let stdout_buffer = Arc::new(Mutex::new(String::new()));
                let stdout_buffer_clone = stdout_buffer.clone();
                thread::spawn(move || {
                    for line in stdout_buf_reader.lines() {
                        let line = line.unwrap();
                        let mut buffer = stdout_buffer_clone.lock().unwrap();
                        buffer.push_str(&line);
                        buffer.push('\n');
                        //println!("{}", line);
                    }
                });

                // Catch stderr
                let stderr = child.stderr.take().unwrap();
                let stderr_buf_reader = BufReader::new(stderr);
                let stderr_buffer = Arc::new(Mutex::new(String::new()));
                let stderr_buffer_clone = stderr_buffer.clone();
                thread::spawn(move || {
                    for line in stderr_buf_reader.lines() {
                        let line = line.unwrap();
                        let mut buffer = stderr_buffer_clone.lock().unwrap();
                        buffer.push_str(&line);
                        buffer.push('\n');
                        //eprintln!("{}", line);
                    }
                });

                let exit_status = child.wait().expect("failed to wait for child process `cargo test`");
                let stdout_output = stdout_buffer.lock().unwrap().clone();
                let stderr_output = stderr_buffer.lock().unwrap().clone();

                let report = self.report_builder.identify(exit_status.success(), &stdout_output, &stderr_output);
                notify(report)
            }
            Err(err) => {
                eprintln!("Failed to spawn `cargo test`");
                eprintln!("{:?}", err);
                process::exit(1);
            }
        }

        Command::new("cargo")
            .args(args)
            .spawn()
            .expect("write to console cargo test run failed");
    }
}

#[cfg(not(target_os = "windows"))]
fn notify(report: Report) {
    let icon = match report.outcome {
        Outcome::TestsPassed => "face-angel",
        Outcome::TestsFailed | Outcome::CompileError => "face-angry"
    };
    let mut notification = Notification::new()
        .summary(report.title())
        .icon(icon)
        .finalize();
    if let Some(detail) = report.detail {
        notification.body(&detail);
    }
    notification
        .show()
        .expect("unable to send notification");
}

#[cfg(target_os = "windows")]
fn notify(report: Report) {
    let icon = match report.outcome {
        Outcome::TestsPassed => "🔵",
        Outcome::TestsFailed | Outcome::CompileError => "🔴"
    };
    let sound = match report.outcome {
        Outcome::TestsPassed | Outcome::CompileError => None,
        Outcome::TestsFailed => Some(winrt_notification::Sound::SMS)
    };
    winrt_notification::Toast::new("cargo-testify")
        .title(&format!("{} {}", report.title(), icon))
        .text1(&report.detail.unwrap_or("".to_owned()))
        .sound(sound)
        .duration(winrt_notification::Duration::Short)
        .show()
        .expect("unable to send notification");
}

/// Should changes in `path` file trigger running the test suite?
fn filter_allows(project_dir: &Path, path: &Path) -> bool {
    const FILES: &'static [&'static str] = &[
        "src",
        "tests",
        "Cargo.toml",
        "Cargo.lock",
        "build.rs",
    ];

    FILES.iter().any(|file| {
        let absolute_file_path = project_dir.join(file);
        path.starts_with(absolute_file_path)
    })
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const PROJECT_DIR: &'static str = "/project";

    fn must_allow(path: &str) {
        let project = PathBuf::from(PROJECT_DIR);
        let path = PathBuf::from(path);
        assert!(filter_allows(project.as_path(), path.as_path()));
    }

    fn must_not_allow(path: &str) {
        let project = PathBuf::from(PROJECT_DIR);
        let path = PathBuf::from(path);
        assert!(!filter_allows(project.as_path(), path.as_path()));
    }

    #[test]
    fn test_filter_allows() {
        must_allow("/project/src/main.rs");
        must_allow("/project/src/lib/os.rs");
        must_allow("/project/tests/watch.rs");
        must_allow("/project/Cargo.toml");
        must_allow("/project/Cargo.lock");
        must_allow("/project/build.rs");

        must_not_allow("/project/README.md");
        must_not_allow("/tmp/file.rs");
        must_not_allow("/tmp/src/file.rs");
    }
}
