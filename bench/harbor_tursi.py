"""Harbor agent adapter for tursi: runs tursi headless inside each task's
container and keeps the DEALS router learning across trials.

    PYTHONPATH=bench harbor run -d <org>/<dataset> \
        --agent-import-path harbor_tursi:Tursi -e docker -o bench/jobs/harbor

What goes into the container (uploaded under /installed-agent, nothing is
installed from the network):
  * the static musl tursi and ripgrep from target-musl/ (any base image works),
  * the host's CA bundle, since minimal images often have none,
  * a copy of ~/.tursi: config.toml, the Cloudflare part of secrets.toml, and
    deals/ (outcomes.jsonl, stations.json), so routing starts from everything
    the host has learned so far.

After the run the trial's new outcomes are appended to the host's
outcomes.jsonl under a project id unique to the trial (every container calls
its project /app or /workspace). The verifier runs after the agent, so its
reward reaches the router as a `truth` record one step later: each trial
sweeps the job's finished trials before it starts, and `python3
bench/harbor_tursi.py <jobs_dir>` does a last sweep after the job.

TURSI_ARGS (e.g. "--pool cloudflare/@cf/zai-org/glm-5.3-flash" or
"--explore-min 3") is passed through to tursi, as in bench-tursi.sh.
TURSI_WORKDIR overrides the project directory (default: the image's WORKDIR,
or the first of /app /workspace /testbed when that is /).
"""
from __future__ import annotations

import fcntl
import json
import os
import shlex
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TURSI_BINARY = REPO / "target-musl/release/tursi"
RG_BINARY = REPO / "target-musl/rg/bin/rg"
CA_BUNDLE = Path("/etc/ssl/certs/ca-certificates.crt")
HOST_TURSI = Path.home() / ".tursi"
# TURSI_OUTCOMES points the merge at another file, for dry runs.
OUTCOMES = Path(os.environ.get("TURSI_OUTCOMES") or HOST_TURSI / "deals/outcomes.jsonl")
# Only the Cloudflare credentials go into task containers.
SECRET_KEYS = ("CLOUDFLARE_API_KEY", "CF_ACCOUNT_ID")
STAGE = "/installed-agent"
HOME = "/tmp/tursi-home"
PROJECT_MARK = "tursi-project.txt"


def _secrets_for_container() -> str:
    """secrets.toml with every top-level key dropped except SECRET_KEYS."""
    out, section = [], None
    for line in (HOST_TURSI / "secrets.toml").read_text().splitlines():
        s = line.strip()
        if s.startswith("["):
            section = s
        key = s.split("=", 1)[0].strip() if "=" in s else None
        if section is None and key and key not in SECRET_KEYS:
            continue
        out.append(line)
    return "\n".join(out) + "\n"


def _append_records(records: list[dict]) -> None:
    if not records:
        return
    with open(OUTCOMES, "a", encoding="utf-8") as f:
        fcntl.flock(f, fcntl.LOCK_EX)
        for r in records:
            f.write(json.dumps(r) + "\n")
        fcntl.flock(f, fcntl.LOCK_UN)


def _reward(result: dict) -> float | None:
    rewards = (result.get("verifier_result") or {}).get("rewards") or {}
    if "reward" in rewards:
        return float(rewards["reward"])
    if len(rewards) == 1:
        return float(next(iter(rewards.values())))
    return None


def feedback(jobs_dir: Path) -> int:
    """Append a truth record for every finished trial under jobs_dir that
    has none yet. Idempotent; returns how many it added."""
    known = set()
    if OUTCOMES.exists():
        for line in OUTCOMES.read_text(encoding="utf-8").splitlines():
            if '"truth"' in line:
                try:
                    known.add(json.loads(line).get("project"))
                except json.JSONDecodeError:
                    pass
    records = []
    for mark in jobs_dir.rglob(f"agent/{PROJECT_MARK}"):
        trial = mark.parent.parent
        result_file = trial / "result.json"
        project = mark.read_text().strip()
        if project in known or not result_file.exists():
            continue
        try:
            reward = _reward(json.loads(result_file.read_text()))
        except (json.JSONDecodeError, ValueError, TypeError):
            continue
        if reward is None:
            continue
        records.append({
            "ts": datetime.now(timezone.utc).isoformat(),
            "project": project,
            "truth": max(0.0, min(1.0, reward)),
            "source": f"harbor:{trial.parent.name}/{trial.name}",
        })
        known.add(project)
    _append_records(records)
    return len(records)


if __name__ == "__main__":
    for d in sys.argv[1:] or [str(REPO / "bench/jobs/harbor")]:
        print(f"{d}: {feedback(Path(d))} truth records added")
    sys.exit(0)


from harbor.agents.installed.base import BaseInstalledAgent  # noqa: E402
from harbor.environments.base import BaseEnvironment  # noqa: E402
from harbor.models.agent.context import AgentContext  # noqa: E402


class Tursi(BaseInstalledAgent):
    @staticmethod
    def name() -> str:
        return "tursi"

    def get_version_command(self) -> str | None:
        return f"{STAGE}/bin/tursi --version 2>/dev/null || echo tursi"

    @property
    def trial_dir(self) -> Path:
        return Path(self.logs_dir).parent

    @property
    def project_id(self) -> str:
        return f"harbor:{self.trial_dir.parent.name}/{self.trial_dir.name}"

    async def install(self, environment: BaseEnvironment) -> None:
        # Truth from trials that finished while this one was queued.
        try:
            feedback(self.trial_dir.parent)
        except Exception:
            pass
        await self.exec_as_root(environment, command=f"mkdir -p {STAGE}/bin {STAGE}/state/deals")
        await environment.upload_file(TURSI_BINARY, f"{STAGE}/bin/tursi")
        await environment.upload_file(RG_BINARY, f"{STAGE}/bin/rg")
        await environment.upload_file(CA_BUNDLE, f"{STAGE}/ca.pem")
        with tempfile.TemporaryDirectory() as tmp:
            secrets = Path(tmp) / "secrets.toml"
            secrets.write_text(_secrets_for_container())
            await environment.upload_file(secrets, f"{STAGE}/state/secrets.toml")
            snapshot = Path(tmp) / "outcomes.jsonl"
            data = OUTCOMES.read_bytes() if OUTCOMES.exists() else b""
            snapshot.write_bytes(data)
            self._snapshot_lines = data.count(b"\n")
            await environment.upload_file(snapshot, f"{STAGE}/state/deals/outcomes.jsonl")
        await environment.upload_file(HOST_TURSI / "config.toml", f"{STAGE}/state/config.toml")
        stations = HOST_TURSI / "deals/stations.json"
        if stations.exists():
            await environment.upload_file(stations, f"{STAGE}/state/deals/stations.json")
        await self.exec_as_root(
            environment,
            command=f"chmod 0755 {STAGE}/bin/tursi {STAGE}/bin/rg && chmod -R a+rX {STAGE}",
        )
        # The agent user owns its HOME; secrets must be 0600 or tursi refuses them.
        await self.exec_as_agent(
            environment,
            command=(
                f"mkdir -p {HOME}/.tursi /logs/agent && cp -r {STAGE}/state/. {HOME}/.tursi/ && "
                f"chmod 600 {HOME}/.tursi/secrets.toml {HOME}/.tursi/config.toml"
            ),
        )
        await self.exec_as_root(environment, command=f"rm -f {STAGE}/state/secrets.toml")

    def populate_context_post_run(self, context: AgentContext) -> None:
        out = Path(self.logs_dir) / "tursi.txt"
        if not out.exists():
            return
        for line in reversed(out.read_text(errors="replace").splitlines()):
            line = line.strip()
            if line.startswith("{") and "cost_usd" in line:
                try:
                    row = json.loads(line)
                except json.JSONDecodeError:
                    continue
                context.cost_usd = row.get("cost_usd")
                context.metadata = {"tursi": row}
                return

    async def run(self, instruction: str, environment: BaseEnvironment, context: AgentContext) -> None:
        Path(self.logs_dir).mkdir(parents=True, exist_ok=True)
        (Path(self.logs_dir) / PROJECT_MARK).write_text(self.project_id + "\n")
        extra = os.environ.get("TURSI_ARGS", "")
        workdir = os.environ.get("TURSI_WORKDIR", "")
        pick = (
            f"cd {shlex.quote(workdir)}" if workdir else
            'if [ "$(pwd)" = / ]; then for d in /app /workspace /testbed; do [ -d $d ] && cd $d && break; done; fi'
        )
        env = {
            "HOME": HOME,
            "PATH": f"{STAGE}/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            "SSL_CERT_FILE": f"{STAGE}/ca.pem",
        }
        inst = shlex.quote(instruction)
        # Always exit 0: the verifier, not tursi's exit code, decides.
        await self.exec_as_agent(
            environment,
            command=(
                f"{pick}; pwd > /logs/agent/workdir.txt; "
                f"(tursi --task {inst} --no-sandbox --json {extra} . 2>&1 | tee /logs/agent/tursi.txt); "
                # tursi's project state is ours, not the grader's.
                'mv .tursi /logs/agent/project-tursi 2>/dev/null; '
                f"cp {HOME}/.tursi/deals/outcomes.jsonl /logs/agent/outcomes.jsonl; true"
            ),
            env=env,
        )
        self._merge_outcomes()

    def _merge_outcomes(self) -> None:
        path = Path(self.logs_dir) / "outcomes.jsonl"
        if not path.exists():
            return
        lines = path.read_text(encoding="utf-8").splitlines()[getattr(self, "_snapshot_lines", 0):]
        records = []
        for line in lines:
            try:
                r = json.loads(line)
            except json.JSONDecodeError:
                continue
            if "truth" in r:
                continue
            r["project"] = self.project_id
            records.append(r)
        _append_records(records)
