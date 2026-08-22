#!/usr/bin/env python3

import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest


INSTALL_SCRIPT = Path(__file__).with_name("install.ps1")
SOURCE = INSTALL_SCRIPT.read_text(encoding="utf-8")

POWERSHELL = shutil.which("pwsh") or shutil.which("powershell")
NO_POWERSHELL = "no PowerShell interpreter on PATH"
NOT_POSIX = "the fake binary these tests spawn is a /bin/sh script"

# The ore-specific contract holds for the installer ore publishes, which
# fork/substitute.py renders onto `main`. On `delta` this is still upstream's
# file, so asserting the fork's coordinates there would only assert that the
# rebrand has not run yet.
UPSTREAM_REPO = "openai/codex"
RENDERED = UPSTREAM_REPO not in SOURCE
NOT_RENDERED = "install.ps1 has not been through fork/substitute.py"

# Layout, helper and environment names the rebrand must leave alone: the
# published archives, the daemon and the packaging scripts all key on them.
CONTRACT_NAMES = (
    "CODEX_HOME",
    "CODEX_INSTALL_DIR",
    "CODEX_NON_INTERACTIVE",
    "CODEX_RELEASE",
    "codex-package.json",
    "codex-package_SHA256SUMS",
    "codex-package-$Target.tar.gz",
    "codex-path",
    "codex-resources",
    "codex-code-mode-host.exe",
    "codex-command-runner.exe",
    "codex-windows-sandbox-setup.exe",
)

DRIVER = """\
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($args[0], [ref]$tokens, [ref]$errors)
if ($errors.Count -gt 0) {
    foreach ($parseError in $errors) {
        [Console]::Error.WriteLine($parseError.ToString())
    }
    exit 2
}

if ($args[1]) {
    $definitions = $ast.FindAll(
        { param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] },
        $false)
    foreach ($wanted in $args[1].Split(",")) {
        $definition = $definitions | Where-Object { $_.Name -eq $wanted } | Select-Object -First 1
        if ($null -eq $definition) {
            [Console]::Error.WriteLine("install.ps1 no longer defines $wanted")
            exit 3
        }
        # The script body below the function definitions refuses to run off
        # Windows, so the functions are lifted out and called on their own.
        . ([scriptblock]::Create($definition.Extent.Text))
    }
}
"""


class InstallPs1Test(unittest.TestCase):
    @unittest.skipUnless(POWERSHELL, NO_POWERSHELL)
    def test_install_ps1_is_syntactically_valid(self) -> None:
        result = run_driver([], "")

        self.assertEqual(result.returncode, 0, result.stderr)

    def test_every_declared_prefix_matches_the_length_it_strips(self) -> None:
        strips = version_prefix_strips()

        self.assertTrue(strips, "Normalize-Version no longer strips a prefix")
        for prefix, length in strips:
            with self.subTest(prefix=prefix):
                self.assertEqual(len(prefix), length)

    @unittest.skipUnless(POWERSHELL, NO_POWERSHELL)
    def test_normalize_version_strips_the_release_tag_prefix(self) -> None:
        prefix = release_tag_prefix()

        result = run_driver(
            ["Normalize-Version"],
            "Write-Output (Normalize-Version -RawVersion $args[2])",
            [f"{prefix}1.149.0"],
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "1.149.0")

    @unittest.skipUnless(POWERSHELL, NO_POWERSHELL)
    def test_normalize_version_handles_plain_and_v_prefixed_input(self) -> None:
        cases = {
            "v1.149.0": "1.149.0",
            "1.149.0": "1.149.0",
            "latest": "latest",
            "": "latest",
        }

        for raw, expected in cases.items():
            with self.subTest(raw=raw):
                result = run_driver(
                    ["Normalize-Version"],
                    "Write-Output (Normalize-Version -RawVersion $args[2])",
                    [raw],
                )

                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), expected)

    @unittest.skipUnless(POWERSHELL, NO_POWERSHELL)
    def test_release_version_grammar(self) -> None:
        prefix = release_tag_prefix()
        accepted = [
            "latest",
            "1.149.0",
            "1.149.0-alpha",
            "1.149.0-alpha.23",
            "0.145.0-alpha.23.1",
            "1.149.0-beta",
            "1.149.0-beta.4",
        ]
        rejected = [
            "",
            "1.149",
            "1.149.0.1",
            "1.149.0-rc.1",
            "1.149.0-alpha.1.2.3",
            "LATEST",
            f"{prefix}1.149.0",
        ]

        result = run_driver(
            ["Assert-ValidReleaseVersion"],
            """\
            foreach ($candidate in $args[2].Split("|")) {
                try {
                    Assert-ValidReleaseVersion -Version $candidate
                    Write-Output "accept"
                } catch {
                    Write-Output "reject"
                }
            }
            """,
            ["|".join(accepted + rejected)],
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.split(),
            ["accept"] * len(accepted) + ["reject"] * len(rejected),
        )

    @unittest.skipUnless(POWERSHELL, NO_POWERSHELL)
    @unittest.skipUnless(os.name == "posix", NOT_POSIX)
    def test_version_from_binary_reads_a_one_line_version(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            binary = fake_binary(Path(temp_dir), "codex-cli 0.149.0\\n")

            result = run_driver(
                ["Get-VersionFromBinary"],
                "Write-Output (Get-VersionFromBinary -CodexPath $args[2])",
                [str(binary)],
            )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "0.149.0")

    @unittest.skipUnless(POWERSHELL, NO_POWERSHELL)
    @unittest.skipUnless(os.name == "posix", NOT_POSIX)
    @unittest.skipUnless(RENDERED, NOT_RENDERED)
    def test_version_from_binary_reads_the_first_of_two_lines(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            binary = fake_binary(
                Path(temp_dir),
                "ore 1.149.0\\ncodex-base: ore-v0.149.0 (758ef40f50)\\n",
            )

            result = run_driver(
                ["Get-VersionFromBinary"],
                "Write-Output (Get-VersionFromBinary -CodexPath $args[2])",
                [str(binary)],
            )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "1.149.0")

    @unittest.skipUnless(POWERSHELL, NO_POWERSHELL)
    def test_version_from_binary_reports_nothing_for_a_missing_binary(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            result = run_driver(
                ["Get-VersionFromBinary"],
                "Write-Output (Get-VersionFromBinary -CodexPath $args[2])",
                [str(Path(temp_dir) / "absent.exe")],
            )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "")

    def test_layout_and_environment_contracts_survive_the_rebrand(self) -> None:
        for name in CONTRACT_NAMES:
            with self.subTest(name=name):
                self.assertIn(name, SOURCE)

    @unittest.skipUnless(RENDERED, NOT_RENDERED)
    def test_release_coordinates_point_at_the_fork(self) -> None:
        self.assertNotIn(UPSTREAM_REPO, SOURCE)
        self.assertNotIn("ore-v", SOURCE)

    @unittest.skipUnless(RENDERED, NOT_RENDERED)
    def test_entrypoint_is_the_fork_binary(self) -> None:
        self.assertIn(r'"bin\ore.exe"', SOURCE)
        self.assertIn(r'Join-Path $VisibleBinDir "ore.exe"', SOURCE)
        self.assertNotIn(r'"bin\codex.exe"', SOURCE)

    @unittest.skipUnless(RENDERED, NOT_RENDERED)
    def test_visible_install_directory_is_not_vendored_under_openai(self) -> None:
        self.assertIn(r'"Programs\ore\bin"', SOURCE)
        self.assertNotIn(r"Programs\OpenAI", SOURCE)

    @unittest.skipUnless(RENDERED, NOT_RENDERED)
    def test_github_releases_are_the_default_source(self) -> None:
        self.assertIn("$DefaultPreferReleasesOpenAICom = $false", SOURCE)
        self.assertIn('$ReleasesBaseUri = ""', SOURCE)


def run_driver(
    functions: list[str],
    body: str,
    args: list[str] | None = None,
) -> subprocess.CompletedProcess[str]:
    """Parse install.ps1, dot-source `functions` out of its AST, then run `body`."""
    script = DRIVER + "\n" + body
    with tempfile.TemporaryDirectory() as temp_dir:
        driver = Path(temp_dir) / "driver.ps1"
        driver.write_text(script, encoding="utf-8")
        return subprocess.run(
            [
                POWERSHELL,
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-File",
                str(driver),
                str(INSTALL_SCRIPT),
                ",".join(functions),
                *(args or []),
            ],
            capture_output=True,
            check=False,
            text=True,
        )


def version_prefix_strips() -> list[tuple[str, int]]:
    return [
        (match.group(1), int(match.group(2)))
        for match in re.finditer(
            r'StartsWith\("([^"]+)"\)\)\s*\{\s*return \$RawVersion\.Substring\((\d+)\)',
            SOURCE,
        )
    ]


def release_tag_prefix() -> str:
    return max((prefix for prefix, _ in version_prefix_strips()), key=len)


def fake_binary(directory: Path, version_output: str) -> Path:
    path = directory / "fake-cli"
    path.write_text(f"#!/bin/sh\nprintf '{version_output}'\n", encoding="utf-8")
    path.chmod(0o755)
    return path


if __name__ == "__main__":
    unittest.main()
