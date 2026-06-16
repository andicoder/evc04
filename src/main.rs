use evc04_charge::reported_current;

fn main() {
    // Placeholder entrypoint: the Modbus slave, MQTT client, and watchdog land here
    // against SPECS.md §5/§7/§9. For now, prove the control math wires up.
    let fuse_limit = 32.0;
    let target = 16.0;
    println!(
        "target {target} A -> report {} A household (fuse {fuse_limit} A)",
        reported_current(fuse_limit, target)
    );
}
