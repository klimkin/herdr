pub fn own_process_group(_command: &mut std::process::Command) {}
pub fn kill_process_group(child: &mut std::process::Child) {
    let _ = child.kill();
}
