fn main() {
    #[cfg(target_os = "linux")]
    if std::env::args().nth(1).as_deref() == Some("install-preflight") {
        if let Err(error) = install_preflight() {
            eprintln!("PariNS installer preflight: {error}");
            std::process::exit(1);
        }
        return;
    }
    run();
}

#[tokio::main(flavor = "current_thread")]
async fn run() {
    if let Err(error) = parins::update::executor::run_cli().await {
        eprintln!("PariNS updater: {error}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
fn install_preflight() -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use nix::{
        sched::{CloneFlags, setns},
        unistd::{Gid, Uid, geteuid, setgroups, setresgid, setresuid},
    };
    use rustix::thread::{
        CapabilitiesSecureBits, CapabilitySet, CapabilitySets, capability_is_in_bounding_set,
        clear_ambient_capability_set, remove_capability_from_bounding_set, set_capabilities,
        set_capabilities_secure_bits, set_no_new_privs,
    };
    use std::{fs::File, os::unix::process::CommandExt, process::Command};

    let args: Vec<_> = std::env::args().skip(2).collect();
    let [mount, uid, gid, groups] = args.as_slice() else {
        anyhow::bail!("invalid installer preflight arguments");
    };
    ensure!(geteuid().is_root(), "preflight requires root");
    let mount = mount
        .strip_prefix("--mount=")
        .context("missing namespace")?;
    let uid = Uid::from_raw(uid.parse()?);
    let gid = Gid::from_raw(gid.parse()?);
    let groups = if groups.is_empty() {
        Vec::new()
    } else {
        groups
            .split(',')
            .map(|group| group.parse().map(Gid::from_raw))
            .collect::<Result<Vec<_>, _>>()?
    };
    ensure!(
        !uid.is_root() && gid.as_raw() != 0 && groups.iter().all(|g| g.as_raw() != 0),
        "privileged preflight identity"
    );

    let namespace = File::open(mount)?;
    setns(&namespace, CloneFlags::CLONE_NEWNS)?;
    drop(namespace);
    set_no_new_privs(true)?;
    set_capabilities_secure_bits(CapabilitiesSecureBits::empty())?;
    clear_ambient_capability_set()?;
    for bit in 0..64 {
        let capability = CapabilitySet::from_bits_retain(1 << bit);
        match capability_is_in_bounding_set(capability) {
            Ok(true) => remove_capability_from_bounding_set(capability)?,
            Ok(false) => {}
            Err(rustix::io::Errno::INVAL) => break,
            Err(error) => return Err(error.into()),
        }
    }
    setgroups(&groups)?;
    setresgid(gid, gid, gid)?;
    setresuid(uid, uid, uid)?;
    set_capabilities(
        None,
        CapabilitySets {
            effective: CapabilitySet::empty(),
            permitted: CapabilitySet::empty(),
            inheritable: CapabilitySet::empty(),
        },
    )?;

    Err(
        Command::new("/opt/parins-managed/.update/install-preflight")
            .env_clear()
            .env("LANG", "C")
            .args([
                "--manage",
                "--check",
                "--state-dir",
                "/var/lib/parins-managed",
                "--web-listen",
                "0.0.0.0:3000",
            ])
            .exec()
            .into(),
    )
}
