"""Default output layout for this machine-local gpui-cef-poc checkout."""
from __future__ import annotations

import os
from pathlib import Path

DEFAULT_LAYOUT_ROOT = (
    Path(r"E:\cargo-target\gpui-cef-poc")
    if Path(r"E:\cargo-target").exists()
    else Path(os.path.splitdrive(str(Path(__file__).resolve()))[0] + "\\") / "cargo-target" / "cef-rs-component"
)


def layout_root() -> Path:
    override = os.environ.get("POC_OUTPUT_ROOT", "").strip()
    if override:
        return Path(override).expanduser().resolve()
    return Path(DEFAULT_LAYOUT_ROOT)


def bundle_dir(profile: str = "debug") -> Path:
    if not profile:
        raise ValueError("profile is empty")
    return layout_root() / f"bundle-{profile}"


def validation_dir(stage: str | None = None, token: str | None = None) -> Path:
    path = layout_root() / "component-validation"
    if stage:
        path = path / stage.lower()
    if token:
        path = path / token
    return path


def temp_dir(name: str | None = None) -> Path:
    path = layout_root() / "tmp"
    if name:
        path = path / name
    return path


def cargo_target_dir() -> Path:
    override = os.environ.get("POC_TARGET_DIR", "").strip()
    if override:
        return Path(override).expanduser().resolve()
    return layout_root() / "cargo"
