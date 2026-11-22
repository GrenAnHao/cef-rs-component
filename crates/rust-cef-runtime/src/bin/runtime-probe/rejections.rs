//! Strict accounting for real metadata refusals; never an error-kind blanket.
use serde_json::{Value, json};

fn number(value: &Value) -> Result<u64, String> {
    value
        .as_u64()
        .ok_or_else(|| "missing/noninteger rejection evidence".into())
}
fn array<const N: usize>(value: &Value) -> Result<[u64; N], String> {
    let values = value
        .as_array()
        .filter(|v| v.len() == N)
        .ok_or("wrong rejection tuple length")?;
    let mut out = [0; N];
    for (target, value) in out.iter_mut().zip(values) {
        *target = number(value)?;
    }
    Ok(out)
}
fn rect(value: &Value) -> Result<[u64; 4], String> {
    let r = array::<4>(value)?;
    if r[2] == 0 || r[3] == 0 {
        return Err("empty rejection rect".into());
    }
    Ok(r)
}
fn inside(inner: [u64; 4], outer: [u64; 4]) -> bool {
    inner[0] >= outer[0]
        && inner[1] >= outer[1]
        && inner[0]
            .checked_add(inner[2])
            .zip(outer[0].checked_add(outer[2]))
            .is_some_and(|(i, o)| i <= o)
        && inner[1]
            .checked_add(inner[3])
            .zip(outer[1].checked_add(outer[3]))
            .is_some_and(|(i, o)| i <= o)
}

pub(super) fn validate(document: &Value, browser: &str, ui: u64) -> Result<Value, String> {
    let capture = &document["native_close"]["capture"];
    let count = number(&capture["failures"])?;
    let probe = &document["native_rejection_observations"];
    let records = probe["native_rejections"]
        .as_array()
        .ok_or("missing native rejection records")?;
    let events = document["incomplete_capture_events"]
        .as_array()
        .ok_or("missing incomplete event records")?;
    let stages = document["stages"]
        .as_array()
        .ok_or("missing completed stages")?;
    if count > 32
        || records.len() as u64 != count
        || events.len() as u64 != count
        || probe["native_rejections_overflow"] != false
    {
        return Err("capture failure/event/witness counts differ or overflow".into());
    }
    if ui == 0
        || number(&capture["owner_thread"])? != ui
        || number(&capture["retirement_thread"])? != ui
    {
        return Err("rejection capture owner/retirement is not actual CEF UI".into());
    }
    if count > 0
        && (capture["last_failure"] != "IncompleteFrame"
            || capture.get("last_failure_code").is_none()
            || !capture["last_failure_code"].is_null())
    {
        return Err("unaccounted capture fault kind/code".into());
    }
    let mut previous_event = 0;
    for (index, (record, event)) in records.iter().zip(events).enumerate() {
        let event_sequence = number(&event["event_sequence"])?;
        if record["browser"] != browser
            || event["browser"] != browser
            || record["surface"] != "View"
            || number(&record["thread"])? != ui
            || number(&record["failure_ordinal"])? != index as u64 + 1
            || event_sequence <= previous_event
            || event.get("code").is_none()
            || !event["code"].is_null()
        {
            return Err("rejection event identity/sequence mismatch".into());
        }
        previous_event = event_sequence;
        let coded = array::<2>(&record["coded"])?;
        let visible = rect(&record["visible"])?;
        let content = rect(&record["content"])?;
        if coded.contains(&0)
            || !inside(visible, [0, 0, coded[0], coded[1]])
            || !inside(content, visible)
            || visible == content
        {
            return Err("not a bounded letterbox metadata refusal".into());
        }
        let generation = number(&record["target_generation"])?;
        let target = array::<2>(&record["target"])?;
        let frame = array::<4>(&record["frame_before"])?;
        let acks = number(&record["acks_before"])?;
        if generation == 0
            || target.contains(&0)
            || frame.contains(&0)
            || frame[0] >= generation
            || record["frame_before"] != record["frame_after"]
            || record["same_frame"] != true
            || number(&record["source_generation_before"])? != generation
            || number(&record["source_generation_after"])? != generation
            || record["recovery_status_before"] != "Pending"
            || record["recovery_status_after"] != "Pending"
            || acks != number(&record["acks_after"])?
        {
            return Err("refused frame altered good frame, target or acknowledgements".into());
        }
        if array::<4>(&record["native_io_before"])? != array::<4>(&record["native_io_after"])? {
            return Err("metadata refusal performed native GPU IO".into());
        }
        let sha = record["frame_sha256"]
            .as_str()
            .filter(|s| s.len() == 64)
            .ok_or("missing retained-frame digest")?;
        let good = stages.iter().any(|stage| {
            stage["generation"].as_u64() == Some(frame[0])
                && stage["sequence"].as_u64() == Some(frame[1])
                && stage["width"].as_u64() == Some(frame[2])
                && stage["height"].as_u64() == Some(frame[3])
                && stage["sha256"] == sha
        });
        let recovered = stages.iter().any(|stage| {
            let recovery = &stage["recovery"];
            stage["generation"].as_u64() == Some(generation)
                && stage["sequence"].as_u64().is_some_and(|seq| seq > frame[1])
                && stage["width"].as_u64() == Some(target[0])
                && stage["height"].as_u64() == Some(target[1])
                && recovery["status"] == "Recovered"
                && recovery["generation"].as_u64() == Some(generation)
                && recovery["expected_width"].as_u64() == Some(target[0])
                && recovery["expected_height"].as_u64() == Some(target[1])
                && acks
                    .checked_add(1)
                    .is_some_and(|ack| recovery["acks"].as_u64() == Some(ack))
        });
        if !good || !recovered {
            return Err(
                "refusal lacks previously verified good pixels or later complete recovery".into(),
            );
        }
    }
    Ok(
        json!({"verified":true,"count":count,"all_events_paired":true,"no_capture_errors_discarded":true}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn witness() -> Value {
        json!({
        "native_close":{"capture":{"failures":1,"last_failure":"IncompleteFrame","last_failure_code":null,"owner_thread":7,"retirement_thread":7}},
        "incomplete_capture_events":[{"browser":"B","event_sequence":8,"code":null}],
        "native_rejection_observations":{"native_rejections_overflow":false,"native_rejections":[{
            "browser":"B","surface":"View","thread":7,"failure_ordinal":1,"coded":[480,360],"visible":[0,0,480,360],"content":[60,0,360,360],
            "source_generation_before":7,"source_generation_after":7,"target_generation":7,"target":[20,20],
            "frame_before":[5,3,480,360],"frame_after":[5,3,480,360],"same_frame":true,"recovery_status_before":"Pending","recovery_status_after":"Pending",
            "acks_before":2,"acks_after":2,"native_io_before":[3,3,3,3],"native_io_after":[3,3,3,3],"frame_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]},
        "stages":[{"generation":5,"sequence":3,"width":480,"height":360,"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
                  {"generation":7,"sequence":4,"width":20,"height":20,"recovery":{"status":"Recovered","generation":7,"expected_width":20,"expected_height":20,"acks":3}}]})
    }
    #[test]
    fn every_rejection_requires_matching_real_event_retained_pixels_and_recovery() {
        let original = witness();
        assert!(validate(&original, "B", 7).is_ok());
        for path in [
            "acks_after",
            "thread",
            "same_frame",
            "frame_sha256",
            "native_io_after",
            "failure_ordinal",
            "content",
        ] {
            let mut bad = original.clone();
            bad["native_rejection_observations"]["native_rejections"][0][path] = Value::Null;
            assert!(validate(&bad, "B", 7).is_err(), "{path}");
        }
        let mut bad = original.clone();
        bad["native_rejection_observations"]["native_rejections"] = json!([]);
        assert!(
            validate(&bad, "B", 7).is_err(),
            "unwitnessed IncompleteFrame is fatal"
        );
        let mut bad = original.clone();
        bad["incomplete_capture_events"][0]["browser"] = json!("foreign");
        assert!(validate(&bad, "B", 7).is_err());
        for (stage, field, replacement) in [
            (0, "sequence", json!(2)),
            (1, "generation", json!(8)),
            (1, "recovery", json!(null)),
        ] {
            let mut bad = original.clone();
            bad["stages"][stage][field] = replacement;
            assert!(validate(&bad, "B", 7).is_err(), "{stage}/{field}");
        }
        for (field, replacement) in [
            ("generation", json!(8)),
            ("expected_width", json!(21)),
            ("expected_height", json!(19)),
            ("acks", json!(2)),
            ("acks", json!(4)),
        ] {
            let mut bad = original.clone();
            bad["stages"][1]["recovery"][field] = replacement;
            assert!(validate(&bad, "B", 7).is_err(), "recovery/{field}");
        }
        let mut bad = original;
        bad["stages"].as_array_mut().unwrap().pop();
        assert!(validate(&bad, "B", 7).is_err());
    }
}
