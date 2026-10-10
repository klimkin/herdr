pub fn own_process_group(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}
pub fn kill_process_group(child: &mut std::process::Child) {
    // Only the process group created for this benchmark's owned server.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
}
