#!/usr/bin/env python3
"""隔离回环 OpenSSH 与真实 congminal PTY/重连检查；不读取或修改用户 SSH 文件。"""
import argparse
import ctypes
import getpass
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time


class TaskInfo(ctypes.Structure):
    _fields_ = [(n, ctypes.c_uint64) for n in (
        "virtual", "rss", "user", "system", "threads_user", "threads_system"
    )] + [(n, ctypes.c_int32) for n in (
        "policy", "faults", "pageins", "cow", "messages_sent", "messages_received",
        "mach", "unix", "csw", "threads", "running", "priority"
    )]


def sampler():
    if sys.platform != "darwin":
        return None, "资源采样仅 macOS；其他平台仍执行传输/重连功能检查"
    lib = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    lib.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
    lib.proc_listchildpids.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
    class Timebase(ctypes.Structure):
        _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]
    base = Timebase()
    if ctypes.CDLL("/usr/lib/libSystem.B.dylib").mach_timebase_info(ctypes.byref(base)) != 0 or not base.denom:
        raise RuntimeError("mach_timebase_info 不可用")

    def tree(pid):
        out, pending = {}, [pid]
        while pending and len(out) < 64:
            current = pending.pop()
            info = TaskInfo()
            if lib.proc_pidinfo(current, 4, 0, ctypes.byref(info), ctypes.sizeof(info)) != ctypes.sizeof(info):
                continue
            out[current] = {"rss": info.rss, "cpu_ns": (info.user + info.system) * base.numer // base.denom, "threads": info.threads}
            children = (ctypes.c_int * 64)()
            lib.proc_listchildpids(current, children, ctypes.sizeof(children))
            pending.extend(p for p in children if p > 0 and p not in out)
        return out
    return tree, f"libproc 每 5 ms；Mach 时间基准 {base.numer}/{base.denom}；进程树最多 64 项"



class CutRelay:
    """仅测试基础设施：关闭自己的 TCP 套接字，模拟已建立连接断链。"""
    def __init__(self, port):
        self.upstream = ("127.0.0.1", port)
        self.closed = threading.Event()
        self.lock = threading.Lock()
        self.pairs = []
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        while self.listener.getsockname()[1] == port:
            self.listener.close()
            self.listener = socket.socket()
            self.listener.bind(("127.0.0.1", 0))
        self.port = self.listener.getsockname()[1]
        self.listener.listen(4)
        self.listener.settimeout(.1)
        self.thread = threading.Thread(target=self.accept, daemon=True)
        self.thread.start()

    def close_pair(self, pair):
        with self.lock:
            if pair in self.pairs:
                self.pairs.remove(pair)
        for connection in pair:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            connection.close()

    def pump(self, source, target, pair):
        try:
            while not self.closed.is_set():
                block = source.recv(65536)
                if not block:
                    break
                target.sendall(block)
        except OSError:
            pass
        finally:
            self.close_pair(pair)

    def accept(self):
        while not self.closed.is_set():
            try:
                client, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            try:
                upstream = socket.create_connection(self.upstream, timeout=2)
                upstream.settimeout(None)
            except OSError:
                client.close()
                continue
            pair = (client, upstream)
            with self.lock:
                self.pairs.append(pair)
            threading.Thread(target=self.pump, args=(client, upstream, pair), daemon=True).start()
            threading.Thread(target=self.pump, args=(upstream, client, pair), daemon=True).start()

    def cut(self):
        with self.lock:
            pairs = list(self.pairs)
        for pair in pairs:
            self.close_pair(pair)

    def close(self):
        self.closed.set()
        self.listener.close()
        self.cut()
        self.thread.join(timeout=1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--scenario", choices=("positive","unknown","changed","refused","auth-failure","agent","passphrase","ask","decline","jump","interrupt"), default="positive")
    parser.add_argument("--preview", type=Path, help="可选 PPM 软件渲染帧；测试末尾捕获，计入采样 RSS 峰值")
    parser.add_argument("--ssh", default=shutil.which("ssh"))
    parser.add_argument("--sshd", default=shutil.which("sshd") or "/usr/sbin/sshd")
    parser.add_argument("--ssh-keygen", default=shutil.which("ssh-keygen"))
    args = parser.parse_args()
    if os.name != "posix" or not args.ssh or not args.ssh_keygen:
        parser.error("需要 POSIX、OpenSSH 客户端、sshd 与 ssh-keygen")
    root = Path(__file__).resolve().parents[1]
    build = subprocess.run(["cargo", "test", "--offline", "--release", "--no-run", "--message-format=json"], cwd=root, capture_output=True, text=True, check=True)
    executables = [j["executable"] for line in build.stdout.splitlines() if line.startswith("{") and (j := json.loads(line)).get("executable")]
    if len(executables) != 1:
        raise RuntimeError("测试入口不唯一，未运行任何 SSH 连接")
    executable = executables[0]
    sample, method = sampler()
    result = {"method": "隔离回环服务，测试生成密钥及严格独立 known_hosts；真实 congminal Session/PTY，五次新建或重连、回显及关闭，UI harness 每 2 ms 推进；不代表 GUI 输入到呈现", "sampler": method, "preview_enabled": bool(args.preview), "scenario": args.scenario,
              "source_sha256": {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in (root / "src").rglob("*.rs")},
              "tool_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), "samples": [], "events": []}
    with tempfile.TemporaryDirectory(prefix="congminal-ssh-native-") as temporary:
        folder = Path(temporary)
        names=["host","client"]+(["other"] if args.scenario in ("changed","auth-failure") else [])
        for name in names:
            phrase="congminal-fixture-passphrase" if args.scenario=="passphrase" and name=="client" else ""
            subprocess.run([args.ssh_keygen, "-q", "-t", "ed25519", "-N", phrase, "-f", str(folder / name)], check=True)
        with socket.socket() as reserved:
            reserved.bind(("127.0.0.1", 0))
            port = reserved.getsockname()[1]
        jump_port=None
        if args.scenario=="jump":
            with socket.socket() as jump_reserved:
                jump_reserved.bind(("127.0.0.1",0));jump_port=jump_reserved.getsockname()[1]
        relay = CutRelay(port) if args.scenario=="interrupt" else None
        known_port = relay.port if relay else port
        public = (folder / ("other.pub" if args.scenario=="changed" else "host.pub")).read_text().split()
        known="" if args.scenario in ("unknown","ask","decline") else f"[127.0.0.1]:{known_port} {public[0]} {public[1]}\n"
        if jump_port:known+=f"[127.0.0.1]:{jump_port} {public[0]} {public[1]}\n"
        (folder / "known_hosts").write_text(known)
        refused=None
        if args.scenario=="refused":
            refused=socket.socket();refused.bind(("127.0.0.1",0))
        target_port=refused.getsockname()[1] if refused else relay.port if relay else port
        forced = folder / "session"
        forced.write_text("#!/bin/sh\nstty raw -echo\nprintf 'SSH_NATIVE_READY\\n'\nexec /bin/cat\n")
        forced.chmod(0o700)
        config = folder / "sshd_config"
        config.write_text(f"Port {port}\nListenAddress 127.0.0.1\nHostKey {folder}/host\nPidFile {folder}/sshd.pid\nAuthorizedKeysFile {folder}/client.pub\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nAllowUsers {getpass.getuser()}\nAllowTcpForwarding no\nX11Forwarding no\nPermitTTY yes\nForceCommand {forced}\nLogLevel ERROR\n")
        jump_config=None
        if jump_port:
            jump_config=folder/"jump_config"
            jump_config.write_text(config.read_text().replace(f"Port {port}\n",f"Port {jump_port}\n",1).replace("AllowTcpForwarding no",f"AllowTcpForwarding yes\nPermitOpen 127.0.0.1:{port}"))
        client_config = folder / "client_config"
        hosts=folder / "hosts_config"
        hosts.write_text(f"Host fixture\n HostName 127.0.0.1\n Port {target_port}\n User {getpass.getuser()}\n")
        if jump_port:
            with hosts.open("a") as more:more.write(f" ProxyJump jump\nHost jump\n HostName 127.0.0.1\n Port {jump_port}\n User {getpass.getuser()}\n IdentityFile {folder}/client\n")
        identity=folder/("other" if args.scenario=="auth-failure" else "client.pub" if args.scenario=="agent" else "client")
        strict="ask" if args.scenario in ("ask","decline") else "yes"
        batch="no" if args.scenario in ("ask","decline","passphrase") else "yes"
        identity_agent=str(folder/"agent.sock") if args.scenario=="agent" else "none"
        client_config.write_text(f"Include {hosts}\nMatch originalhost fixture\n IdentityFile {identity}\nHost *\n IdentitiesOnly yes\n IdentityAgent {identity_agent}\n StrictHostKeyChecking {strict}\n UserKnownHostsFile {folder}/known_hosts\n GlobalKnownHostsFile none\n BatchMode {batch}\n ConnectTimeout 2\n ConnectionAttempts 1\n")
        wrapper = folder / "ssh tool"
        wrapper.write_text(f"#!/bin/sh\nexport LC_ALL=C\nexec {shlex.quote(args.ssh)} -F {shlex.quote(str(client_config))} \"$@\"\n")
        wrapper.chmod(0o700)
        subprocess.run([args.sshd, "-t", "-f", str(config)], check=True)
        with (folder / "server.log").open("w") as server_log:
            server = subprocess.Popen([args.sshd, "-D", "-e", "-f", str(config)], stdout=server_log, stderr=server_log, start_new_session=True)
            process = None
            agent = None
            jump_server = None
            try:
                if jump_config:
                    subprocess.run([args.sshd,"-t","-f",str(jump_config)],check=True)
                    jump_server=subprocess.Popen([args.sshd,"-D","-e","-f",str(jump_config)],stdout=server_log,stderr=server_log,start_new_session=True)
                if args.scenario=="agent":
                    agent_program=shutil.which("ssh-agent");add_program=shutil.which("ssh-add")
                    if not agent_program or not add_program:raise RuntimeError("agent 场景需要 ssh-agent 与 ssh-add")
                    agent=subprocess.Popen([agent_program,"-D","-a",str(folder/"agent.sock")],stdout=subprocess.DEVNULL,stderr=server_log)
                    until=time.monotonic()+3
                    while not (folder/"agent.sock").exists():
                        if time.monotonic()>until or agent.poll() is not None:raise RuntimeError("隔离 agent 启动失败")
                        time.sleep(.01)
                    subprocess.run([add_program,str(folder/"client")],env=dict(os.environ,SSH_AUTH_SOCK=str(folder/"agent.sock")),stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,check=True,timeout=3)
                time.sleep(0.2)
                if server.poll() is not None:
                    raise RuntimeError("隔离 sshd 启动失败：" + (folder / "server.log").read_text())
                started = time.monotonic_ns()
                positive=args.scenario in ("positive","agent","passphrase","ask","jump")
                test="ssh_native_isolated_transport_reconnect_and_cleanup" if positive else "ssh_native_isolated_interrupt_and_manual_reconnect" if args.scenario=="interrupt" else "ssh_native_isolated_rejection_preserves_local_session"
                process = subprocess.Popen([executable, test, "--ignored", "--nocapture", "--test-threads=1"], cwd=root,
                                           env=dict(os.environ, CONGMINAL_TEST_SSH_PROGRAM=str(wrapper), CONGMINAL_TEST_SSH_SCENARIO=args.scenario, CONGMINAL_TEST_SSH_RESTART_READY=str(folder/"restarted"), **({"CONGMINAL_SSH_PREVIEW_PATH": str(args.preview.resolve())} if args.preview else {})), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
                lines = []
                def read():
                    nonlocal server
                    length = 0
                    for line in process.stdout:
                        if length < 64 * 1024:
                            lines.append(line)
                            length += len(line)
                        if "SSH_NATIVE" in line:
                            line=line[line.index("SSH_NATIVE"):]
                            result["events"].append({"elapsed_ns": time.monotonic_ns() - started, "line": line.strip()})
                            if line.startswith("SSH_NATIVE_INTERRUPT_REQUEST"):
                                relay.cut()
                            if line.startswith("SSH_NATIVE_RESTART_REQUEST"):
                                (folder/"restarted").touch()
                reader = threading.Thread(target=read)
                reader.start()
                while process.poll() is None:
                    elapsed = time.monotonic_ns() - started
                    if elapsed > 40_000_000_000:
                        process.kill()
                        raise RuntimeError("SSH 应用测试超过 40 秒")
                    if sample:
                        result["samples"].append({"elapsed_ns": elapsed, "client": sample(process.pid), "server": sample(server.pid), "agent": sample(agent.pid) if agent else {}, "jump_server": sample(jump_server.pid) if jump_server else {}})
                    time.sleep(0.005)
                reader.join(timeout=2)
                result["exit_code"] = process.returncode
                result["relay_used"] = bool(relay)
                result["log"] = "".join(lines)
                result["rounds"] = [{k: int(v) for k, v in re.findall(r"(round|ready_us|echo_us|submit_us|spawn_us|max_tick_us|used|peak)=(\d+)", e["line"])} for e in result["events"] if e["line"].startswith("SSH_NATIVE round=")]
                if sample and result["samples"]:
                    result["peak_client_tree_rss"] = max(sum(p["rss"] for p in s["client"].values()) for s in result["samples"])
                    result["max_client_processes"] = max(len(s["client"]) for s in result["samples"])
                    result["max_client_threads"] = max(sum(p["threads"] for p in s["client"].values()) for s in result["samples"])
                    seen = {int(pid) for s in result["samples"] for pid in s["client"]} - {process.pid}
                    time.sleep(0.1)
                    result["observed_client_children_still_live"] = [pid for pid in seen if sample(pid)]
                args.output.parent.mkdir(parents=True, exist_ok=True)
                args.output.write_text(json.dumps(result, indent=2) + "\n")
                if process.returncode or len(result["rounds"]) != (5 if positive else 0) or (not positive and ("SSH_NATIVE_INTERRUPTED_AND_RECOVERED" if args.scenario=="interrupt" else "SSH_NATIVE_REJECTED exit=255") not in result["log"]) or "SSH_NATIVE_CLEANUP used=0 mappings=0" not in result["log"] or result.get("observed_client_children_still_live"):
                    raise RuntimeError("SSH 应用检查未通过，详情已保存到输出 JSON")
            finally:
                if process and process.poll() is None:
                    process.kill()
                    process.wait(timeout=3)
                if relay:relay.close()
                if refused:refused.close()
                if agent:
                    agent.terminate()
                    try:agent.wait(timeout=3)
                    except subprocess.TimeoutExpired:agent.kill();agent.wait(timeout=3)
                for service in [server,jump_server]:
                    if service and service.poll() is None:
                        os.killpg(service.pid,signal.SIGTERM)
                        try:service.wait(timeout=3)
                        except subprocess.TimeoutExpired:os.killpg(service.pid,signal.SIGKILL);service.wait(timeout=3)
    print(json.dumps({k: result[k] for k in ("exit_code", "rounds", "peak_client_tree_rss", "max_client_processes", "observed_client_children_still_live") if k in result}))


if __name__ == "__main__":
    main()
