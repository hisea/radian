"""Tests for `AB_CAPTURE` in the A/B shell scripts.

A stub `harbor` records every arm it is asked to run (see
`tests/test_ab_scripts.py`); a stub `mitmdump` records its pid, port and output
file, writes the arm's CA file, and then stays alive until the script kills it.
That is enough to check the routing without Docker, Harbor or a model:

    python3 benchmarks/terminal-bench/tests/test_ab_capture_scripts.py
"""

import os
import sys
import tempfile
import unittest
from pathlib import Path

TESTS_DIR = Path(__file__).resolve().parent
BENCH_DIR = TESTS_DIR.parent
AB = BENCH_DIR / "ab"

sys.path.insert(0, str(TESTS_DIR))
sys.path.insert(0, str(BENCH_DIR))

import test_ab_scripts as base  # noqa: E402

# The stub proxy: record "<pid>\t<port>\t<AB_CAPTURE_OUT>", write the CA file,
# then stay alive until the script stops it.
STUB_MITMDUMP = """\
#!/bin/sh
confdir=
port=
while [ $# -gt 0 ]; do
  case $1 in
    --set) case $2 in confdir=*) confdir=${2#confdir=} ;; esac; shift 2 ;;
    --listen-port) port=$2; shift 2 ;;
    *) shift ;;
  esac
done
printf '%s\\t%s\\t%s\\n' "$$" "$port" "${AB_CAPTURE_OUT-}" >> "$MITM_STUB_LOG"
if [ -n "$confdir" ]; then
  mkdir -p "$confdir"
  sleep 0.1
  printf 'stub-ca\\n' > "$confdir/mitmproxy-ca-cert.pem"
fi
while :; do sleep 0.2; done
"""

# A two-task set keeps the recorded argument lists short.
TASKS = ["largest-eigenval", "write-compressor"]


def stub_host(root, with_mitmdump=True):
    """`test_ab_scripts.stub_host` plus a stub proxy on PATH."""
    home, env = base.stub_host(root)
    stub_dir = Path(root) / "stub-bin"
    if with_mitmdump:
        proxy = stub_dir / "mitmdump"
        proxy.write_text(STUB_MITMDUMP, encoding="utf-8")
        proxy.chmod(0o755)
    env["MITM_STUB_LOG"] = str(Path(root) / "mitmdump-stub.log")
    # Drop any real mitmdump from PATH so the test measures the stub only.
    dirs = [
        directory
        for directory in os.environ.get("PATH", "").split(":")
        if directory and not (Path(directory) / "mitmdump").exists()
    ]
    env["PATH"] = ":".join([str(stub_dir), *dirs])
    tasks = Path(root) / "tasks.txt"
    tasks.write_text("\n".join(TASKS) + "\n", encoding="utf-8")
    env["AB_TASKS"] = str(tasks)
    return home, env


def proxy_calls(root):
    """Every stub proxy start: pid, port and the capture file it writes."""
    log = Path(root) / "mitmdump-stub.log"
    if not log.exists():
        return []
    calls = []
    for line in log.read_text(encoding="utf-8").splitlines():
        pid, port, out = line.split("\t", 2)
        calls.append({"pid": int(pid), "port": int(port), "out": Path(out)})
    return calls


def is_alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:  # pragma: no cover - a pid we may not signal
        return True
    return True


def capture_pairs(call):
    """The `--ak` values a stub harbor call received, in order."""
    return base.flag_values(call, "--ak")


class AbShCaptureTest(unittest.TestCase):
    def run_ab_sh(self, env, cwd, *names):
        return base.run_script(AB / "ab.sh", ["caps", *names], env, cwd)

    def test_one_proxy_per_arm_on_consecutive_ports(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            home, env = stub_host(root)
            env["AB_CAPTURE"] = "1"
            rc, stderr = self.run_ab_sh(env, tmp, "radian", "radian-test")
            self.assertEqual(rc, 0, stderr)
            calls = proxy_calls(root)
            self.assertEqual([call["port"] for call in calls], [18080, 18081, 18082, 18083])
            self.assertEqual(
                [call["out"].name for call in calls],
                ["radian.jsonl", "radian-test.jsonl", "pi.jsonl", "opencode.jsonl"],
            )
            self.assertEqual(
                [call["out"].parent.name for call in calls], ["capture"] * 4
            )
            out = home / "tb2" / "jobs" / "caps"
            for name in ("radian", "radian-test", "pi", "opencode"):
                self.assertTrue(
                    (out / "capture" / f"{name}.jsonl").exists(), f"no {name}.jsonl"
                )

    def test_each_arm_gets_its_own_ak_pair(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            home, env = stub_host(root)
            env["AB_CAPTURE"] = "1"
            rc, stderr = self.run_ab_sh(env, tmp, "radian", "radian-test")
            self.assertEqual(rc, 0, stderr)
            out = home / "tb2" / "jobs" / "caps"
            arms = {call["arm"]: call for call in base.stub_invocations(root)}
            expected = {
                "radian": 18080,
                "radian-test": 18081,
                "pi": 18082,
                "opencode": 18083,
            }
            for arm, port in expected.items():
                with self.subTest(arm=arm):
                    values = capture_pairs(arms[arm])
                    self.assertIn(f"capture_port={port}", values)
                    self.assertIn(
                        f"capture_ca={out / 'capture' / arm / 'confdir' / 'mitmproxy-ca-cert.pem'}",
                        values,
                    )
                    # The arm's own options are untouched.
                    self.assertIn(
                        {
                            "radian": "thinking=high",
                            "radian-test": "thinking=high",
                            "pi": "thinking=high",
                            "opencode": "variant=high",
                        }[arm],
                        values,
                    )

    def test_no_proxy_process_survives_the_arms(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _, env = stub_host(root)
            env["AB_CAPTURE"] = "1"
            rc, stderr = self.run_ab_sh(env, tmp, "radian")
            self.assertEqual(rc, 0, stderr)
            calls = proxy_calls(root)
            self.assertTrue(calls)
            for call in calls:
                self.assertFalse(
                    is_alive(call["pid"]), f"stub mitmdump {call['pid']} still runs"
                )

    def test_ab_capture_port_moves_the_first_port(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _, env = stub_host(root)
            env["AB_CAPTURE"] = "1"
            env["AB_CAPTURE_PORT"] = "18200"
            rc, stderr = self.run_ab_sh(env, tmp, "radian")
            self.assertEqual(rc, 0, stderr)
            self.assertEqual(
                [call["port"] for call in proxy_calls(root)], [18200, 18201, 18202]
            )


class WithoutCaptureTest(unittest.TestCase):
    """Without AB_CAPTURE the scripts run exactly the commands they ran before."""

    def expected_args(self, home, tasks, arm):
        common = [
            "run",
            "-d",
            "terminal-bench@2.0",
            "-m",
            "opencode-go/deepseek-v4.1-flash",
            "-n",
            "2",
            "-k",
            "2",
            "-y",
        ]
        for task in tasks:
            common += ["-i", task]
        binary = str(home / "tb2" / "bin" / "radian")
        digest = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
        tail = {
            "radian": [
                "-a",
                "radian_agent:RadianAgent",
                "--ak",
                f"binary_path={binary}",
                "--ak",
                f"binary_sha256={digest}",
                "--ak",
                "thinking=high",
            ],
            "pi": [
                "-a",
                "pi_opencode_go:PiOpencodeGo",
                "--ak",
                "version=1.4.0",
                "--ak",
                "thinking=high",
            ],
            "opencode": [
                "-a",
                "opencode_go:OpenCodeGo",
                "--ak",
                "version=1.19.4",
                "--ak",
                "variant=high",
                "--ak",
                "disable_web_search=true",
            ],
        }[arm]
        return common + tail

    def test_no_proxy_starts_and_the_arguments_are_todays(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            home, env = stub_host(root)
            rc, stderr = base.run_script(AB / "ab.sh", ["plain", "radian"], env, tmp)
            self.assertEqual(rc, 0, stderr)
            self.assertEqual(proxy_calls(root), [])
            calls = {call["arm"]: call for call in base.stub_invocations(root)}
            for arm in ("radian", "pi", "opencode"):
                with self.subTest(arm=arm):
                    self.assertEqual(
                        calls[arm]["args"], self.expected_args(home, TASKS, arm)
                    )

    def test_capture_only_adds_the_two_ak_values(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _, env = stub_host(root)
            rc, stderr = base.run_script(AB / "ab.sh", ["plain", "radian"], env, tmp)
            self.assertEqual(rc, 0, stderr)
            plain = {
                call["arm"]: call["args"] for call in base.stub_invocations(root)
            }

            (root / "harbor-stub.log").unlink()
            env["AB_CAPTURE"] = "1"
            rc, stderr = base.run_script(
                AB / "ab.sh", ["captured", "radian"], env, tmp
            )
            self.assertEqual(rc, 0, stderr)
            captured = {
                call["arm"]: call["args"] for call in base.stub_invocations(root)
            }
            for arm in ("radian", "pi", "opencode"):
                with self.subTest(arm=arm):
                    args = list(captured[arm])
                    start = next(
                        index
                        for index, value in enumerate(args)
                        if value == "--ak"
                        and args[index + 1].startswith("capture_port=")
                    )
                    # Drop the capture pair: `--ak capture_port` and
                    # `--ak capture_ca`.
                    del args[start : start + 4]
                    self.assertEqual(args, plain[arm])


class OtherScriptsCaptureTest(unittest.TestCase):
    def test_ab_system_starts_one_proxy_per_arm(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            home, env = stub_host(root)
            env["AB_CAPTURE"] = "1"
            role = root / "role.md"
            role.write_text("You are terse.\n", encoding="utf-8")
            rc, stderr = base.run_script(
                AB / "ab-system.sh", ["capsys", "radian-test", str(role)], env, tmp
            )
            self.assertEqual(rc, 0, stderr)
            self.assertEqual([call["port"] for call in proxy_calls(root)], [18080, 18081])
            out = home / "tb2" / "jobs" / "capsys"
            calls = {call["arm"]: call for call in base.stub_invocations(root)}
            self.assertIn(
                f"capture_ca={out / 'capture' / 'base' / 'confdir' / 'mitmproxy-ca-cert.pem'}",
                capture_pairs(calls["base"]),
            )
            self.assertIn(
                f"capture_ca={out / 'capture' / 'concise' / 'confdir' / 'mitmproxy-ca-cert.pem'}",
                capture_pairs(calls["concise"]),
            )

    def test_ab_body_starts_one_proxy_per_arm(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            home, env = stub_host(root)
            env["AB_CAPTURE"] = "1"
            body = root / "body.json"
            body.write_text('{"thinking": {"type": "enabled"}}\n', encoding="utf-8")
            rc, stderr = base.run_script(
                AB / "ab-body.sh",
                ["capsbody", "radian-test", str(body)],
                env,
                tmp,
            )
            self.assertEqual(rc, 0, stderr)
            self.assertEqual([call["port"] for call in proxy_calls(root)], [18080, 18081])
            out = home / "tb2" / "jobs" / "capsbody"
            calls = {call["arm"]: call for call in base.stub_invocations(root)}
            for arm in ("base", "body"):
                self.assertIn(
                    f"capture_ca={out / 'capture' / arm / 'confdir' / 'mitmproxy-ca-cert.pem'}",
                    capture_pairs(calls[arm]),
                )

    def test_smoke_starts_one_proxy(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            home, env = stub_host(root)
            env["AB_CAPTURE"] = "1"
            env["AB_CAPTURE_PORT"] = "18300"
            rc, stderr = base.run_script(
                AB / "smoke.sh", ["radian", "capsmoke"], env, tmp
            )
            self.assertEqual(rc, 0, stderr)
            calls = proxy_calls(root)
            self.assertEqual([call["port"] for call in calls], [18300])
            self.assertEqual(calls[0]["out"].name, "radian.jsonl")
            out = home / "tb2" / "jobs" / "capsmoke"
            harbor = base.stub_invocations(root)
            self.assertEqual(len(harbor), 1)
            self.assertIn(
                f"capture_ca={out / 'capture' / 'radian' / 'confdir' / 'mitmproxy-ca-cert.pem'}",
                capture_pairs(harbor[0]),
            )
            self.assertEqual(
                (out / "done").read_text(encoding="utf-8"), "rc=0\n"
            )


class MissingMitmdumpTest(unittest.TestCase):
    def test_the_script_refuses_with_one_line_naming_mitmdump(self):
        for script, args in (
            ("ab.sh", ["caps", "radian"]),
            ("ab-system.sh", None),
            ("smoke.sh", ["radian"]),
        ):
            with self.subTest(script=script):
                with tempfile.TemporaryDirectory() as tmp:
                    root = Path(tmp)
                    _, env = stub_host(root, with_mitmdump=False)
                    env["AB_CAPTURE"] = "1"
                    if args is None:
                        role = root / "role.md"
                        role.write_text("You are terse.\n", encoding="utf-8")
                        args = ["caps", "radian-test", str(role)]
                    rc, stderr = base.run_script(AB / script, args, env, tmp)
                    self.assertNotEqual(rc, 0)
                    self.assertEqual(len(stderr.splitlines()), 1, stderr)
                    self.assertIn("mitmdump", stderr)
                    self.assertEqual(base.stub_invocations(root), [])
                    self.assertEqual(proxy_calls(root), [])


if __name__ == "__main__":
    unittest.main()
