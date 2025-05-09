mod config;
mod auth;
mod util;
mod logs;

use clap::{Arg, Command};
use config::Config;
use std::os::unix::process::CommandExt;
use std::process::{exit, Command as ProcessCommand};
use util::{get_user_groups, switch_user, run_command};
use auth::{verify_password, AuthState};
use logs::{init_logger, log_info, log_warn, log_error};
use nix::unistd::{getuid, geteuid, User};
use nix::libc;
use std::ffi::CStr;

/// Retrieve the real (invoking) user's username via their real UID.
fn real_username() -> String {
    let uid = getuid().as_raw();
    unsafe {
        let pw = libc::getpwuid(uid);
        if !pw.is_null() {
            let name_ptr = (*pw).pw_name;
            if !name_ptr.is_null() {
                return CStr::from_ptr(name_ptr)
                    .to_string_lossy()
                    .into_owned();
            }
        }
    }
    // Fallback to UID string if lookup fails
    uid.to_string()
}

fn main() {
    // Check for correct usage
    let uid = getuid().as_raw();
    let euid = geteuid().as_raw();

    if uid == 0 {
        eprintln!("Do not run 'elev' directly as root.");
        exit(1);
    }

    if euid != 0 {
        log_error("Error: 'elev' must be installed as setuid-root.");
        exit(1);
    }

    let matches = Command::new("elev")
        .version(env!("CARGO_PKG_VERSION"))
        .about("elev: a sudo/doas-like drop-in replacement")
        .arg(
            Arg::new("user")
                .short('u')
                .long("user")
                .help("Target user to run command as")
                .value_name("USER")
                .value_parser(clap::value_parser!(String))
                .default_value("root"),
        )
        .arg(
            Arg::new("login")
                .short('i')
                .long("login")
                .help("Run as login shell; skips command requirement")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("command")
                .required_unless_present("login")
                .required_unless_present("reset_auth")
                .num_args(1..)
                .allow_hyphen_values(true)
                .trailing_var_arg(true)
                .value_name("COMMAND")
                .help("Command to execute"),
        )
        .arg(
            Arg::new("reset_auth")
                .short('K')
                .long("reset-auth")
                .help("Invalidate cached credentials (clears authentication state)")
                .action(clap::ArgAction::SetTrue)
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .help("Enable verbose logging")
                .action(clap::ArgAction::SetTrue),
        )
        .get_matches();

    // Initialize logging
    let verbose = *matches.get_one::<bool>("verbose").unwrap_or(&false);
    init_logger(verbose);

    // Who invoked elev (real user)
    let current_user = real_username();
    let groups = get_user_groups(&current_user);

    // Who to run command as
    let target_user = matches.get_one::<String>("user").map(String::as_str).unwrap_or("root");

    let config = Config::load("/etc/elev.conf").expect("Failed to load config");
    let mut auth_state = AuthState::new(config.timeout, current_user.clone(), groups.clone(), &config);

    // Reset authentication timestamp (-K)
    if matches.get_flag("reset_auth") {
        auth_state.invalidate();
        log_info("Authentication state cleared.");
        println!("elev: authentication cache cleared");
        exit(0);
    }

    // Login shell mode (-i)
    if matches.get_flag("login") {
        // Switch to target user
        if let Err(e) = switch_user(target_user) {
            log_error(&format!("Failed to switch to user '{}': {}", target_user, e));
            exit(1);
        }

        // Lookup target user's info
        let user_entry = match User::from_name(target_user) {
            Ok(Some(u)) => u,
            Ok(None) => {
                log_error(&format!("User '{}' not found", target_user));
                exit(1);
            }
            Err(e) => {
                log_error(&format!("Failed to lookup user '{}': {}", target_user, e));
                exit(1);
            }
        };
        let home_dir = user_entry.dir;
        let shell_path = user_entry.shell;

        // Launch login shell
        let mut shell = ProcessCommand::new(&shell_path);
        shell.arg("-l"); // login shell flag
        shell.env("HOME", &home_dir);
        shell.env("USER", target_user);
        shell.env("LOGNAME", target_user);
        shell.env("SHELL", &shell_path);
        shell.env("PS1", r"\u@\h: \w\$ ");
        shell.current_dir(&home_dir);

        // Replace current process
        let err = shell.exec();
        log_error(&format!("Failed to exec login shell: {}", err));
        exit(1);
    }

    // Collect command and args
    let parts = matches
        .get_many::<String>("command")
        .expect("Command is required when not using -i")
        .collect::<Vec<_>>();
    let command = parts[0].as_str();
    let args: Vec<&str> = parts[1..].iter().map(|s| s.as_str()).collect();

    log_info(&format!(
        "elev invoked by '{}' to run '{}' as '{}'",
        current_user, command, target_user
    ));

    let config = Config::load("/etc/elev.conf").unwrap_or_else(|e| {
        log_error(&format!("Failed to load config: {}", e));
        exit(1);
    });

    let mut auth_state = AuthState::new(config.timeout, current_user.clone(), groups.clone(), &config);

    // Enforce timeout and password
    if !auth_state.check_timeout() {
        log_warn("Authentication timeout expired, re-enter password.");
        if !verify_password(&current_user, &mut auth_state, &config, &target_user, &command) {
            log_error("Authentication failed");
            exit(1);
        }
    }

    // Run the command
    run_command(command, &args, target_user, &config, &mut auth_state).unwrap_or_else(|e| {
        use std::io::ErrorKind;

        match e.kind() {
            ErrorKind::PermissionDenied => {
                eprintln!("elev: permission denied: '{}'", command);
            }
            ErrorKind::NotFound => {
                eprintln!("elev: command not found: '{}'", command);
            }
            ErrorKind::TimedOut => {
                eprintln!("elev: authentication timed out");
            }
            _ => {
                eprintln!("elev: error running command '{}': {}", command, e);
            }
        }

        log_error(&format!("Command failed: {}", e));
        exit(1);
    });
}
