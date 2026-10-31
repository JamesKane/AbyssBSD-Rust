// SPDX-License-Identifier: BSD-2-Clause
//! Suite `p7` (docs/NETWORKING.md §17, tests/perf/README.md): the network
//! components, over loopback and over virtio-net to the build server, which
//! runs the far end of each benchmark on the test bridge.
//!
//! Each run boots the installed root with a DHCP `vtnet0` on a tap of the
//! bridge, logs in on the serial console, and runs the bench disk's
//! [`RUN`]: `ubench p7`, then `iperf3`. The server's side, `iperf3 -s` and
//! `ubench p7.peer`, runs for the whole command ([`Peers`]).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use super::{Dirs, doas, parse};
use crate::arch::Arch;
use crate::expect::Session;
use crate::qemu::{Net, Tap, Vm, Vmm};
use crate::util::{self, Result};

/// `iperf3` from FreeBSD's packages, the same build on both guests and as
/// the server's peer: not part of base, so kept, checked, in the
/// distribution directory.
const IPERF3: &str = "iperf3-3.22";
const IPERF3_SHA256: &str = "b8cc467ebe58ff020a95515e0c8eb9d53e3ad0a0cd6c5a232b11aaeb6885666e";

/// The test bridge's address, where the peers listen.
const PEER: &str = "10.77.0.1";
/// The guest's vCPUs: pinned to the server's physical cores 0 to 3
/// ([`crate::qemu::PIN_STRIDE`]), leaving cores 4 to 7 (host CPUs 8 to 15)
/// to the peers, so neither side's throughput is the other's bottleneck.
const SMP: u32 = 4;
const PEER_CPUS: &str = "8-15";

/// What the guest runs once root has logged in, given the peer's address:
/// the files from the bench disk (`vtbd0`; the root is on NVMe), device
/// interrupts to CPU 0 as for every suite, the IPv4 address from DHCP,
/// then the benchmarks between one `kbench begin` and `kbench end`.
///
/// `iperf3`'s result is the receiver's rate, turned into nanoseconds per
/// operation (1 KiB; a datagram for `udp.*.small`). `tcp.cpu.*` sends at
/// 1 Gbit/s for 20 s and charges the guest's busy CPU time
/// (`kern.cp_time`: user, nice, system, interrupt) to the data moved.
const RUN: &str = r#"#!/bin/sh
peer=$1
rm -f /tmp/ubench /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/ubench /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
sysctl vm.pmap.pti hw.ibrs_active hw.mds_disable_state
for irq in $(vmstat -ai | awk '/virtio_pci|nvme/ { sub(/^irq/, "", $1); sub(/:$/, "", $1); print $1 }'); do
	cpuset -l 0 -x "$irq"
done
for i in $(jot 120); do
	ifconfig vtnet0 | grep -q 'inet 10\.77\.0\.' && break
	sleep 1
done
export LD_LIBRARY_PATH=/tmp
rate() {
	awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Kbits/sec") v = $(i - 1) } END { print v + 0 }'
}
ip() {
	name=$1 scale=$2
	shift 2
	/tmp/iperf3 -f k "$@" | rate | awk -v n="$name" -v s="$scale" \
	    '$1 > 0 { printf "kbench %s %.3f ns/op\n", n, s / $1 }'
}
cpu() {
	name=$1
	shift
	a=$(sysctl -n kern.cp_time)
	r=$(/tmp/iperf3 -f k -t 20 -b 1000M "$@" | rate)
	b=$(sysctl -n kern.cp_time)
	hz=$(sysctl -n kern.clockrate | sed 's/.*stathz = \([0-9]*\).*/\1/')
	echo $a $b $hz $r | awk -v n="$name" '$12 > 0 {
		busy = 0
		for (i = 1; i <= 4; i++)
			busy += $(i + 5) - $i
		kib = $12 * 1000 / 8 * 20 / 1024
		printf "kbench %s %.3f ns/op\n", n, busy / $11 * 1e9 / kib
	}'
}
echo "kbench begin p7 cpus=$(sysctl -n hw.ncpu)"
/tmp/ubench p7 "$peer" | grep -v -e '^kbench begin' -e '^kbench end'
/tmp/iperf3 -s -D -B 127.0.0.1
sleep 1
ip tcp.lo.s1 8.192e9 -c 127.0.0.1 -t 10 -O 2
ip tcp.lo.s4 8.192e9 -c 127.0.0.1 -t 10 -O 2 -P 4
ip tcp.tx.s1 8.192e9 -c "$peer" -t 10 -O 2
ip tcp.tx.s4 8.192e9 -c "$peer" -t 10 -O 2 -P 4
ip tcp.rx.s1 8.192e9 -c "$peer" -t 10 -O 2 -R
ip tcp.rx.s4 8.192e9 -c "$peer" -t 10 -O 2 -R -P 4
cpu tcp.cpu.tx -c "$peer"
cpu tcp.cpu.rx -c "$peer" -R
ip udp.tx.bulk 8.192e9 -c "$peer" -t 10 -O 2 -u -b 0 -l 1448
ip udp.rx.bulk 8.192e9 -c "$peer" -t 10 -O 2 -u -b 0 -l 1448 -R
ip udp.tx.small 5.12e8 -c "$peer" -t 10 -O 2 -u -b 0 -l 64
ip udp.rx.small 5.12e8 -c "$peer" -t 10 -O 2 -u -b 0 -l 64 -R
echo "kbench end p7"
shutdown -p now
"#;

/// The bench disk's files beyond `ubench`: `iperf3` and its library, from
/// the package, which is fetched once and checked against
/// [`IPERF3_SHA256`].
pub(super) fn iperf3(dirs: &Dirs) -> Result<Vec<PathBuf>> {
    let pkg = dirs.dist.join(format!("{IPERF3}.pkg"));
    if !pkg.exists() {
        let tmp = dirs.dist.join("pkg-fetch");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp)?;
        doas(&["pkg", "fetch", "-y", "-o", &tmp.to_string_lossy(), IPERF3])?;
        let found = util::output(Command::new("find").arg(&tmp).args(["-name", "*.pkg"]))?;
        let found = found
            .lines()
            .next()
            .ok_or_else(|| format!("pkg fetch {IPERF3} left no package"))?;
        fs::copy(found, &pkg)?;
        let _ = doas(&["rm", "-rf", &tmp.to_string_lossy()]);
    }
    let sum = util::output(Command::new("sha256").arg("-q").arg(&pkg))?;
    if sum.trim() != IPERF3_SHA256 {
        return Err(format!("{}: SHA-256 mismatch", pkg.display()).into());
    }
    let dir = dirs.perf.join(IPERF3);
    let files = vec![
        dir.join("usr/local/bin/iperf3"),
        dir.join("usr/local/lib/libiperf.so.0"),
    ];
    if !files.iter().all(|f| f.exists()) {
        fs::create_dir_all(&dir)?;
        util::run(Command::new("tar").arg("-xf").arg(&pkg).arg("-C").arg(&dir))?;
    }
    Ok(files)
}

/// The far ends on the server, bound to [`PEER`] and pinned to
/// [`PEER_CPUS`]: `iperf3 -s` and `ubench p7.peer`. Stopped when dropped.
pub(super) struct Peers(Vec<Child>);

impl Peers {
    pub(super) fn start(ubench: &Path, iperf3: &[PathBuf]) -> Result<Self> {
        let lib = iperf3[1].parent().ok_or("libiperf has no directory")?;
        let mut peers = Self(Vec::new());
        peers.0.push(
            Command::new("cpuset")
                .args(["-l", PEER_CPUS])
                .arg(&iperf3[0])
                .args(["-s", "-B", PEER])
                .env("LD_LIBRARY_PATH", lib)
                .stdout(Stdio::null())
                .spawn()?,
        );
        peers.0.push(
            Command::new("cpuset")
                .args(["-l", PEER_CPUS])
                .arg(ubench)
                .args(["p7.peer", PEER])
                .spawn()?,
        );
        std::thread::sleep(Duration::from_secs(1));
        for p in &mut peers.0 {
            if let Some(status) = p.try_wait()? {
                return Err(format!("a p7 peer exited: {status}").into());
            }
        }
        Ok(peers)
    }
}

impl Drop for Peers {
    fn drop(&mut self) {
        for p in &mut self.0 {
            let _ = p.kill();
            let _ = p.wait();
        }
    }
}

/// The bench disk's script.
pub(super) fn script() -> &'static str {
    RUN
}

/// One run: boots `disk` (the installed root, on NVMe) with the bench disk
/// and a tap on the test bridge, logs in as root, and runs [`RUN`], which
/// powers the guest off.
pub(super) fn run_one(
    disk: &Path,
    bench_disk: &Path,
    log: &Path,
) -> Result<Vec<(String, f64, String)>> {
    let vm = Vm {
        smp: SMP,
        mem: "2G".into(),
        nvme: true,
        extra_disk: Some(bench_disk.to_path_buf()),
        pin: true,
        net: Some(Net::Bridge(Tap::new()?)),
        ..Vm::new(Vmm::Bhyve, Arch::X86_64, disk.to_path_buf())
    };
    let mut session = Session::start(vm, log)?;
    let started = (|| -> Result<()> {
        crate::net::login(&mut session, Duration::from_secs(600), &["panic:"])?;
        session.send(&format!(
            "mount -o ro /dev/vtbd0 /mnt && sh /mnt/run.sh {PEER}\n"
        ))
    })();
    let exited = session.finish(Duration::from_secs(1800))?;
    started?;
    let text = fs::read_to_string(log)?;
    parse(&text).map_err(|e| format!("{}: {e} (exited: {exited})", log.display()).into())
}
