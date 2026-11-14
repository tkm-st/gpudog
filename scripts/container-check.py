#!/usr/bin/env python3
"""Cross-container regressions: default CPU driver doubles, --gpu for real GPUs.

Only containers created by this invocation are removed. Logs remain in tmp/.
The production images never contain the mock driver libraries.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import tempfile
import time


ROOT = Path(__file__).resolve().parent.parent


class Checks:
    daemon_image = "gpudogd:container-check"
    client_image = "gpudog-client:container-check"
    driver_image = "gpudog-drivers:container-check"

    def __init__(self, gpu, release=False, binaries=None):
        self.gpu = gpu
        self.release = release
        self.binaries = binaries
        if release:
            self.daemon_image = self.client_image = "gpudog:release-check"
            self.driver_image = "gpudog-drivers:release-check"
        (ROOT / "tmp").mkdir(exist_ok=True)
        self.root = Path(tempfile.mkdtemp(prefix="containers-", dir=ROOT / "tmp"))
        self.root.chmod(0o755)
        self.containers = []
        self.uid = os.getuid()
        self.gid = os.getgid()
        self.other_uid = 42421 if self.uid != 42421 else 42422
        options = self.docker("info", "--format", "{{json .SecurityOptions}}").stdout
        if "rootless" in options:
            raise RuntimeError("container checks require a rootful Docker engine")
        if gpu:
            output = subprocess.run(
                ["nvidia-smi", "--query-gpu=uuid,pci.bus_id", "--format=csv,noheader"],
                capture_output=True, text=True, check=True,
            ).stdout
            rows = [row.split(",", 1) for row in output.splitlines()]
            self.uuids = [row[0].strip() for row in sorted(rows, key=lambda row: row[1])]
            if len(self.uuids) < 2:
                raise RuntimeError("--gpu requires two non-MIG GPUs and NVIDIA Container Toolkit")
        else:
            self.uuids = [f"GPU-00000000-0000-0000-0000-{i:012x}" for i in range(4)]

    def docker(self, *args, check=True, timeout=30):
        result = subprocess.run(["docker", *args], cwd=ROOT, capture_output=True,
                                text=True, timeout=timeout)
        if check and result.returncode:
            raise RuntimeError(f"docker {' '.join(args)}\n{result.stdout}\n{result.stderr}")
        return result

    def build(self):
        if self.release:
            targets = [("minimum-runtime", self.daemon_image)]
            if not self.gpu:
                targets.append(("minimum-test-drivers", self.driver_image))
            for target, image in targets:
                self.docker("build", "--platform", "linux/amd64", "-f", "Dockerfile.release", "--target", target, "-t", image, ".", timeout=1800)
                print(f"built {image}", flush=True)
            return
        targets = [("runtime", self.daemon_image), ("client", self.client_image)]
        if not self.gpu:
            targets.append(("test-drivers", self.driver_image))
        for target, image in targets:
            self.docker("build", "--target", target, "-t", image, ".", timeout=600)
            print(f"built {image}", flush=True)

    def prepare_drivers(self):
        if not self.gpu:
            extraction = self.create("--entrypoint", "/bin/true", self.driver_image)
            (self.root / "drivers").mkdir()
            self.docker("cp", f"{extraction}:/test-drivers/.", str(self.root / "drivers"))

    def create(self, *args):
        cid = self.docker("create", *args).stdout.strip()
        self.containers.append(cid)
        return cid

    def common(self, gpu_devices="all"):
        args = ["--userns", "host", "--network", "none",
                "--env", "CUDA_DEVICE_ORDER=PCI_BUS_ID"]
        if self.binaries:
            for binary in ("gpudog", "gpudogd"):
                args += ["--mount", f"type=bind,src={self.binaries / binary},dst=/usr/bin/{binary},readonly"]
        if self.gpu:
            args += ["--gpus", gpu_devices]
        else:
            args += ["--mount", f"type=bind,src={self.root / 'drivers'},dst=/test-drivers,readonly",
                     "--env", "LD_LIBRARY_PATH=/test-drivers",
                     "--env", "NVIDIA_VISIBLE_DEVICES=void"]
        return args

    def start_daemon(self):
        settings = f'devices = ["{self.uuids[0]}", "{self.uuids[1]}"]\nmax_held_per_uid = 1\nwatch_unmanaged_ms = 100\n'
        # Config and socket directory are created as the daemon's root user.
        # Only the socket directory is mounted into client containers.
        setup = ("mkdir -p /checks/run\nchmod 0755 /checks/run\n"
                 f"printf '%s' {shlex.quote(settings)} > /checks/config.toml\n"
                 "chmod 0600 /checks/config.toml\n"
                 "exec /usr/bin/gpudogd -c /checks/config.toml -s /checks/run/g.sock")
        extra = [] if self.gpu else ["--env", "MOCK_GPU_PROCESS_FILE=/checks/busy.txt",
                                    "--env", "MOCK_GPU_OBSERVATION_FAILURE_FILE=/checks/failure"]
        cid = self.create(*self.common(), "--pid", "host", *extra, "--mount", f"type=bind,src={self.root},dst=/checks",
                          "--entrypoint", "/bin/sh", self.daemon_image, "-c", setup)
        self.docker("start", cid)
        self.wait_log(cid, "listening on")
        return cid

    def client(self, args, uid=None, visible=None, gpu_devices="all"):
        settings = self.common(gpu_devices) + ["--entrypoint", "/usr/bin/gpudog", "--user", f"{self.uid if uid is None else uid}:{self.gid}",
            "--mount", f"type=bind,src={self.root / 'run'},dst=/run/gpudog,readonly"]
        if visible is not None:
            settings += ["--env", f"CUDA_VISIBLE_DEVICES={visible}"]
        return self.create(*settings, self.client_image, *args)

    def exit_code(self, cid):
        return int(self.docker("inspect", "--format", "{{.State.ExitCode}}", cid).stdout.strip())

    def run(self, args, uid=None, visible=None, gpu_devices="all"):
        cid = self.client(args, uid=uid, visible=visible, gpu_devices=gpu_devices)
        result = self.docker("start", "--attach", cid, check=False)
        state = json.loads(self.docker("inspect", "--format", "{{json .State}}", cid).stdout)
        if state["Status"] != "exited" or state.get("Error"):
            raise RuntimeError(f"client container did not execute: {state}\n{result.stderr}")
        return self.exit_code(cid), result.stdout, result.stderr

    def wait_log(self, cid, fragment, timeout=15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            output = self.docker("logs", cid)
            if fragment in output.stdout + output.stderr:
                return
            running = self.docker("inspect", "--format", "{{.State.Running}}", cid).stdout.strip()
            if running != "true":
                raise RuntimeError(output.stdout + output.stderr)
            time.sleep(0.05)
        raise RuntimeError(f"container never reported {fragment}: {cid}")

    def snapshot(self, hidden=True, fragment=None, timeout=15):
        args = ["dash", "-s", "/run/gpudog/g.sock", "-r", "1"]
        if hidden:
            args.append("--no-gpu-processes")
        cid = self.client(args)
        self.docker("start", cid)
        # Keep polling one dashboard until the requested fresh frame appears;
        # repeatedly sampling only its first frame can synchronize with scans.
        self.wait_log(cid, fragment or ("GPU process rows hidden" if hidden else "GPU processes"), timeout=timeout)
        output = self.docker("logs", cid)
        self.docker("stop", "--time", "2", cid)
        return output.stdout + output.stderr

    def wait_snapshot(self, fragment, timeout=15):
        return self.snapshot(fragment=fragment, timeout=timeout)

    def job(self, options, uid=None, visible=None, command=None, gpu_devices="all"):
        args = ["run", "-s", "/run/gpudog/g.sock", *options, "--"]
        args += command or ["sh", "-c", 'printf "%s" "$CUDA_VISIBLE_DEVICES"']
        return self.run(args, uid=uid, visible=visible, gpu_devices=gpu_devices)

    def check(self):
        self.prepare_drivers()
        self.start_daemon()
        self.wait_snapshot("Observation:")
        if self.gpu:
            # A query in flight can briefly show unknown free on the minimum
            # runtime; only a fresh sample can establish preflight availability.
            try:
                self.wait_snapshot("2 free, 0 waiting")
            except RuntimeError as error:
                raise RuntimeError("no fresh observation of two free GPUs; allocator checks need two free GPUs and a healthy observer. Inspect its logs without stopping external jobs.") from error
        code, out, err = self.run(["health", "-s", "/run/gpudog/g.sock"])
        assert code == 0, (out, err)
        for options, visible, expected in [(["-g", "0"], "1,0", self.uuids[1]),
                                            (["-g", "1"], "1,0", self.uuids[0]),
                                            (["-n", "1"], "1", self.uuids[1])]:
            code, out, err = self.job(options, visible=visible)
            assert code == 0 and out == expected, (code, out, err)
        code, out, err = self.job(["-g", self.uuids[0]], visible="1")
        assert code == 1 and not out and "not visible" in err, (code, out, err)
        for visible in ["", "-1"]:
            code, out, err = self.job([], visible=visible)
            assert code == 1 and not out and "no GPUs are visible" in err, (code, out, err)
        code, out, err = self.job([], command=["sh", "-c", "exit 7"])
        assert code == 7, (code, out, err)
        print("ok cross-container health, CUDA ordering/visibility, UUID rejection, exit status", flush=True)
        if self.gpu:
            devices = f"device={self.uuids[1]}"
            code, out, err = self.job(["-g", "0"], gpu_devices=devices)
            assert code == 0 and out == self.uuids[1], (code, out, err)
            code, out, err = self.job(["-g", "1"], gpu_devices=devices)
            assert code == 1 and not out and "not visible" in err, (code, out, err)
            print("ok client-container Docker GPU subset has its own local CUDA index", flush=True)

        holder = self.client(["run", "-s", "/run/gpudog/g.sock", "-g", "0", "--", "sleep", "30"])
        self.docker("start", holder)
        self.wait_log(holder, "holding gpu(s)")
        code, out, err = self.job(["-g", "1"])
        assert code == 1 and "per-user cap" in err, (code, out, err)
        code, out, err = self.job(["-g", "1"], uid=self.other_uid)
        assert code == 0 and out == self.uuids[1], (code, out, err)
        view = self.wait_snapshot("1 free, 0 waiting")
        assert re.search(rf"^\s+0\s+{self.uid}\s+", view, re.MULTILINE), view
        assert "1 free, 0 waiting" in view, view
        print("ok kernel UID identity, same-UID cap across containers, independent UID", flush=True)

        def waiter(acquire_timeout):
            cid = self.client(["run", "-s", "/run/gpudog/g.sock", "-g", "0", "-t", acquire_timeout,
                               "--", "echo", "resumed"], uid=self.other_uid)
            self.docker("start", cid)
            self.wait_log(cid, "waiting for GPUs")
            return cid

        abandoned = waiter("5m")
        self.docker("kill", abandoned)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            view = self.snapshot()
            if time.monotonic() <= deadline and "1 free, 0 waiting" in view:
                break
        else:
            raise RuntimeError("killed queued container was not removed within 10 seconds")
        resumed = waiter("10s")
        self.docker("kill", holder)
        self.wait_log(resumed, "resumed")
        self.docker("wait", resumed)
        assert self.exit_code(resumed) == 0
        code, out, err = self.job(["-g", "0"])
        assert code == 0 and out == self.uuids[0], (code, out, err)
        print("ok queued-container cancellation, holder death, wakeup and reservation release", flush=True)
        if not self.gpu:
            self.check_unmanaged()

    def check_unmanaged(self):
        busy = self.root / "busy.txt"
        failure = self.root / "failure"
        busy.write_text("0 999999\n")
        self.wait_snapshot("1 free, 0 waiting")
        code, out, err = self.job(["-g", "0", "-t", "150ms"], command=["echo", "must-not-run"])
        assert code == 1 and not out and "timed out" in err, (code, out, err)
        busy.write_text("")
        self.wait_snapshot("2 free, 0 waiting")
        holder = self.client(["run", "-s", "/run/gpudog/g.sock", "-g", "0", "--", "sleep", "30"])
        self.docker("start", holder)
        self.wait_log(holder, "holding gpu(s)")
        pid = int(self.docker("inspect", "--format", "{{.State.Pid}}", holder).stdout.strip())
        children = Path(f"/proc/{pid}/task/{pid}/children").read_text().split()
        assert children, "holder did not launch its job"
        child = int(children[0])
        busy.write_text(f"0 {child}\n")
        # Default dash displays daemon-observed process identity even though
        # the client has an independent PID namespace.
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            text = self.snapshot(hidden=False)
            lines = [line.split() for line in text.splitlines()]
            if any(len(fields) >= 6 and fields[:3] == ["0", str(child), str(self.uid)]
                   and "managed" in fields and "not" not in fields and "(wrong" not in fields
                   for fields in lines):
                break
        else:
            raise RuntimeError("daemon did not associate the host child identity with its reservation")
        failure.touch()
        self.wait_snapshot("GPU observation failed")
        code, out, err = self.job(["-g", "1", "-t", "150ms"], uid=self.other_uid, command=["echo", "must-not-run"])
        assert code == 1 and not out and "timed out" in err, (code, out, err)
        assert self.docker("inspect", "--format", "{{.State.Running}}", holder).stdout.strip() == "true"
        failure.unlink()
        self.wait_snapshot("1 free, 0 waiting")
        code, out, err = self.job(["-g", "1"], uid=self.other_uid)
        assert code == 0 and out == self.uuids[1], (code, out, err)
        self.docker("kill", holder)
        busy.write_text("")
        self.wait_snapshot("2 free, 0 waiting")
        print("ok unmanaged usage wait, host child identity, failure pause, existing-job survival, recovery", flush=True)

    def close(self):
        failures = []
        for cid in reversed(self.containers):
            try:
                output = self.docker("logs", cid, check=False)
                (self.root / f"{cid[:12]}.log").write_text(output.stdout + output.stderr)
                if output.returncode:
                    failures.append(f"logs {cid}: {output.stderr}")
            except Exception as error:
                failures.append(f"logs {cid}: {error}")
            finally:
                try:
                    removed = self.docker("rm", "--force", cid, check=False)
                    if removed.returncode:
                        failures.append(f"remove {cid}: {removed.stderr}")
                except Exception as error:
                    failures.append(f"remove {cid}: {error}")
        print(f"logs: {self.root}", flush=True)
        if failures:
            raise RuntimeError("could not clean up test containers: " + "; ".join(failures))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gpu", action="store_true", help="use two real GPUs instead of driver doubles")
    parser.add_argument("--skip-build", action="store_true", help="reuse images built by a previous run")
    parser.add_argument("--release", action="store_true", help="use the glibc 2.28 release test images (build/test only, never deploy)")
    parser.add_argument("--binaries", type=Path, help="test exported binaries by mounting this directory read-only")
    args = parser.parse_args()
    if not __debug__:
        parser.error("container checks cannot run with Python optimization")
    binaries = args.binaries.resolve() if args.binaries else None
    if binaries and not all((binaries / binary).is_file() for binary in ("gpudog", "gpudogd")):
        parser.error("--binaries must contain gpudog and gpudogd")
    checks = Checks(args.gpu, args.release, binaries)
    try:
        if not args.skip_build:
            checks.build()
        checks.check()
    finally:
        checks.close()


if __name__ == "__main__":
    main()
