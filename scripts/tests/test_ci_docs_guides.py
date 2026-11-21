"""Lint the new guides: every command, flag and config key they name must exist.

Backticks in these guides always name something mini-agent has. Names from other tools are
written in plain text, so this check can be strict.
"""

import json
import re
import unittest
from pathlib import Path

from scripts import render_package_manifests  # noqa: F401  (imported to keep the package importable)
from scripts.ci import acp_smoke

REPOSITORY = Path(__file__).resolve().parents[2]
DOCS = REPOSITORY / "docs/agent"
GUIDES = ["MIGRATING.md", "TROUBLESHOOTING.md", "CI.md", "EDITORS.md"]


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def known_flags() -> set[str]:
    cli = read(REPOSITORY / "src/cli.rs")
    flags = set(re.findall(r'(?:long|alias|visible_alias) = "([a-z0-9-]+)"', cli))
    flags |= {name.replace("_", "-") for name in re.findall(r"pub(?:\(crate\))? ([a-z0-9_]+):", cli)}
    flags |= set(re.findall(r"--([A-Za-z][A-Za-z0-9-]+)", read(DOCS / "COMMANDS.md") + read(DOCS / "GET_STARTED.md")))
    # Flags of install.sh and of subcommands (`trust`, `hooks trust`, `update`) are not in the top-level Cli.
    flags |= set(re.findall(r"--([a-z][a-z0-9-]+)", read(REPOSITORY / "install.sh")))
    flags |= {"yes", "list", "revoke", "check", "release"}
    return flags


# Names that belong to GitHub, editors or the operating system, not to mini-agent.
FOREIGN_NAMES = {"pull_request_target", "issue_comment", "author_association", "agent_servers"}
SYSTEM_DIRECTORIES = {"var", "tmp", "etc", "usr", "dev", "opt", "bin", "home"}


def known_commands() -> set[str]:
    text = read(DOCS / "COMMANDS.md") + read(REPOSITORY / "src/ui/slash/help.rs")
    return set(re.findall(r"/([a-z][a-z-]*)", text))


def corpus(exclude: str) -> str:
    parts = [read(p) for p in DOCS.rglob("*.md") if p.name != exclude]
    parts += [read(REPOSITORY / "src/config/keys.rs"), read(REPOSITORY / "src/cli.rs"), read(REPOSITORY / "action.yml")]
    return "\n".join(parts)


def inline_tokens(text: str) -> list[str]:
    text = re.sub(r"(?s)```.*?```", "", text)
    return re.findall(r"`([^`\n]+)`", text)


class GuideReferenceTests(unittest.TestCase):
    def test_flags_commands_and_keys_exist(self) -> None:
        flags, commands = known_flags(), known_commands()
        problems = []
        for guide in GUIDES:
            text = read(DOCS / guide)
            body = text.split("---", 2)[2]
            if guide == "MIGRATING.md":
                # The first column of a migration table names the other tool's feature.
                body = re.sub(r"(?m)^\| [^|]*\| ", "| ", body)
            rest = corpus(guide)
            for token in inline_tokens(body):
                for flag in re.findall(r"(?<![\w-])--([A-Za-z][A-Za-z0-9-]*)", token):
                    if flag not in flags:
                        problems.append(f"{guide}: unknown flag --{flag}")
                match = re.match(r"^/([a-z][a-z-]*)(?: |$)", token)
                if match and match.group(1) not in commands | SYSTEM_DIRECTORIES:
                    problems.append(f"{guide}: unknown slash command /{match.group(1)}")
                if re.fullmatch(r"[a-z][a-z0-9]*(?:[_-][a-z0-9]+)+", token) and token not in rest and token not in FOREIGN_NAMES:
                    problems.append(f"{guide}: `{token}` is not a config key, flag or documented name")
        self.assertEqual([], problems)

    def test_known_flags_include_documented_ones(self) -> None:
        for flag in ("print", "output", "permission-mode", "max-budget-usd", "max-agent-turns", "doctor", "acp"):
            self.assertIn(flag, known_flags())


class ActionDocsTests(unittest.TestCase):
    def test_ci_guide_embeds_the_example_workflows_verbatim(self) -> None:
        guide = read(DOCS / "CI.md")
        for name in ("mini-agent-pr-review.yml", "mini-agent-issue-comment.yml"):
            example = read(REPOSITORY / "examples/ci" / name).rstrip("\n")
            self.assertIn(example, guide, name)

    def test_every_action_input_and_output_is_documented(self) -> None:
        action = read(REPOSITORY / "action.yml")
        guide = read(DOCS / "CI.md")
        inputs = re.search(r"(?ms)^inputs:\n(.*?)^outputs:", action).group(1)
        outputs = re.search(r"(?ms)^outputs:\n(.*?)^runs:", action).group(1)
        for name in re.findall(r"(?m)^  ([a-z-]+):\n", inputs) + re.findall(r"(?m)^  ([a-z-]+):\n", outputs):
            self.assertIn(f"`{name}`", guide, name)

    def test_example_workflows_never_interpolate_untrusted_text_into_run(self) -> None:
        for path in (REPOSITORY / "examples/ci").glob("*.yml"):
            text = read(path)
            for block in re.findall(r"(?ms)^\s+run: \|\n(.*?)(?=^\s+- |\Z)", text):
                self.assertNotIn("${{", block, f"{path.name}: expression inside a run script")


class EditorDocsTests(unittest.TestCase):
    def test_each_client_argv_is_shown_in_the_editors_guide(self) -> None:
        guide = read(DOCS / "EDITORS.md")
        for client, argv in acp_smoke.ARGV_BY_CLIENT.items():
            self.assertIn(json.dumps(argv), guide, client)
            self.assertEqual(["--acp"], argv)

    def test_every_guide_is_in_the_site_manifest(self) -> None:
        manifest = read(DOCS / "manifest.txt")
        for guide in GUIDES + ["PARITY.md"]:
            self.assertIn(f"| {guide}", manifest)


if __name__ == "__main__":
    unittest.main()
