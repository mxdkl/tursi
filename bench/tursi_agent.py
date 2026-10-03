"""Pier agent adapter for tursi — runs tursi headless inside a DeepSWE task
container. Point Pier at it with:

    PYTHONPATH=bench pier run -p ~/deep-swe/tasks/<task> \
        --agent-import-path tursi_agent:TursiAgent \
        --model deepseek/deepseek-flash --env docker -o bench/jobs

tursi needs no source changes for this: it self-configures on first run
(seeds ~/.tursi/config.toml) and resolves the key from DEEPSEEK_API_KEY. It
runs with --no-sandbox — the task container is the isolation, and nested user
namespaces are usually unavailable inside it (PERMISSIONS.md §6).
"""
import shlex

from pier.agents.installed.base import BaseInstalledAgent
from pier.models.agent.install import AgentInstallSpec, InstallStep
from pier.models.agent.network import NetworkAllowlist

# glibc-matched binary (built in rust:bookworm to match the Debian-12 images).
TURSI_BINARY = "/home/player1/tursi/harness/target-bookworm/release/tursi"
REPO_DIR = "/app"
MAX_TURNS = 100


class TursiAgent(BaseInstalledAgent):
    @staticmethod
    def name() -> str:
        return "tursi"

    def network_allowlist(self) -> NetworkAllowlist:
        # tursi's only outbound need is the model API.
        return NetworkAllowlist(domains=["api.deepseek.com"])

    def get_version_command(self) -> str | None:
        return "/usr/local/bin/tursi --help >/dev/null 2>&1 && echo tursi"

    def install_spec(self) -> AgentInstallSpec:
        # The binary is uploaded in run(); here we ensure the tools tursi needs
        # exist in the (ephemeral, per-task) container: git (repo detection +
        # the diff-based grader) and ripgrep (tursi's `search` tool — without
        # it, search hard-errors and code navigation collapses).
        #
        # Language servers back tursi's post-edit diagnostics (§3.1): they catch
        # type/import errors — an undefined symbol, a type-only import used as a
        # value — the moment they're written, which pure execution only reveals
        # if the agent bothers to run the code. Each is best-effort per the
        # task's toolchain: no npm/rustup/go → that server is simply skipped, and
        # tursi degrades to no diagnostics for that language (never an error).
        # Installed in the adapter, not fallback-baked into tursi.
        lsp_servers = (
            "(command -v npm >/dev/null 2>&1 && "
            "npm install -g --silent typescript typescript-language-server pyright) || true ; "
            "(command -v rustup >/dev/null 2>&1 && "
            "rustup component add rust-analyzer) || true ; "
            "(command -v go >/dev/null 2>&1 && "
            "GOBIN=/usr/local/bin go install golang.org/x/tools/gopls@latest) || true"
        )
        return AgentInstallSpec(
            agent_name="tursi",
            steps=[
                InstallStep(
                    run="apt-get update -qq && apt-get install -y -qq git ripgrep",
                    user="root",
                ),
                InstallStep(run=lsp_servers, user="root"),
            ],
        )

    def populate_context_post_run(self, context) -> None:
        return None

    async def run(self, instruction, environment, context) -> None:
        # 1. Get the glibc-matched tursi binary into the container.
        await environment.upload_file(TURSI_BINARY, "/usr/local/bin/tursi")
        await self.exec_as_root(environment, command="chmod 0755 /usr/local/bin/tursi")

        # 2. Env: the model key + a writable HOME for first-run config seeding.
        env = self.build_process_env()
        if key := self._get_env("DEEPSEEK_API_KEY"):
            env["DEEPSEEK_API_KEY"] = key
        env["HOME"] = "/tmp/tursi-home"

        # 3. Run tursi headless in the repo. Always exit 0 — the hidden tests,
        #    not tursi's exit code, decide correctness.
        inst = shlex.quote(instruction)
        await self.exec_as_agent(
            environment,
            command=(
                f"mkdir -p /tmp/tursi-home /logs/agent && cd {REPO_DIR} && "
                f"(/usr/local/bin/tursi --task {inst} --json --max-turns {MAX_TURNS} --no-sandbox . "
                f"2>&1 | tee /logs/agent/tursi.txt) ; true"
            ),
            env=env,
        )

        # 4. Commit tursi's working-tree edits so the collect hook's
        #    `git diff base..HEAD` captures them (tursi never commits itself).
        await self.exec_as_agent(
            environment,
            command=(
                f"cd {REPO_DIR} && git config user.email tursi@bench.local && "
                f"git config user.name tursi && git add -A && "
                f"git commit -m 'tursi solution' --allow-empty || true"
            ),
        )
