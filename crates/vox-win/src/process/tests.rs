//! The test binary re-executes itself as the helper (and the helper's own child): [`child_entry`] runs the role named
//! in [`ROLE`] and exits, so a normal test run passes it straight through.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::*;

const ROLE: &str = "VOX_WIN_PROCESS_TEST_ROLE";
const PID_PREFIX: &str = "vox-test-pid:";
const DEATH_WAIT: Duration = Duration::from_secs(10);

/// Not a test: the body of the re-executed processes. Without [`ROLE`] it returns at once.
#[test]
fn child_entry() {
    let Ok(role) = std::env::var(ROLE) else { return };
    match role.as_str() {
        "sleep" => thread::sleep(Duration::from_secs(60)),
        "exit-3" => std::process::exit(3),
        // Starts an uncontained child of its own (it inherits the job / process group), reports its pid, sleeps.
        "spawn-grandchild" => {
            // Never waited for: it dies with the tree, and init reaps it.
            #[allow(clippy::zombie_processes)]
            let grandchild = role_command("sleep").stdout(Stdio::null()).spawn().expect("spawn the grandchild");
            report_pid(grandchild.id());
            thread::sleep(Duration::from_secs(60));
        }
        // Stands in for Vox: spawns a contained helper, reports its pid, and on a line from stdin exits the way a
        // crash or `process::exit` does, without dropping the helper.
        // Its stdout is not ours: the test closes our pipe once it has the pid, and a helper still writing to it
        // would die of the broken pipe instead of the containment under test.
        "vox-then-exit" => {
            let mut helper = role_command("sleep");
            helper.stdout(Stdio::null());
            let helper = Spawner::global().spawn(helper).expect("spawn the helper");
            report_pid(helper.id());
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            std::process::exit(0);
        }
        // Launched by a parent that ignores SIGCHLD: the spawner must still be able to wait for its helpers.
        #[cfg(not(windows))]
        "ignore-sigchld" => {
            // SAFETY: plain signal call; no handler runs.
            unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };
            let mut helper = Spawner::global().spawn(role_command("exit-3")).expect("spawn the helper");
            let code = helper.wait_timeout(DEATH_WAIT).ok().flatten().and_then(|s| s.code());
            let missing = Spawner::global().spawn(Command::new("/nonexistent/vox-helper")).err();
            let ok = code == Some(3) && missing.is_some_and(|e| classify_spawn_error(&e) == SpawnBlock::NotFound);
            std::process::exit(if ok { 0 } else { 1 });
        }
        other => panic!("unknown role {other}"),
    }
    std::process::exit(0);
}

/// This test binary, running only [`child_entry`] in `role`.
fn role_command(role: &str) -> Command {
    let mut cmd = Command::new(std::env::current_exe().expect("the test binary"));
    cmd.args(["process::tests::child_entry", "--exact", "--nocapture", "--test-threads=1"])
        .env(ROLE, role)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

fn report_pid(pid: u32) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{PID_PREFIX}{pid}");
    let _ = out.flush();
}

/// The first pid a re-executed process reports on stdout. libtest's `test … ... ` has no newline, so the report can
/// sit mid-line.
fn read_pid(stdout: impl std::io::Read) -> u32 {
    BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .find_map(|line| line.split_once(PID_PREFIX).map(|(_, pid)| pid.trim().parse().expect("a pid")))
        .expect("the child reported a pid")
}

fn spawn_role(role: &str, stdout: bool) -> ContainedChild {
    let mut cmd = role_command(role);
    cmd.stdout(if stdout { Stdio::piped() } else { Stdio::null() });
    Spawner::global().spawn(cmd).expect("spawn")
}

#[cfg(not(windows))]
fn is_alive(pid: u32) -> bool {
    // A zombie (Z) or a dead task (X) counts as gone: it runs nothing and is only waiting to be reaped.
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return false };
    let state = stat.rsplit_once(')').and_then(|(_, rest)| rest.trim_start().chars().next());
    !matches!(state, Some('Z' | 'X') | None)
}

#[cfg(windows)]
fn is_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};

    // SAFETY: the handle is closed below.
    let Ok(process) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else { return false };
    // SAFETY: `process` is open.
    let running = unsafe { WaitForSingleObject(process, 0) } == WAIT_TIMEOUT;
    // SAFETY: we own `process`.
    let _ = unsafe { CloseHandle(process) };
    running
}

fn dies_within(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while is_alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
    true
}

#[test]
fn kill_tree_kills_the_helper_and_its_child() {
    let mut helper = spawn_role("spawn-grandchild", true);
    let grandchild = read_pid(helper.take_stdout().unwrap());
    assert!(is_alive(helper.id()) && is_alive(grandchild));
    let status = helper.kill_tree().expect("kill_tree");
    assert!(!status.success());
    assert!(!is_alive(helper.id()));
    assert!(dies_within(grandchild, DEATH_WAIT), "the grandchild outlived kill_tree");
    // A second call returns the reaped status without signalling anything.
    assert_eq!(helper.kill_tree().unwrap(), status);
}

#[test]
fn dropping_the_helper_kills_its_tree() {
    let mut helper = spawn_role("spawn-grandchild", true);
    let grandchild = read_pid(helper.take_stdout().unwrap());
    let pid = helper.id();
    drop(helper);
    assert!(!is_alive(pid));
    assert!(dies_within(grandchild, DEATH_WAIT), "the grandchild outlived the drop");
}

/// The PR_SET_PDEATHSIG trap: the death signal follows the thread that forked, so a helper asked for by a thread
/// that then exits must still run.
#[test]
fn a_helper_outlives_the_thread_that_asked_for_it() {
    let mut helper = thread::spawn(|| spawn_role("sleep", false)).join().unwrap();
    thread::sleep(Duration::from_millis(300));
    assert_eq!(helper.try_exit_status().unwrap(), None, "the helper died with its requesting thread");
    assert!(is_alive(helper.id()));
    helper.kill_tree().unwrap();
}

/// What the job object and the death signal are for: Vox exiting without dropping its helpers.
#[test]
fn a_helper_dies_when_vox_exits_without_cleaning_up() {
    let mut vox = role_command("vox-then-exit").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let helper = read_pid(vox.stdout.take().unwrap());
    // Still running a moment later, so the death below is Vox's exit and nothing else.
    thread::sleep(Duration::from_millis(300));
    assert!(is_alive(helper));
    vox.stdin.take().unwrap().write_all(b"exit\n").unwrap();
    assert!(vox.wait().unwrap().success());
    assert!(dies_within(helper, DEATH_WAIT), "the helper outlived Vox");
}

#[cfg(not(windows))]
#[test]
fn an_inherited_ignored_sigchld_is_undone() {
    let status = role_command("ignore-sigchld").status().unwrap();
    assert!(status.success(), "{status}");
}

#[test]
fn the_exit_status_is_read_without_reaping_the_helper() {
    let mut helper = spawn_role("exit-3", false);
    let status = helper.wait_timeout(DEATH_WAIT).unwrap().expect("the helper exited");
    assert_eq!(status.code(), Some(3));
    // Not reaped: on Linux the zombie keeps the process group id reserved for kill_tree.
    #[cfg(not(windows))]
    assert!(std::fs::read_to_string(format!("/proc/{}/stat", helper.id())).is_ok());
    assert_eq!(helper.try_exit_status().unwrap(), Some(status));
    assert_eq!(helper.kill_tree().unwrap(), status);
}

#[test]
fn wait_timeout_gives_up_on_a_running_helper() {
    let mut helper = spawn_role("sleep", false);
    let started = Instant::now();
    assert_eq!(helper.wait_timeout(Duration::from_millis(100)).unwrap(), None);
    assert!(started.elapsed() >= Duration::from_millis(100));
    helper.kill_tree().unwrap();
}

#[test]
fn a_missing_program_is_not_found() {
    let missing = std::env::temp_dir().join(format!("vox-missing-helper-{}", std::process::id())).join("helper.exe");
    let Err(e) = Spawner::global().spawn(Command::new(missing)) else { panic!("a missing program started") };
    assert_eq!(classify_spawn_error(&e), SpawnBlock::NotFound);
}

#[cfg(not(windows))]
#[test]
fn a_program_without_the_execute_bit_cannot_run() {
    let dir = std::env::temp_dir().join(format!("vox-noexec-helper-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let program = dir.join("helper");
    std::fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
    let result = Spawner::global().spawn(Command::new(&program));
    std::fs::remove_dir_all(&dir).unwrap();
    let Err(e) = result else { panic!("a non-executable file started") };
    assert_eq!(classify_spawn_error(&e), SpawnBlock::NoExec);
}

#[test]
fn spawn_errors_are_classified() {
    let os = io::Error::from_raw_os_error;
    #[cfg(windows)]
    let cases = [
        (os(2), SpawnBlock::NotFound),
        (os(3), SpawnBlock::NotFound),
        (os(5), SpawnBlock::Blocked),
        (os(225), SpawnBlock::Blocked),
        (os(1260), SpawnBlock::Blocked),
        (os(4551), SpawnBlock::Blocked),
        (os(193), SpawnBlock::NoExec),
        (os(216), SpawnBlock::NoExec),
        (os(8), SpawnBlock::Other),
    ];
    #[cfg(not(windows))]
    let cases = [
        (os(libc::ENOENT), SpawnBlock::NotFound),
        (os(libc::ENOTDIR), SpawnBlock::NotFound),
        (os(libc::EPERM), SpawnBlock::Blocked),
        (os(libc::EACCES), SpawnBlock::NoExec),
        (os(libc::ENOEXEC), SpawnBlock::NoExec),
        (os(libc::ENOMEM), SpawnBlock::Other),
    ];
    for (e, block) in cases {
        assert_eq!(classify_spawn_error(&e), block, "{e}");
    }
    assert_eq!(classify_spawn_error(&io::Error::from(io::ErrorKind::NotFound)), SpawnBlock::NotFound);
    assert_eq!(classify_spawn_error(&io::Error::other("spawner gone")), SpawnBlock::Other);
}
