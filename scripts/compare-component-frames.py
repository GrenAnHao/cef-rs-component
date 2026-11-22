#!/usr/bin/env python3
"""Exact cross-backend comparison of task 9.3 Native capture-validation runs.

Source of truth: the four `capture-validation.json` files written by the
Native `Frames` scenario, plus the raw premultiplied BGRA files they reference.
These raw files are actual CEF offscreen (OSR) VIEW callback frames owned by the
Browser frame budget. PNGs produced here are reference images of that real
capture output; they are NOT desktop, Native-window, or GPUI presentation
screenshots. Window-image evidence, if any, is written separately by the
parent's own integration and is not consumed or imitated here.

Rules, fixed in advance and never relaxed on failure:

* exactly four cases: {Integrated, Threaded} x {Software, D3d11};
* every case JSON must report success with complete ordered stage/check lists;
* each stage raw BGRA file must exist and match width*height*4 exactly;
* each stage's legacy 12-byte fingerprint must equal this script's own
  implementation of the preserved algorithm ([0, len//3, len//2, len-4],
  three bytes each) and the run's declared dimensions;
* the same stage under Software and D3d11 (within the same thread mode) must
  match byte-for-byte over the full frame and carry the same fingerprint;
* the parent-attached native_close block must prove capture resources retired,
  no live imports, maps == unmaps, recovery Closed and not queued, and the CPU
  pixel ledger returned to zero;
* native_close.capture.failures is retained as the full production failure
  count (plain non-negative integer) and is never required to be zero;
* every counted Native capture failure is reconciled here, independently of
  any Rust precheck flag: the document must carry exactly one
  IncompleteFrame event and one callback rejection record per failure,
  in the same order; each record must describe a bounded metadata letterbox
  refusal that kept the previously verified good pixels and was followed by a
  complete stage at the requested target; no pixel tolerance is involved.

Any missing case, unreadable artifact, mismatch, incomplete rejection
accounting, or failed post-run closure keeps all evidence and exits non-zero.
There is no skip and no fallback.
"""

import argparse
import hashlib
import json
import os
import re
import sys

from PIL import Image

MODES = ("Integrated", "Threaded")
BACKENDS = ("Software", "D3d11")
EXPECTED_CASES = {(mode, backend) for mode in MODES for backend in BACKENDS}
STAGE_IDS = (
    "initial",
    "resize-352x264",
    "resize-480x360",
    "shrink-20x20",
    "grow-600x420",
    "final-320x240",
    "exhaustion-recovery",
    "incomplete-recovery",
)
CHECK_IDS = (
    "initial_frame_valid_320x240",
    "resize_352x264_recovered",
    "resize_480x360_recovered",
    "shrink_20x20_recovered",
    "grow_600x420_recovered",
    "final_320x240_recovered",
    "exhaustion_attempts_8",
    "exhaustion_request_spacing_ge_100ms",
    "exhaustion_native_invalidate_8",
    "exhaustion_requests_total_plus_8",
    "exhaustion_acks_unchanged",
    "exhaustion_old_good_frame_retained",
    "exhaustion_no_new_generation_frame",
    "hidden_no_requests_250ms",
    "reattach_recovered_new_generation",
    "incomplete_rejected_once",
    "incomplete_old_good_frame_retained",
    "incomplete_target_pending",
    "incomplete_acks_unchanged",
    "incomplete_recovered_new_generation",
    "samples_alpha_non_black_all_stages",
)
EXPECTED_STAGE_DIMS = {
    "initial": (320, 240),
    "resize-352x264": (352, 264),
    "resize-480x360": (480, 360),
    "shrink-20x20": (20, 20),
    "grow-600x420": (600, 420),
    "final-320x240": (320, 240),
    "exhaustion-recovery": (396, 264),
    "incomplete-recovery": (440, 308),
}


class Failure(Exception):
    """Named validation failure; always retained in the evidence output."""


def unpremultiply(value, alpha):
    """Fixed integer inverse of premultiplication; clamped, never guessed."""
    return min(255, (value * 255 + alpha // 2) // alpha)


def premultiplied_bgra_to_straight_rgba(data):
    """Fixed premultiplied BGRA -> straight RGBA rule (documented, exact).

    a == 0 has no meaningful colour and becomes RGB 0. Every other channel
    uses min(255, (v*255 + a//2)//a); alpha is copied unchanged. Channel order
    swaps B/R because CEF supplies BGRA and PNG stores RGBA.
    """
    out = bytearray(len(data))
    for offset in range(0, len(data), 4):
        blue, green, red, alpha = data[offset : offset + 4]
        if alpha == 0:
            straight_red = straight_green = straight_blue = 0
        else:
            straight_red = unpremultiply(red, alpha)
            straight_green = unpremultiply(green, alpha)
            straight_blue = unpremultiply(blue, alpha)
        out[offset : offset + 4] = bytes(
            (straight_red, straight_green, straight_blue, alpha)
        )
    return bytes(out)


def legacy_fingerprint(data):
    """Preserved original algorithm; must not be changed to fit a run."""
    if len(data) < 4:
        return None
    picks = (0, len(data) // 3, len(data) // 2, len(data) - 4)
    sample = bytearray()
    for index in picks:
        base = min(index, len(data) - 4)
        sample.extend(data[base : base + 3])
    return bytes(sample)


def find_case_files(run_dir):
    """Locate every capture-validation.json beneath the run directory."""
    found = []
    for root, dirnames, filenames in os.walk(run_dir):
        dirnames.sort()
        for name in sorted(filenames):
            if name == "capture-validation.json":
                found.append(os.path.join(root, name))
    return found


def load_cases(run_dir):
    """Map (mode, backend) to its validated JSON document and file path."""
    cases = {}
    duplicates = []
    for path in find_case_files(run_dir):
        try:
            with open(path, "r", encoding="utf-8") as handle:
                document = json.load(handle)
        except (OSError, ValueError) as error:
            raise Failure(f"{path}: unreadable capture-validation.json: {error}")
        mode = document.get("mode")
        backend = document.get("backend")
        key = (mode, backend)
        if key not in EXPECTED_CASES:
            raise Failure(f"{path}: unexpected mode/backend {mode!r}/{backend!r}")
        if key in cases:
            duplicates.append(f"{key} in {cases[key]['path']} and {path}")
            continue
        cases[key] = {"path": path, "document": document}
    if duplicates:
        raise Failure("duplicate cases: " + "; ".join(duplicates))
    missing = sorted(EXPECTED_CASES - set(cases))
    if missing:
        expected = ", ".join(f"{mode}-{backend}" for mode, backend in sorted(EXPECTED_CASES))
        got = ", ".join(f"{mode}-{backend}" for mode, backend in sorted(cases)) or "<none>"
        raise Failure(
            f"missing capture-validation.json case(s): {missing}; expected {expected}; found {got}"
        )
    return cases


def resolve_artifact(run_dir, value):
    if not isinstance(value, str) or not value:
        raise Failure("stage raw_bgra path is missing")
    if os.path.isabs(value):
        return value
    return os.path.join(run_dir, value)


def validate_case(case_key, case):
    """Validate one run JSON and every raw artifact it names."""
    mode, backend = case_key
    path = case["path"]
    document = case["document"]
    evidence = {
        "mode": mode,
        "backend": backend,
        "path": path,
        "stages": [],
        "checks": 0,
        "raw_bytes": 0,
        "pngs": [],
    }
    if document.get("schema") != 1 or document.get("kind") != "capture-validation":
        raise Failure(f"{path}: unexpected schema/kind")
    if document.get("success") is not True:
        raise Failure(f"{path}: run did not succeed: {document.get('error')!r}")

    checks = document.get("checks")
    if not isinstance(checks, list) or len(checks) != len(CHECK_IDS):
        raise Failure(f"{path}: expected {len(CHECK_IDS)} ordered checks")
    for index, expected_id in enumerate(CHECK_IDS):
        check = checks[index]
        if check.get("id") != expected_id:
            raise Failure(
                f"{path}: check #{index} is {check.get('id')!r}, expected {expected_id!r}"
            )
        if check.get("passed") is not True:
            raise Failure(f"{path}: check {expected_id} did not pass")
    evidence["checks"] = len(checks)

    stages = document.get("stages")
    if not isinstance(stages, list) or len(stages) != len(STAGE_IDS):
        raise Failure(f"{path}: expected {len(STAGE_IDS)} ordered stages")
    raw_dir = os.path.join(os.path.dirname(path), "reference-png", f"{mode}-{backend}")
    os.makedirs(raw_dir, exist_ok=True)
    for index, expected_id in enumerate(STAGE_IDS):
        stage = stages[index]
        if stage.get("id") != expected_id:
            raise Failure(
                f"{path}: stage #{index} is {stage.get('id')!r}, expected {expected_id!r}"
            )
        width = stage.get("width")
        height = stage.get("height")
        expected_width, expected_height = EXPECTED_STAGE_DIMS[expected_id]
        if (width, height) != (expected_width, expected_height):
            raise Failure(
                f"{path}: stage {expected_id} is {width}x{height}, "
                f"expected {expected_width}x{expected_height}"
            )
        if stage.get("bytes") != width * height * 4:
            raise Failure(f"{path}: stage {expected_id} declared byte length is wrong")
        raw_path = resolve_artifact(os.path.dirname(path), stage.get("raw_bgra"))
        try:
            with open(raw_path, "rb") as handle:
                data = handle.read()
        except OSError as error:
            raise Failure(f"{path}: stage {expected_id} raw BGRA unreadable: {error}")
        if len(data) != width * height * 4:
            raise Failure(
                f"{path}: stage {expected_id} raw file has {len(data)} bytes, "
                f"expected {width * height * 4}"
            )
        fingerprint = legacy_fingerprint(data)
        declared = stage.get("fingerprint")
        if not isinstance(declared, list) or len(declared) != 12:
            raise Failure(f"{path}: stage {expected_id} fingerprint is not 12 bytes")
        if list(fingerprint) != declared:
            raise Failure(
                f"{path}: stage {expected_id} legacy fingerprint mismatch: "
                f"declared {declared}, recomputed {list(fingerprint)}"
            )
        samples = stage.get("samples")
        if not isinstance(samples, list) or not samples:
            raise Failure(f"{path}: stage {expected_id} has no aligned pixel samples")
        if any(sample.get("passed") is not True for sample in samples):
            raise Failure(f"{path}: stage {expected_id} has a failing aligned sample")
        if stage.get("sha256") != hashlib.sha256(data).hexdigest():
            raise Failure(f"{path}: stage {expected_id} sha256 does not match raw BGRA")
        generation = stage.get("generation")
        sequence = stage.get("sequence")
        if type(generation) is not int or generation <= 0:
            raise Failure(f"{path}: stage {expected_id} generation is not a positive integer")
        if type(sequence) is not int or sequence <= 0:
            raise Failure(f"{path}: stage {expected_id} sequence is not a positive integer")
        recovery_block = stage.get("recovery")
        recovery_status = (
            recovery_block.get("status") if isinstance(recovery_block, dict) else None
        )
        if not isinstance(recovery_status, str) or not recovery_status:
            raise Failure(f"{path}: stage {expected_id} recovery status is missing")

        # Exact reference image: CEF supplies PREMULTIPLIED BGRA; PNG stores
        # STRAIGHT RGBA. Convert with the fixed integer rule
        # min(255, (v*255 + a//2)//a) for a!=0 and RGB=0 for a==0, then swap
        # B/R. No resampling, no dithering, no tolerance.
        rgba = premultiplied_bgra_to_straight_rgba(data)
        image = Image.frombytes("RGBA", (width, height), rgba)
        png_path = os.path.join(raw_dir, f"{expected_id}.png")
        image.save(png_path, format="PNG")
        evidence["pngs"].append(png_path)
        evidence["raw_bytes"] += len(data)
        evidence["stages"].append(
            {
                "id": expected_id,
                "width": width,
                "height": height,
                "bytes": len(data),
                "sha256": stage.get("sha256"),
                "generation": generation,
                "sequence": sequence,
                "recovery_status": recovery_status,
                "recovery": recovery_block,
                "fingerprint": declared,
                "raw_bgra": raw_path,
                "png": png_path,
                "bgra": data,
                "rgba": rgba,
            }
        )

    validate_native_close(path, document, evidence)
    evidence["rejection_accounting"] = validate_rejection_accounting(
        path, document, evidence["stages"]
    )
    evidence["success"] = True
    return evidence


def validate_native_close(path, document, evidence):
    """Require the parent's post-shutdown closure block, not just run success."""
    if type(document.get("expected_resize_exhausted_events")) is not int or document["expected_resize_exhausted_events"] != 1:
        raise Failure(f"{path}: exactly one controlled ResizeExhausted event required")
    close = document.get("native_close")
    if not isinstance(close, dict):
        raise Failure(f"{path}: native_close closure block is missing")
    capture = close.get("capture")
    recovery = close.get("recovery")
    if not isinstance(capture, dict) or not isinstance(recovery, dict):
        raise Failure(f"{path}: native_close.capture/recovery missing")
    def nonnegative_integer(value):
        return type(value) is int and value >= 0

    maps = capture.get("maps")
    unmaps = capture.get("unmaps")
    requirements = {
        "capture.retired": capture.get("retired") is True,
        "capture.native_resources_live": capture.get("native_resources_live") is False,
        "capture.abandoned": capture.get("abandoned") is False,
        "capture.live_imports": capture.get("live_imports") == 0,
        "capture.staging_bytes": capture.get("staging_bytes") == 0,
        "capture.maps_equals_unmaps": nonnegative_integer(maps)
        and nonnegative_integer(unmaps) and maps == unmaps,
        "capture.failures_nonnegative": nonnegative_integer(capture.get("failures")),
        "recovery.status": recovery.get("status") == "Closed",
        "recovery.queued": recovery.get("queued") is False,
        "cpu.used_zero": close.get("cpu_used") == 0,
    }
    peak = close.get("cpu_peak")
    limit = close.get("cpu_limit")
    requirements["cpu.peak_within_limit"] = (
        nonnegative_integer(peak) and nonnegative_integer(limit) and 0 <= peak <= limit
    )
    failed = [name for name, passed in requirements.items() if not passed]
    if failed:
        raise Failure(f"{path}: native closure failed: {', '.join(failed)}")
    evidence["native_close"] = {
        "retired": capture.get("retired"),
        "native_resources_live": capture.get("native_resources_live"),
        "live_imports": capture.get("live_imports"),
        "maps": capture.get("maps"),
        "unmaps": capture.get("unmaps"),
        "staging_bytes": capture.get("staging_bytes"),
        "failures": capture.get("failures"),
        "recovery_status": recovery.get("status"),
        "queued": recovery.get("queued"),
        "cpu_used": close.get("cpu_used"),
        "cpu_peak": peak,
        "cpu_limit": limit,
    }


def _is_int(value):
    """JSON integer that is not a Python bool (bool is an int subclass)."""
    return type(value) is int


def _integer(value, label, minimum=None, positive=False):
    if not _is_int(value):
        raise Failure(f"{label} is not an integer: {value!r}")
    if positive and value <= 0:
        raise Failure(f"{label} is not positive: {value!r}")
    if minimum is not None and value < minimum:
        raise Failure(f"{label} is below {minimum}: {value!r}")
    return value


def _tuple(value, length, label, positive=False):
    if not isinstance(value, list) or len(value) != length:
        raise Failure(f"{label} is not a {length}-element integer list: {value!r}")
    for index, item in enumerate(value):
        _integer(item, f"{label}[{index}]", positive=positive)
    return list(value)


def _rect(value, label):
    rect = _tuple(value, 4, label)
    if rect[2] <= 0 or rect[3] <= 0:
        raise Failure(f"{label} is not a positive rectangle: {rect}")
    return rect


def _inside(inner, outer):
    return (
        inner[0] >= outer[0]
        and inner[1] >= outer[1]
        and inner[0] + inner[2] <= outer[0] + outer[2]
        and inner[1] + inner[3] <= outer[1] + outer[3]
    )


SHA256_LENGTH = 64
FRAME_SHA256 = re.compile(r"[0-9a-fA-F]{%d}" % SHA256_LENGTH)
def validate_rejection_accounting(path, document, stages):
    """Reconcile every counted Native capture failure with its own record.

    This is an independent Python recomputation of the fixed refusal protocol,
    never a trust of the Rust `native_rejection_accounting` precheck flag. The
    retained `native_close.capture.failures` count is kept exactly: it must
    equal both the IncompleteFrame event count and the callback rejection
    record count, and every record must describe a bounded metadata letterbox
    refusal that left the previously verified good pixels untouched and was
    later followed by a complete frame at the requested target.
    """
    close = document.get("native_close")
    capture = close.get("capture") if isinstance(close, dict) else None
    if not isinstance(capture, dict):
        raise Failure(f"{path}: native_close.capture is missing")
    count = _integer(capture.get("failures"), f"{path}: capture.failures", minimum=0)
    if count > 32:
        raise Failure(f"{path}: capture.failures {count} exceeds the bounded 32 records")

    probe = document.get("native_rejection_observations")
    if not isinstance(probe, dict):
        raise Failure(f"{path}: native_rejection_observations is missing")
    records = probe.get("native_rejections")
    if not isinstance(records, list):
        raise Failure(f"{path}: native_rejection_observations.native_rejections is missing")
    overflow = probe.get("native_rejections_overflow")
    if overflow is not False:
        raise Failure(f"{path}: native rejection records overflowed: {overflow!r}")

    events = document.get("incomplete_capture_events")
    if not isinstance(events, list):
        raise Failure(f"{path}: incomplete_capture_events is missing")

    if count > 0:
        if capture.get("last_failure") != "IncompleteFrame":
            raise Failure(
                f"{path}: capture.last_failure is {capture.get('last_failure')!r}, "
                "not the accounted IncompleteFrame"
            )
        if "last_failure_code" not in capture or capture.get("last_failure_code") is not None:
            raise Failure(f"{path}: capture.last_failure_code is not an explicit null")

    if len(events) != count or len(records) != count:
        raise Failure(
            f"{path}: failures {count}, events {len(events)}, records {len(records)} "
            "are not exactly reconciled"
        )

    browser_identity = document.get("browser")
    if not isinstance(browser_identity, str) or not browser_identity:
        raise Failure(f"{path}: probe Browser identity missing")
    ui_thread = _integer(document.get("cef_ui_thread_id"), f"{path}: CEF UI thread", positive=True)
    owner = _integer(capture.get("owner_thread"), f"{path}: capture owner", positive=True)
    retired = _integer(capture.get("retirement_thread"), f"{path}: capture retirement thread", positive=True)
    if owner != ui_thread or retired != ui_thread:
        raise Failure(f"{path}: capture owner/retirement is not actual CEF UI")
    previous_event_sequence = 0
    for index, (record, event) in enumerate(zip(records, events), start=1):
        where = f"{path}: rejection #{index}"
        if not isinstance(record, dict) or not isinstance(event, dict):
            raise Failure(f"{where}: event/record is not an object")

        browser = record.get("browser")
        if not isinstance(browser, str) or not browser:
            raise Failure(f"{where}: record browser is missing")
        if browser != browser_identity or event.get("browser") != browser:
            raise Failure(f"{where}: event browser does not match the record browser")

        event_sequence = _integer(
            event.get("event_sequence"), f"{where}: event_sequence", positive=True
        )
        if event_sequence <= previous_event_sequence:
            raise Failure(
                f"{where}: event_sequence {event_sequence} is not strictly after "
                f"{previous_event_sequence}"
            )
        previous_event_sequence = event_sequence
        if "code" not in event or event.get("code") is not None:
            raise Failure(f"{where}: event code is not an explicit null")

        if _integer(record.get("failure_ordinal"), f"{where}: failure_ordinal", positive=True) != index:
            raise Failure(
                f"{where}: failure_ordinal {record.get('failure_ordinal')!r} "
                f"is not the expected {index}"
            )

        if record.get("surface") != "View" or _integer(record.get("thread"), f"{where}: callback thread", positive=True) != ui_thread:
            raise Failure(f"{where}: rejection is not VIEW on actual CEF UI")
        # Natural-letterbox geometry: metadata only, no pixel tolerance.
        coded = _tuple(record.get("coded"), 2, f"{where}: coded", positive=True)
        visible = _rect(record.get("visible"), f"{where}: visible")
        content = _rect(record.get("content"), f"{where}: content")
        if not _inside(visible, [0, 0, coded[0], coded[1]]):
            raise Failure(f"{where}: visible rect is not inside the coded size")
        if not _inside(content, visible):
            raise Failure(f"{where}: content rect is not inside the visible rect")
        if visible == content:
            raise Failure(f"{where}: content equals visible, so no letterbox refusal occurred")

        if record.get("recovery_status_before") != "Pending":
            raise Failure(f"{where}: recovery status before refusal is not Pending")
        if record.get("recovery_status_after") != "Pending":
            raise Failure(f"{where}: recovery status after refusal is not Pending")

        target_generation = _integer(
            record.get("target_generation"), f"{where}: target_generation", positive=True
        )
        target = _tuple(record.get("target"), 2, f"{where}: target", positive=True)
        source_before = _integer(
            record.get("source_generation_before"), f"{where}: source_generation_before"
        )
        source_after = _integer(
            record.get("source_generation_after"), f"{where}: source_generation_after"
        )
        if source_before != target_generation or source_after != target_generation:
            raise Failure(
                f"{where}: source generations {source_before}/{source_after} do not "
                f"equal target generation {target_generation}"
            )

        if record.get("same_frame") is not True:
            raise Failure(f"{where}: refusal did not retain the same frame instance")
        if "frame_before" not in record or "frame_after" not in record:
            raise Failure(f"{where}: frame_before/frame_after evidence is missing")
        if record.get("frame_before") is None or record.get("frame_after") is None:
            raise Failure(f"{where}: retained frame identity is null")
        frame_before = _tuple(record.get("frame_before"), 4, f"{where}: frame_before", positive=True)
        frame_after = _tuple(record.get("frame_after"), 4, f"{where}: frame_after", positive=True)
        if frame_before != frame_after:
            raise Failure(f"{where}: frame identity changed across the refusal")
        frame_generation, frame_sequence, frame_width, frame_height = frame_before
        if frame_generation >= target_generation:
            raise Failure(
                f"{where}: retained frame generation {frame_generation} is not older "
                f"than target generation {target_generation}"
            )

        acks_before = _integer(record.get("acks_before"), f"{where}: acks_before", minimum=0)
        acks_after = _integer(record.get("acks_after"), f"{where}: acks_after", minimum=0)
        if acks_before != acks_after:
            raise Failure(f"{where}: acknowledgements changed across the refusal")

        io_before = _tuple(record.get("native_io_before"), 4, f"{where}: native_io_before")
        io_after = _tuple(record.get("native_io_after"), 4, f"{where}: native_io_after")
        for name, values in (("before", io_before), ("after", io_after)):
            if any(value < 0 for value in values):
                raise Failure(f"{where}: native I/O {name} counters are negative: {values}")
        if io_before != io_after:
            raise Failure(
                f"{where}: metadata refusal performed native I/O: {io_before} -> {io_after}"
            )

        frame_sha256 = record.get("frame_sha256")
        if not isinstance(frame_sha256, str) or not FRAME_SHA256.fullmatch(frame_sha256):
            raise Failure(
                f"{where}: frame_sha256 is not a {SHA256_LENGTH}-character hex digest"
            )

        retained = [
            stage
            for stage in stages
            if stage["generation"] == frame_generation
            and stage["width"] == frame_width
            and stage["height"] == frame_height
            and stage["sequence"] == frame_sequence
            and stage["sha256"] == frame_sha256
        ]
        if not retained:
            raise Failure(
                f"{where}: no earlier verified {frame_width}x{frame_height} stage at "
                f"generation {frame_generation} with sequence {frame_sequence} and "
                "the recorded frame_sha256"
            )

        recovered = False
        for stage in stages:
            if (
                stage["width"] != target[0]
                or stage["height"] != target[1]
                or stage["generation"] != target_generation
                or stage["sequence"] <= frame_sequence
            ):
                continue
            recovery = stage.get("recovery")
            if not isinstance(recovery, dict) or recovery.get("status") != "Recovered":
                continue
            fields = ("generation", "expected_width", "expected_height", "acks")
            actual = tuple(recovery.get(field) for field in fields)
            if all(type(value) is int for value in actual) and actual == (
                target_generation, *target, acks_before + 1
            ):
                recovered = True
                break
        if not recovered:
            raise Failure(
                f"{where}: no later complete {target[0]}x{target[1]} stage at "
                f"generation {target_generation} with ACK {acks_before + 1}"
            )

    return {
        "verified_failures": count,
        "verified_events": len(events),
        "verified_records": len(records),
        "all_events_paired": True,
        "independent_of_rust_precheck": True,
    }


def compare_backends(evidence_by_key):
    """Byte-exact full-frame and fingerprint equality per stage and mode."""
    comparisons = []
    failures = []
    for mode in MODES:
        software = evidence_by_key[(mode, "Software")]
        accelerated = evidence_by_key[(mode, "D3d11")]
        for software_stage, accelerated_stage in zip(software["stages"], accelerated["stages"]):
            stage_id = software_stage["id"]
            bgra_equal = software_stage["bgra"] == accelerated_stage["bgra"]
            rgba_equal = software_stage["rgba"] == accelerated_stage["rgba"]
            fingerprints_match = software_stage["fingerprint"] == accelerated_stage["fingerprint"]
            first_difference = None
            for channel_bytes, channel_name in (
                (software_stage["bgra"], "bgra"),
                (software_stage["rgba"], "rgba"),
            ):
                reference = (
                    accelerated_stage["bgra"]
                    if channel_name == "bgra"
                    else accelerated_stage["rgba"]
                )
                if channel_bytes != reference:
                    for offset, (left, right) in enumerate(zip(channel_bytes, reference)):
                        if left != right:
                            pixel = offset // 4
                            first_difference = {
                                "channel_bytes": channel_name,
                                "byte_offset": offset,
                                "pixel": [
                                    pixel % software_stage["width"],
                                    pixel // software_stage["width"],
                                ],
                                "software": channel_bytes[
                                    pixel * 4 : pixel * 4 + 4
                                ].hex(),
                                "d3d11": reference[pixel * 4 : pixel * 4 + 4].hex(),
                            }
                            break
                    break
            comparisons.append(
                {
                    "mode": mode,
                    "stage": stage_id,
                    "bytes": len(software_stage["bgra"]),
                    "bgra_bytes_equal": bgra_equal,
                    "rgba_bytes_equal": rgba_equal,
                    "fingerprint_equal": fingerprints_match,
                    "first_difference": first_difference,
                    "software_raw": software_stage["raw_bgra"],
                    "d3d11_raw": accelerated_stage["raw_bgra"],
                }
            )
            if not bgra_equal:
                failures.append(f"{mode}/{stage_id}: raw premultiplied BGRA differs")
            if not rgba_equal:
                failures.append(f"{mode}/{stage_id}: exported straight RGBA differs")
            if not fingerprints_match:
                failures.append(f"{mode}/{stage_id}: legacy fingerprint differs")
    return comparisons, failures


def main(argv):
    parser = argparse.ArgumentParser(
        description="Exact Software/D3d11 comparison of Native capture-validation evidence."
    )
    parser.add_argument("--run-dir", required=True)
    options = parser.parse_args(argv)
    run_dir = os.path.abspath(options.run_dir)
    result_path = os.path.join(run_dir, "capture-validation-compare.json")
    summary = {
        "schema": 1,
        "kind": "capture-validation-compare",
        "run_dir": run_dir,
        "source": (
            "actual CEF OSR capture frames (premultiplied BGRA) from each Browser's "
            "Runtime::frame_source; PNGs are reference images of these captures, "
            "not desktop or GPUI window screenshots"
        ),
        "png_conversion": (
            "premultiplied BGRA -> straight RGBA with min(255,(v*255 + a//2)//a) "
            "for alpha!=0 and RGB=0 for alpha==0, then B/R swap; exact, no tolerance"
        ),
        "expected_cases": len(EXPECTED_CASES),
        "compared_cases": 0,
        "stage_comparisons": 0,
        "pngs": 0,
        "raw_bytes": 0,
        "verified_rejections": 0,
        "success": False,
        "error": None,
        "failures": [],
        "cases": [],
        "comparisons": [],
    }
    try:
        if not os.path.isdir(run_dir):
            raise Failure(f"run directory does not exist: {run_dir}")
        cases = load_cases(run_dir)
        evidence_by_key = {}
        for key in sorted(cases):
            try:
                evidence = validate_case(key, cases[key])
            except Failure as error:
                summary["failures"].append(str(error))
                continue
            evidence_by_key[key] = evidence
            summary["cases"].append({name: value for name, value in evidence.items() if name != "stages"})
            summary["cases"][-1]["stages"] = [
                {
                    name: value
                    for name, value in stage.items()
                    if name not in ("bgra", "rgba")
                }
                for stage in evidence["stages"]
            ]
            summary["compared_cases"] += 1
            summary["pngs"] += len(evidence["pngs"])
            summary["raw_bytes"] += evidence["raw_bytes"]
            summary["verified_rejections"] += evidence["rejection_accounting"][
                "verified_failures"
            ]
        if len(evidence_by_key) == len(EXPECTED_CASES) and not summary["failures"]:
            comparisons, failures = compare_backends(evidence_by_key)
            summary["comparisons"] = comparisons
            summary["stage_comparisons"] = len(comparisons)
            summary["failures"].extend(failures)
        elif len(evidence_by_key) == len(EXPECTED_CASES):
            summary["comparisons"] = []
        if summary["failures"]:
            raise Failure("; ".join(summary["failures"]))
        summary["success"] = (
            summary["compared_cases"] == summary["expected_cases"]
            and summary["stage_comparisons"] == len(MODES) * len(STAGE_IDS)
            and summary["pngs"] == len(EXPECTED_CASES) * len(STAGE_IDS)
            and summary["raw_bytes"] > 0
        )
        if not summary["success"]:
            raise Failure("comparison completed without full expected coverage")
    except Failure as error:
        summary["error"] = str(error)
    except Exception as error:  # noqa: BLE001 - retained as a hard failure
        summary["error"] = f"unexpected comparison error: {error!r}"
    finally:
        os.makedirs(run_dir, exist_ok=True)
        with open(result_path, "w", encoding="utf-8") as handle:
            json.dump(summary, handle, indent=2)
    print(json.dumps({"success": summary["success"], "error": summary["error"],
                      "cases": summary["compared_cases"],
                      "verified_rejections": summary["verified_rejections"],
                      "evidence": result_path}))
    if summary["success"]:
        return 0
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
