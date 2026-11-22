#!/usr/bin/env python3
"""Qualify actual Cargo executions separately from the --no-run artifact plan."""
from collections import Counter
import hashlib
import json
from pathlib import Path
import re
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
import poc_paths


START = re.compile(r"^\s*Running\s+.+?\s+\((.+\.exe)\)\s*$")
DOC = re.compile(r"^\s*Doc-tests\s+([A-Za-z_0-9]+)\s*$")
END = re.compile(
    r"^test result: ok\. (\d+) passed; 0 failed; 0 ignored; 0 measured; "
    r"0 filtered out(?:;.*)?$"
)
SELF_HOSTED = "|test|deployment_distribution"
RUNNING_COUNT = re.compile(r"^running (\d+) tests?$")
TEST_PASSED = re.compile(r"^test .+ \.\.\. ok$")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def load(path):
    return json.loads(path.read_text(encoding="utf-8-sig"))


def digest(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def path_key(path, root):
    candidate = Path(path)
    return str((root / candidate).resolve(strict=True)).casefold()


def audit(evidence, root):
    metadata = load(evidence / "metadata.json")
    plan = load(evidence / "harness-plan.json")
    expected = Counter(plan["enabled"])
    libraries = Counter(
        t["name"]
        for p in metadata["packages"]
        if p["id"] in metadata["workspace_members"]
        for t in p["targets"]
        if t["doctest"] and "lib" in t["kind"]
    )
    artifacts = {}
    for artifact in load(evidence / "test-artifacts.json"):
        if artifact["key"] in expected and (
            artifact["profile_test"] or "|test|" in artifact["key"]
        ):
            path = path_key(artifact["executable"], root)
            require(path not in artifacts, f"multiple planned identities for {path}")
            require(digest(Path(artifact["executable"])) == artifact["sha256"].lower(),
                    f"changed test executable: {path}")
            artifacts[path] = artifact["key"]
    require(Counter(artifacts.values()) == expected, "planned executable set changed")

    sections = []
    for line in (evidence / "tests.log").read_text(encoding="utf-8-sig").splitlines():
        line = line.strip()
        start = START.fullmatch(line)
        doc = DOC.fullmatch(line)
        if start:
            path = path_key(start[1], root)
            require(path in artifacts, f"Cargo ran an unplanned executable: {path}")
            sections.append((artifacts[path], "harness", []))
        elif doc:
            sections.append((doc[1], "doctest", []))
        elif sections:
            sections[-1][2].append(line)

    require(Counter(key for key, kind, _ in sections if kind == "harness") == expected,
            "Cargo did not start the exact enabled harness plan")
    require(Counter(key for key, kind, _ in sections if kind == "doctest") == libraries,
            "Cargo did not execute every doctest group")
    executed = []
    for key, kind, lines in sections:
        if key.endswith(SELF_HOSTED):
            markers = [line for line in lines if line.startswith("DEPLOYMENT_INTEGRATION_RESULT=ok evidence=")]
            require(len(markers) == 1, "self-hosted deployment did not emit its real receipt")
            location = Path(markers[0].split("=ok evidence=", 1)[1]).resolve(strict=True)
            require(location.is_relative_to(poc_paths.validation_dir("deployment").resolve()),
                    "deployment receipt left its source workspace")
            for artifact in ("package.log", "validate.log", "ordinary-host-as-helper.log",
                             "bare-helper.log", "missing-dll.log", "validated-manifest.json"):
                require((location / artifact).is_file(), f"missing deployment evidence {artifact}")
            executed.append(dict(key=key, kind="self-hosted", receipt=str(location)))
            continue
        results = [line for line in lines if line.startswith("test result:")]
        result = END.fullmatch(results[0]) if len(results) == 1 else None
        require(result is not None,
                f"missing, skipped, filtered or failed test result for {key}: {results}")
        count = int(result[1])
        announced = [int(match[1]) for line in lines if (match := RUNNING_COUNT.fullmatch(line))]
        require(announced == [count] and sum(bool(TEST_PASSED.fullmatch(line)) for line in lines) == count,
                f"test listing and result count differ for {key}")
        if key.endswith(("|lib|gpui_cef_poc", "|bin|gpui-cef-poc", "|bin|gpui-cef-component-demo")):
            require(count > 0, f"empty root test program: {key}")
        executed.append(dict(key=key, kind=kind, passed=count, failed=0, ignored=0, filtered=0))

    return dict(harnesses=sum(expected.values()), doctest_groups=sum(libraries.values()),
                libtest_passed=sum(r.get("passed", 0) for r in executed),
                self_hosted_passed=sum(r["kind"] == "self-hosted" for r in executed),
                executed=executed, opt_in_not_enabled=plan["opt_in_not_enabled"])


if __name__ == "__main__":
    try:
        evidence, root = [Path(p).resolve(strict=True) for p in sys.argv[1:]]
        print(json.dumps(audit(evidence, root), ensure_ascii=False))
    except (OSError, KeyError, ValueError) as error:
        print(f"Workspace receipt validation: {error}", file=sys.stderr)
        raise SystemExit(1)
